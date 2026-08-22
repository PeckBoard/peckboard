//! Mint a server-side auth session + JWT. Shared by login, password
//! change, and desktop first-run auto-login.

use crate::auth::token::{create_token, hash_token};
use crate::db::Db;
use crate::db::models::NewAuthSession;

/// Create an `auth_sessions` row and return the raw JWT.
///
/// `require_auth` rejects tokens whose `jti` is missing from this table,
/// so every issuer (login, password-change, desktop bootstrap) must go
/// through here.
pub async fn issue_session_token(
    db: &Db,
    jwt_secret: &[u8],
    user_id: &str,
    role: &str,
    user_agent: Option<String>,
    ip_address: Option<String>,
) -> anyhow::Result<String> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (token, exp) = create_token(jwt_secret, user_id, role, &session_id)?;

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    db.create_auth_session(NewAuthSession {
        id: session_id,
        user_id: user_id.to_string(),
        token_hash: hash_token(&token),
        created_at: now_ts,
        expires_at: exp as i64,
        user_agent,
        ip_address,
    })
    .await?;

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password::hash_password;
    use crate::auth::token::{generate_jwt_secret, validate_token};
    use crate::db::models::NewUser;

    #[tokio::test]
    async fn issued_token_has_matching_auth_session() {
        let db = Db::in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let user = db
            .create_user(NewUser {
                id: uuid::Uuid::new_v4().to_string(),
                username: "alice".into(),
                email: None,
                password_hash: hash_password("twelve-chars!!").unwrap(),
                role: "admin".into(),
                created_at: now.clone(),
                updated_at: now,
            })
            .await
            .unwrap();

        let secret = generate_jwt_secret();
        let token = issue_session_token(
            &db,
            &secret,
            &user.id,
            &user.role,
            Some("Peckboard desktop".into()),
            Some("127.0.0.1".into()),
        )
        .await
        .unwrap();

        let claims = validate_token(&secret, &token).unwrap();
        assert_eq!(claims.sub, user.id);
        assert_eq!(claims.role, "admin");
        let session = db
            .get_auth_session(&claims.jti)
            .await
            .unwrap()
            .expect("session row must exist so require_auth accepts the JWT");
        assert_eq!(session.user_id, user.id);
        assert_eq!(session.user_agent.as_deref(), Some("Peckboard desktop"));
        assert_eq!(session.ip_address.as_deref(), Some("127.0.0.1"));
    }
}
