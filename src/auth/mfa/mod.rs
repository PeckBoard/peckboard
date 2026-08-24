//! Pluggable MFA methods for Peckboard login.
//!
//! v1 ships TOTP + recovery codes. New methods implement [`MfaMethod`]
//! and register in [`MfaRegistry::builtin`]. Login never matches on
//! method strings itself.

use async_trait::async_trait;

use crate::auth::password::verify_password;
use crate::auth::session::issue_session_token;
use crate::db::Db;
use crate::db::models::{NewMfaMethod, NewMfaPending, NewMfaRecoveryCode, User};

mod challenge;
mod recovery;
mod totp;
pub mod vault;

pub use challenge::{IssuedChallenge, LiveChallenge};
pub use totp::EnrollMaterial;

const KIND_TOTP: &str = totp::KIND;
const PENDING_TTL_SECS: i64 = 10 * 60;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[derive(Debug)]
pub enum MfaError {
    InvalidPassword,
    InvalidCode,
    AlreadyEnabled,
    NotEnabled,
    UnknownMethod,
    ChallengeUnknown,
    ChallengeExpired,
    ChallengeConsumed,
    PendingExpired,
    PendingMissing,
    LockedOut,
    Internal(anyhow::Error),
}

impl std::fmt::Display for MfaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPassword => write!(f, "invalid credentials"),
            Self::InvalidCode => write!(f, "invalid code"),
            Self::AlreadyEnabled => write!(f, "two-factor authentication is already enabled"),
            Self::NotEnabled => write!(f, "two-factor authentication is not enabled"),
            Self::UnknownMethod => write!(f, "unknown MFA method"),
            Self::ChallengeUnknown | Self::ChallengeExpired | Self::ChallengeConsumed => {
                write!(f, "MFA challenge expired, sign in again")
            }
            Self::PendingExpired | Self::PendingMissing => {
                write!(f, "setup expired, start again")
            }
            Self::LockedOut => write!(f, "too many attempts, sign in again"),
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl MfaError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::InvalidPassword | Self::InvalidCode | Self::LockedOut => StatusCode::UNAUTHORIZED,
            Self::AlreadyEnabled => StatusCode::CONFLICT,
            Self::NotEnabled
            | Self::UnknownMethod
            | Self::PendingExpired
            | Self::PendingMissing => StatusCode::BAD_REQUEST,
            Self::ChallengeUnknown | Self::ChallengeExpired | Self::ChallengeConsumed => {
                StatusCode::UNAUTHORIZED
            }
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Status for routes that already require a valid JWT. Wrong password or
    /// TOTP must not be 401 — the frontend treats 401 as a dead session.
    pub fn authed_status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::InvalidPassword | Self::InvalidCode => StatusCode::FORBIDDEN,
            other => other.status(),
        }
    }
}

/// Proof token: bearer has verified this user's password.
/// See [`crate::routes::auth`] login for an example.
#[derive(Debug)]
pub struct PasswordVerified {
    user_id: String,
    role: String,
    username: String,
    mfa_enabled: bool,
}

impl PasswordVerified {
    /// Only call after `verify_password` returned true for `user`.
    pub fn after_password_ok(user: &User, mfa_enabled: bool) -> Self {
        Self {
            user_id: user.id.clone(),
            role: user.role.clone(),
            username: user.username.clone(),
            mfa_enabled,
        }
    }

    pub fn mfa_enabled(&self) -> bool {
        self.mfa_enabled
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Password is enough only when MFA is not enrolled.
    pub fn session_grant(self) -> Result<SessionGrant, Self> {
        if self.mfa_enabled {
            Err(self)
        } else {
            Ok(SessionGrant {
                user_id: self.user_id,
                role: self.role,
                username: self.username,
            })
        }
    }
}

/// Proof token: every required login factor succeeded.
/// Only constructors: [`PasswordVerified::session_grant`] (no MFA) and
/// [`MfaVerified::session_grant`].
pub struct SessionGrant {
    user_id: String,
    role: String,
    username: String,
}

impl SessionGrant {
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
    pub fn role(&self) -> &str {
        &self.role
    }
    pub fn username(&self) -> &str {
        &self.username
    }
}

/// Proof token: an MFA method accepted a live challenge.
#[derive(Debug)]
pub struct MfaVerified {
    user_id: String,
    role: String,
    username: String,
}

impl MfaVerified {
    pub fn session_grant(self) -> SessionGrant {
        SessionGrant {
            user_id: self.user_id,
            role: self.role,
            username: self.username,
        }
    }
}

pub struct VerifyCtx<'a> {
    pub db: &'a Db,
    pub vault_key: &'a [u8],
    pub user_id: &'a str,
}

#[async_trait]
pub trait MfaMethod: Send + Sync {
    fn kind(&self) -> &'static str;
    async fn verify(&self, ctx: &VerifyCtx<'_>, code: &str) -> Result<(), MfaError>;
}

pub struct MfaRegistry {
    methods: Vec<Box<dyn MfaMethod>>,
}

impl MfaRegistry {
    pub fn builtin() -> Self {
        Self {
            methods: vec![Box::new(TotpMethod), Box::new(RecoveryMethod)],
        }
    }

    pub fn get(&self, kind: &str) -> Option<&dyn MfaMethod> {
        self.methods
            .iter()
            .find(|m| m.kind() == kind)
            .map(|b| &**b as &dyn MfaMethod)
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        self.methods.iter().map(|m| m.kind()).collect()
    }
}

struct TotpMethod;
struct RecoveryMethod;

#[async_trait]
impl MfaMethod for TotpMethod {
    fn kind(&self) -> &'static str {
        totp::KIND
    }

    async fn verify(&self, ctx: &VerifyCtx<'_>, code: &str) -> Result<(), MfaError> {
        let row = ctx
            .db
            .get_mfa_method(ctx.user_id, totp::KIND)
            .await
            .map_err(MfaError::Internal)?
            .ok_or(MfaError::NotEnabled)?;
        let (Some(ct), Some(nonce)) = (row.secret_ct.as_deref(), row.secret_nonce.as_deref())
        else {
            return Err(MfaError::Internal(anyhow::anyhow!("totp secret missing")));
        };
        let secret_b32 = vault::decrypt(ctx.vault_key, nonce, ct).map_err(MfaError::Internal)?;
        let now = now_unix() as u64;
        let ts = totp::verify(&secret_b32, code, row.last_timestep, now)?;
        ctx.db
            .set_mfa_last_timestep(&row.id, ts)
            .await
            .map_err(MfaError::Internal)?;
        Ok(())
    }
}

#[async_trait]
impl MfaMethod for RecoveryMethod {
    fn kind(&self) -> &'static str {
        recovery::KIND
    }

    async fn verify(&self, ctx: &VerifyCtx<'_>, code: &str) -> Result<(), MfaError> {
        let unused = ctx
            .db
            .list_unused_recovery_codes(ctx.user_id)
            .await
            .map_err(MfaError::Internal)?;
        if unused.is_empty() {
            return Err(MfaError::InvalidCode);
        }
        let pairs: Vec<(String, String)> =
            unused.into_iter().map(|r| (r.id, r.code_hash)).collect();
        let id = recovery::find_match(code, &pairs)?;
        ctx.db
            .mark_recovery_code_used(id, chrono::Utc::now().to_rfc3339())
            .await
            .map_err(MfaError::Internal)?;
        Ok(())
    }
}

pub async fn available_methods(db: &Db, user_id: &str) -> Result<Vec<String>, MfaError> {
    let mut out = Vec::new();
    if db
        .get_mfa_method(user_id, totp::KIND)
        .await
        .map_err(MfaError::Internal)?
        .is_some()
    {
        out.push(totp::KIND.to_string());
    }
    let unused = db
        .list_unused_recovery_codes(user_id)
        .await
        .map_err(MfaError::Internal)?;
    if !unused.is_empty() {
        out.push(recovery::KIND.to_string());
    }
    Ok(out)
}

pub async fn status(db: &Db, user_id: &str) -> Result<(bool, Vec<serde_json::Value>), MfaError> {
    let methods = db
        .list_mfa_methods(user_id)
        .await
        .map_err(MfaError::Internal)?;
    let enabled = !methods.is_empty();
    let json: Vec<serde_json::Value> = methods
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "kind": m.kind,
                "created_at": m.created_at,
            })
        })
        .collect();
    Ok((enabled, json))
}

fn require_password(user: &User, password: &str) -> Result<(), MfaError> {
    if verify_password(password, &user.password_hash) {
        Ok(())
    } else {
        Err(MfaError::InvalidPassword)
    }
}

/// Start TOTP enrollment. Stores the secret pending a confirming code.
pub async fn begin_totp(
    db: &Db,
    vault_key: &[u8],
    user: &User,
    password: &str,
) -> Result<EnrollMaterial, MfaError> {
    require_password(user, password)?;
    if db
        .user_has_mfa(&user.id)
        .await
        .map_err(MfaError::Internal)?
    {
        return Err(MfaError::AlreadyEnabled);
    }
    let material = totp::enroll_material(&user.username)?;
    let (ct, nonce) =
        vault::encrypt(vault_key, &material.secret_b32).map_err(MfaError::Internal)?;
    let now = now_unix();
    db.replace_mfa_pending(NewMfaPending {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        kind: KIND_TOTP.to_string(),
        secret_ct: ct,
        secret_nonce: nonce,
        created_at: now,
        expires_at: now + PENDING_TTL_SECS,
    })
    .await
    .map_err(MfaError::Internal)?;
    Ok(material)
}

/// Confirm TOTP with a live code; enables MFA and returns recovery codes once.
pub async fn confirm_totp(
    db: &Db,
    vault_key: &[u8],
    user: &User,
    password: &str,
    code: &str,
) -> Result<Vec<String>, MfaError> {
    require_password(user, password)?;
    if db
        .user_has_mfa(&user.id)
        .await
        .map_err(MfaError::Internal)?
    {
        return Err(MfaError::AlreadyEnabled);
    }
    let pending = db
        .get_mfa_pending(&user.id)
        .await
        .map_err(MfaError::Internal)?
        .ok_or(MfaError::PendingMissing)?;
    if pending.kind != KIND_TOTP {
        return Err(MfaError::PendingMissing);
    }
    if pending.expires_at <= now_unix() {
        let _ = db.delete_mfa_pending(&user.id).await;
        return Err(MfaError::PendingExpired);
    }
    let secret_b32 = vault::decrypt(vault_key, &pending.secret_nonce, &pending.secret_ct)
        .map_err(MfaError::Internal)?;
    totp::verify(&secret_b32, code, None, now_unix() as u64)?;
    let (ct, nonce) = vault::encrypt(vault_key, &secret_b32).map_err(MfaError::Internal)?;
    // Enrollment proves possession; it is not a login. Do not consume the
    // current TOTP window so the user can sign in (or disable) immediately.
    db.insert_mfa_method(NewMfaMethod {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        kind: KIND_TOTP.to_string(),
        label: Some("Authenticator app".into()),
        secret_ct: Some(ct),
        secret_nonce: Some(nonce),
        last_timestep: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    })
    .await
    .map_err(MfaError::Internal)?;
    let plains = recovery::generate_plain();
    let mut rows = Vec::with_capacity(plains.len());
    for p in &plains {
        rows.push(NewMfaRecoveryCode {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user.id.clone(),
            code_hash: recovery::hash_plain(p)?,
            used_at: None,
        });
    }
    db.insert_recovery_codes(rows)
        .await
        .map_err(MfaError::Internal)?;
    let _ = db.delete_mfa_pending(&user.id).await;
    Ok(plains)
}

async fn verify_code_for_user(
    db: &Db,
    vault_key: &[u8],
    user_id: &str,
    method: &str,
    code: &str,
) -> Result<(), MfaError> {
    let registry = MfaRegistry::builtin();
    let m = registry.get(method).ok_or(MfaError::UnknownMethod)?;
    m.verify(
        &VerifyCtx {
            db,
            vault_key,
            user_id,
        },
        code,
    )
    .await
}

/// Disable MFA. Requires password **and** a valid TOTP/recovery code so a
/// stolen session JWT cannot turn it off.
pub async fn disable(
    db: &Db,
    vault_key: &[u8],
    user: &User,
    password: &str,
    method: &str,
    code: &str,
) -> Result<(), MfaError> {
    require_password(user, password)?;
    if !db
        .user_has_mfa(&user.id)
        .await
        .map_err(MfaError::Internal)?
    {
        return Err(MfaError::NotEnabled);
    }
    verify_code_for_user(db, vault_key, &user.id, method, code).await?;
    db.wipe_user_mfa(&user.id)
        .await
        .map_err(MfaError::Internal)?;
    Ok(())
}

/// Replace recovery codes. Requires password + a live TOTP/recovery code.
pub async fn regenerate_recovery(
    db: &Db,
    vault_key: &[u8],
    user: &User,
    password: &str,
    method: &str,
    code: &str,
) -> Result<Vec<String>, MfaError> {
    require_password(user, password)?;
    if !db
        .user_has_mfa(&user.id)
        .await
        .map_err(MfaError::Internal)?
    {
        return Err(MfaError::NotEnabled);
    }
    verify_code_for_user(db, vault_key, &user.id, method, code).await?;
    db.delete_recovery_codes_for_user(&user.id)
        .await
        .map_err(MfaError::Internal)?;
    let plains = recovery::generate_plain();
    let mut rows = Vec::with_capacity(plains.len());
    for p in &plains {
        rows.push(NewMfaRecoveryCode {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user.id.clone(),
            code_hash: recovery::hash_plain(p)?,
            used_at: None,
        });
    }
    db.insert_recovery_codes(rows)
        .await
        .map_err(MfaError::Internal)?;
    Ok(plains)
}

pub async fn issue_challenge(
    db: &Db,
    proof: &PasswordVerified,
) -> Result<IssuedChallenge, MfaError> {
    challenge::issue(db, proof).await
}

/// Complete the MFA hop. Consumes the challenge on success.
pub async fn verify_challenge(
    db: &Db,
    vault_key: &[u8],
    raw_token: &str,
    method: &str,
    code: &str,
) -> Result<MfaVerified, MfaError> {
    let live = challenge::load_live(db, raw_token).await?;
    match verify_code_for_user(db, vault_key, live.user_id(), method, code).await {
        Ok(()) => {
            challenge::consume(db, &live).await?;
            let user = db
                .get_user(live.user_id())
                .await
                .map_err(MfaError::Internal)?
                .ok_or_else(|| MfaError::Internal(anyhow::anyhow!("user missing")))?;
            Ok(MfaVerified {
                user_id: user.id,
                role: user.role,
                username: user.username,
            })
        }
        Err(e @ MfaError::InvalidCode) => {
            let _ = challenge::record_failure(db, &live).await;
            Err(e)
        }
        Err(e) => Err(e),
    }
}

pub async fn complete_login(
    db: &Db,
    jwt_secret: &[u8],
    grant: &SessionGrant,
    user_agent: Option<String>,
    ip_address: Option<String>,
) -> anyhow::Result<String> {
    issue_session_token(
        db,
        jwt_secret,
        grant.user_id(),
        grant.role(),
        user_agent,
        ip_address,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password::hash_password;
    use crate::auth::token::{generate_jwt_secret, validate_token};
    use crate::db::models::NewUser;

    async fn seed_user(db: &Db, username: &str, password: &str) -> User {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_user(NewUser {
            id: uuid::Uuid::new_v4().to_string(),
            username: username.into(),
            email: None,
            password_hash: hash_password(password).unwrap(),
            role: "user".into(),
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .unwrap()
    }

    fn vault() -> Vec<u8> {
        vec![7u8; 32]
    }

    #[tokio::test]
    async fn totp_enroll_then_login_challenge_then_verify() {
        let db = Db::in_memory().unwrap();
        let vault_key = vault();
        let user = seed_user(&db, "alice", "twelve-chars!!").await;

        let mat = begin_totp(&db, &vault_key, &user, "twelve-chars!!")
            .await
            .unwrap();
        let now = now_unix() as u64;
        let code = totp::generate_now(&mat.secret_b32, now).unwrap();
        let recovery_codes = confirm_totp(&db, &vault_key, &user, "twelve-chars!!", &code)
            .await
            .unwrap();
        assert_eq!(recovery_codes.len(), 10);
        assert!(db.user_has_mfa(&user.id).await.unwrap());

        let proof = PasswordVerified::after_password_ok(&user, true);
        assert!(proof.session_grant().is_err());
        let proof = PasswordVerified::after_password_ok(&user, true);
        let issued = issue_challenge(&db, &proof).await.unwrap();
        assert!(
            db.list_auth_sessions_by_user(&user.id)
                .await
                .unwrap()
                .is_empty(),
        );
        let same_window = totp::generate_now(&mat.secret_b32, now).unwrap();
        let verified = verify_challenge(&db, &vault_key, &issued.raw_token, "totp", &same_window)
            .await
            .unwrap();
        let grant = verified.session_grant();
        let secret = generate_jwt_secret();
        let token = complete_login(&db, &secret, &grant, None, None)
            .await
            .unwrap();
        let claims = validate_token(&secret, &token).unwrap();
        assert_eq!(claims.sub, user.id);
        assert!(validate_token(&secret, &issued.raw_token).is_err());
    }

    #[tokio::test]
    async fn recovery_code_works_once() {
        let db = Db::in_memory().unwrap();
        let vault_key = vault();
        let user = seed_user(&db, "bob", "twelve-chars!!").await;
        let mat = begin_totp(&db, &vault_key, &user, "twelve-chars!!")
            .await
            .unwrap();
        let code = totp::generate_now(&mat.secret_b32, now_unix() as u64).unwrap();
        let plains = confirm_totp(&db, &vault_key, &user, "twelve-chars!!", &code)
            .await
            .unwrap();

        let proof = PasswordVerified::after_password_ok(&user, true);
        let issued = issue_challenge(&db, &proof).await.unwrap();
        verify_challenge(&db, &vault_key, &issued.raw_token, "recovery", &plains[0])
            .await
            .unwrap();

        let proof = PasswordVerified::after_password_ok(&user, true);
        let issued = issue_challenge(&db, &proof).await.unwrap();
        let err = verify_challenge(&db, &vault_key, &issued.raw_token, "recovery", &plains[0])
            .await
            .unwrap_err();
        assert!(matches!(err, MfaError::InvalidCode));
    }

    #[tokio::test]
    async fn password_only_grant_when_mfa_off() {
        let db = Db::in_memory().unwrap();
        let user = seed_user(&db, "cara", "twelve-chars!!").await;
        let proof = PasswordVerified::after_password_ok(&user, false);
        let grant = proof.session_grant().expect("no mfa");
        let secret = generate_jwt_secret();
        let token = complete_login(&db, &secret, &grant, None, None)
            .await
            .unwrap();
        assert!(validate_token(&secret, &token).is_ok());
    }

    #[tokio::test]
    async fn wipe_clears_mfa_so_password_only_login_works() {
        let db = Db::in_memory().unwrap();
        let vault_key = vault();
        let user = seed_user(&db, "dana", "twelve-chars!!").await;
        let mat = begin_totp(&db, &vault_key, &user, "twelve-chars!!")
            .await
            .unwrap();
        let code = totp::generate_now(&mat.secret_b32, now_unix() as u64).unwrap();
        confirm_totp(&db, &vault_key, &user, "twelve-chars!!", &code)
            .await
            .unwrap();
        db.wipe_user_mfa(&user.id).await.unwrap();
        assert!(!db.user_has_mfa(&user.id).await.unwrap());
        let proof = PasswordVerified::after_password_ok(&user, false);
        assert!(proof.session_grant().is_ok());
    }

    #[tokio::test]
    async fn disable_without_valid_code_fails() {
        let db = Db::in_memory().unwrap();
        let vault_key = vault();
        let user = seed_user(&db, "erin", "twelve-chars!!").await;
        let mat = begin_totp(&db, &vault_key, &user, "twelve-chars!!")
            .await
            .unwrap();
        let code = totp::generate_now(&mat.secret_b32, now_unix() as u64).unwrap();
        confirm_totp(&db, &vault_key, &user, "twelve-chars!!", &code)
            .await
            .unwrap();
        let err = disable(&db, &vault_key, &user, "twelve-chars!!", "totp", "000000")
            .await
            .unwrap_err();
        assert!(matches!(err, MfaError::InvalidCode));
        assert!(db.user_has_mfa(&user.id).await.unwrap());
    }

    #[tokio::test]
    async fn totp_replay_rejected_across_login() {
        let db = Db::in_memory().unwrap();
        let vault_key = vault();
        let user = seed_user(&db, "finn", "twelve-chars!!").await;
        let mat = begin_totp(&db, &vault_key, &user, "twelve-chars!!")
            .await
            .unwrap();
        let now = now_unix() as u64;
        let setup = totp::generate_now(&mat.secret_b32, now).unwrap();
        confirm_totp(&db, &vault_key, &user, "twelve-chars!!", &setup)
            .await
            .unwrap();

        let code = totp::generate_now(&mat.secret_b32, now + 30).unwrap();
        let proof = PasswordVerified::after_password_ok(&user, true);
        let issued = issue_challenge(&db, &proof).await.unwrap();
        verify_challenge(&db, &vault_key, &issued.raw_token, "totp", &code)
            .await
            .unwrap();

        let proof = PasswordVerified::after_password_ok(&user, true);
        let issued = issue_challenge(&db, &proof).await.unwrap();
        let err = verify_challenge(&db, &vault_key, &issued.raw_token, "totp", &code)
            .await
            .unwrap_err();
        assert!(matches!(err, MfaError::InvalidCode));
    }

    #[test]
    fn authed_status_is_forbidden_for_wrong_code_or_password() {
        use axum::http::StatusCode;
        assert_eq!(MfaError::InvalidCode.authed_status(), StatusCode::FORBIDDEN);
        assert_eq!(
            MfaError::InvalidPassword.authed_status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(MfaError::InvalidCode.status(), StatusCode::UNAUTHORIZED);
    }
}
