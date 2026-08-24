//! Short-lived MFA login challenges. Opaque token, hashed at rest, 5 min TTL.
//!
//! A challenge is **not** a Bearer JWT. `require_auth` must keep rejecting it.

use rand::RngCore;
use rand::rngs::OsRng;

use crate::auth::token::hash_token;
use crate::db::Db;
use crate::db::models::{MfaChallengeRow, NewMfaChallenge};

use super::{MfaError, PasswordVerified};

const TTL_SECS: i64 = 5 * 60;
pub const MAX_FAILURES: i32 = 8;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Proof token: password was verified and this user has MFA enrolled.
/// Constructed only by [`issue`].
pub struct IssuedChallenge {
    pub raw_token: String,
    pub user_id: String,
}

/// Proof token: bearer presented a live, unconsumed, unexpired challenge.
/// Constructed only by [`load_live`].
pub struct LiveChallenge {
    row: MfaChallengeRow,
}

impl LiveChallenge {
    pub fn user_id(&self) -> &str {
        &self.row.user_id
    }

    pub fn id(&self) -> &str {
        &self.row.id
    }
}
/// Issue a challenge. Only callable with a [`PasswordVerified`] so login
/// cannot mint one without a successful password check.
pub async fn issue(db: &Db, proof: &PasswordVerified) -> Result<IssuedChallenge, MfaError> {
    if !proof.mfa_enabled() {
        return Err(MfaError::NotEnabled);
    }
    let raw = random_token();
    let now = now_unix();
    db.insert_mfa_challenge(NewMfaChallenge {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: proof.user_id().to_string(),
        token_hash: hash_token(&raw),
        created_at: now,
        expires_at: now + TTL_SECS,
        consumed_at: None,
        failures: 0,
    })
    .await
    .map_err(MfaError::Internal)?;
    Ok(IssuedChallenge {
        raw_token: raw,
        user_id: proof.user_id().to_string(),
    })
}
/// Load a live challenge from the raw token the client holds.
pub async fn load_live(db: &Db, raw_token: &str) -> Result<LiveChallenge, MfaError> {
    let token = raw_token.trim();
    if token.is_empty() {
        return Err(MfaError::ChallengeUnknown);
    }
    let row = db
        .get_mfa_challenge_by_token_hash(&hash_token(token))
        .await
        .map_err(MfaError::Internal)?
        .ok_or(MfaError::ChallengeUnknown)?;
    let now = now_unix();
    if row.consumed_at.is_some() {
        return Err(MfaError::ChallengeConsumed);
    }
    if row.expires_at <= now || row.failures >= MAX_FAILURES {
        return Err(MfaError::ChallengeExpired);
    }
    Ok(LiveChallenge { row })
}

pub async fn consume(db: &Db, live: &LiveChallenge) -> Result<(), MfaError> {
    let ok = db
        .consume_mfa_challenge(live.id(), now_unix())
        .await
        .map_err(MfaError::Internal)?;
    if !ok {
        return Err(MfaError::ChallengeConsumed);
    }
    Ok(())
}

/// Record a failed verify. Consumes the challenge once it hits [`MAX_FAILURES`].
pub async fn record_failure(db: &Db, live: &LiveChallenge) -> Result<(), MfaError> {
    let failures = db
        .bump_mfa_challenge_failure(live.id())
        .await
        .map_err(MfaError::Internal)?;
    if failures >= MAX_FAILURES {
        let _ = db.consume_mfa_challenge(live.id(), now_unix()).await;
        return Err(MfaError::LockedOut);
    }
    Ok(())
}
