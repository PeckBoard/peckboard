//! First-run bootstrap: ensure exactly one admin user exists, generating
//! and printing credentials on first startup.
//!
//! Peckboard does not expose self-service registration. The very first
//! server start creates the sole admin account and prints its password
//! to stdout once. After that, additional users can only be added by an
//! authenticated admin via `POST /api/users`.

use rand::Rng;

use crate::auth::password::hash_password;
use crate::db::Db;
use crate::db::models::NewUser;

/// Outcome of the first-run admin bootstrap.
#[derive(Debug)]
pub struct BootstrapOutcome {
    pub username: String,
    pub new_password: String,
}

/// Default bootstrap username.
const BOOTSTRAP_USERNAME: &str = "admin";

/// Bootstrap an admin user if (and only if) the users table is empty.
///
/// Returns `Ok(Some(_))` if a new admin was created (caller is expected
/// to print the password to the operator), `Ok(None)` if at least one
/// user already existed, and `Err(_)` only on hard DB failure.
///
/// The username and password are read from the environment if set:
///   - `PECKBOARD_BOOTSTRAP_USERNAME` — overrides the default "admin"
///   - `PECKBOARD_BOOTSTRAP_PASSWORD` — overrides the random password
///
/// These exist primarily for e2e tests and unattended installs where a
/// known credential is needed. They only take effect when the users
/// table is empty, so they cannot be used to overwrite an existing
/// admin. In normal use, leave them unset and read the credentials
/// printed on first start.
pub async fn ensure_admin_user(db: &Db) -> anyhow::Result<Option<BootstrapOutcome>> {
    let count = db.count_users().await?;
    if count > 0 {
        return Ok(None);
    }

    let username = std::env::var("PECKBOARD_BOOTSTRAP_USERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| BOOTSTRAP_USERNAME.to_string());
    let new_password = std::env::var("PECKBOARD_BOOTSTRAP_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(generate_password);
    let password_hash = hash_password(&new_password)?;
    let now = chrono::Utc::now().to_rfc3339();

    // If two concurrent startups race (only possible if the user runs
    // two binaries against the same data dir simultaneously, which is
    // unsupported), the UNIQUE constraint on users.username will reject
    // the loser. Surface that as "someone else already bootstrapped"
    // instead of a generic error.
    match db
        .create_user(NewUser {
            id: uuid::Uuid::new_v4().to_string(),
            username: username.clone(),
            email: None,
            password_hash,
            role: "admin".into(),
            created_at: now.clone(),
            updated_at: now,
        })
        .await
    {
        Ok(_) => Ok(Some(BootstrapOutcome {
            username,
            new_password,
        })),
        Err(e) => {
            if e.to_string().to_lowercase().contains("unique") {
                tracing::warn!("Bootstrap race: another startup created the admin first");
                Ok(None)
            } else {
                Err(e.into())
            }
        }
    }
}

/// Generate a 20-character URL-safe password. ~120 bits of entropy.
/// Matches the alphabet used by `--reset-password` so operators see one
/// consistent format whichever path produced the credentials.
fn generate_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..20)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password::verify_password;

    #[tokio::test]
    async fn bootstraps_admin_on_empty_db() {
        let db = Db::in_memory().unwrap();
        let outcome = ensure_admin_user(&db).await.unwrap().expect("created");
        assert_eq!(outcome.username, "admin");
        assert_eq!(outcome.new_password.len(), 20);

        let user = db.get_user_by_username("admin").await.unwrap().unwrap();
        assert_eq!(user.role, "admin");
        assert!(verify_password(&outcome.new_password, &user.password_hash));
    }

    #[tokio::test]
    async fn skips_bootstrap_when_users_exist() {
        let db = Db::in_memory().unwrap();
        // Seed an unrelated user.
        let now = chrono::Utc::now().to_rfc3339();
        db.create_user(NewUser {
            id: uuid::Uuid::new_v4().to_string(),
            username: "alice".into(),
            email: None,
            password_hash: hash_password("pw").unwrap(),
            role: "admin".into(),
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .unwrap();

        let outcome = ensure_admin_user(&db).await.unwrap();
        assert!(outcome.is_none(), "must not bootstrap when users exist");
    }
}
