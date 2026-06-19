//! `--reset-password` CLI flow.
//!
//! Resets a single user's password to a freshly-generated random value
//! and revokes their auth sessions so any leaked tokens stop working.
//! Deliberately narrow: it does not touch other users, never wipes the
//! `users` table, and prints the new password to stdout exactly once.

use rand::Rng;

use crate::auth::password::hash_password;
use crate::db::Db;
use crate::db::models::UpdateUser;

/// Outcome of a reset. Returned so callers (and tests) can verify what
/// happened without re-querying the DB.
#[derive(Debug)]
pub struct ResetOutcome {
    pub username: String,
    pub new_password: String,
    pub sessions_revoked: usize,
}

/// Reset the password of `username` (or the only user, if `username` is
/// None and exactly one user exists). Returns an error if the target
/// can't be uniquely identified — never blanket-wipes users.
pub async fn reset_user_password(db: &Db, username: Option<&str>) -> anyhow::Result<ResetOutcome> {
    let user = match username {
        Some(name) => db
            .get_user_by_username(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no user named '{name}'"))?,
        None => {
            let users = db.list_users().await?;
            match users.len() {
                0 => anyhow::bail!("no users exist; register the first user via the web UI"),
                1 => users.into_iter().next().unwrap(),
                n => anyhow::bail!("{n} users exist; pass --user <username> to pick one",),
            }
        }
    };

    let new_password = generate_password();
    let password_hash = hash_password(&new_password)?;
    let now = chrono::Utc::now().to_rfc3339();

    db.update_user(
        &user.id,
        UpdateUser {
            password_hash: Some(password_hash),
            updated_at: Some(now),
            ..Default::default()
        },
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("failed to update user '{}'", user.username))?;

    let sessions_revoked = db.delete_auth_sessions_by_user(&user.id).await?;

    Ok(ResetOutcome {
        username: user.username,
        new_password,
        sessions_revoked,
    })
}

/// Generate a 16-character URL-safe password. ~95 bits of entropy —
/// more than enough for an interactive admin password.
fn generate_password() -> String {
    // Avoid look-alike characters (0/O, 1/l/I) for easier transcription.
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password::verify_password;
    use crate::db::models::{NewAuthSession, NewUser};

    async fn seed_user(db: &Db, username: &str, password: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        db.create_user(NewUser {
            id: id.clone(),
            username: username.into(),
            email: None,
            password_hash: hash_password(password).unwrap(),
            role: "admin".into(),
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .unwrap();
        id
    }

    async fn seed_session(db: &Db, user_id: &str) -> String {
        let session_id = uuid::Uuid::new_v4().to_string();
        db.create_auth_session(NewAuthSession {
            id: session_id.clone(),
            user_id: user_id.into(),
            token_hash: "deadbeef".into(),
            created_at: 0,
            expires_at: 9999999999,
            user_agent: None,
            ip_address: None,
        })
        .await
        .unwrap();
        session_id
    }

    #[tokio::test]
    async fn resets_single_user_without_username() {
        let db = Db::in_memory().unwrap();
        let user_id = seed_user(&db, "alice", "old-password").await;
        seed_session(&db, &user_id).await;
        seed_session(&db, &user_id).await;

        let outcome = reset_user_password(&db, None).await.unwrap();

        assert_eq!(outcome.username, "alice");
        assert_eq!(outcome.sessions_revoked, 2);
        assert_eq!(outcome.new_password.len(), 16);

        let user = db.get_user_by_username("alice").await.unwrap().unwrap();
        assert!(verify_password(&outcome.new_password, &user.password_hash));
        assert!(!verify_password("old-password", &user.password_hash));
        assert_eq!(
            db.list_auth_sessions_by_user(&user_id).await.unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn requires_username_when_multiple_users() {
        let db = Db::in_memory().unwrap();
        seed_user(&db, "alice", "pw1").await;
        seed_user(&db, "bob", "pw2").await;

        let err = reset_user_password(&db, None).await.unwrap_err();
        assert!(err.to_string().contains("2 users"));
    }

    #[tokio::test]
    async fn resets_named_user_and_leaves_others_alone() {
        let db = Db::in_memory().unwrap();
        let alice_id = seed_user(&db, "alice", "pw1").await;
        seed_user(&db, "bob", "bob-original").await;
        seed_session(&db, &alice_id).await;

        let outcome = reset_user_password(&db, Some("alice")).await.unwrap();
        assert_eq!(outcome.username, "alice");
        assert_eq!(outcome.sessions_revoked, 1);

        let bob = db.get_user_by_username("bob").await.unwrap().unwrap();
        assert!(
            verify_password("bob-original", &bob.password_hash),
            "bob's password must be untouched",
        );
    }

    #[tokio::test]
    async fn errors_when_username_does_not_exist() {
        let db = Db::in_memory().unwrap();
        seed_user(&db, "alice", "pw").await;

        let err = reset_user_password(&db, Some("nobody")).await.unwrap_err();
        assert!(err.to_string().contains("no user named 'nobody'"));
    }

    #[tokio::test]
    async fn errors_when_no_users_exist() {
        let db = Db::in_memory().unwrap();
        let err = reset_user_password(&db, None).await.unwrap_err();
        assert!(err.to_string().contains("no users exist"));
    }
}
