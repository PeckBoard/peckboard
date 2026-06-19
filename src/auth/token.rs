use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const JWT_EXPIRY_SECS: u64 = 7 * 24 * 60 * 60; // 7 days

/// JWT claims payload.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    /// Subject (user ID)
    pub sub: String,
    /// User role
    pub role: String,
    /// JWT ID (matches auth_sessions.id)
    pub jti: String,
    /// Issued at (unix timestamp)
    pub iat: u64,
    /// Expiry (unix timestamp)
    pub exp: u64,
}

/// Generate a fresh 256-bit JWT secret key. Used the first time a
/// server starts (when no on-disk key exists yet).
pub fn generate_jwt_secret() -> Vec<u8> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut key = vec![0u8; 32];
    rng.fill(&mut key[..]);
    key
}

/// Load the JWT secret from disk, or generate + persist one on first
/// run. Persisting is what stops every server restart from invalidating
/// every issued token — without this the user gets logged out every
/// time the binary restarts.
///
/// Stored at `<data_dir>/jwt_secret` with `0600` permissions on Unix.
pub fn load_or_create_jwt_secret(data_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let path = data_dir.join("jwt_secret");

    if path.exists() {
        let key = std::fs::read(&path)?;
        if key.len() == 32 {
            return Ok(key);
        }
        tracing::warn!(
            "JWT secret at {} has unexpected length {}, regenerating",
            path.display(),
            key.len(),
        );
    }

    std::fs::create_dir_all(data_dir)?;
    let key = generate_jwt_secret();
    std::fs::write(&path, &key)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    tracing::info!("Generated new JWT secret at {}", path.display());
    Ok(key)
}

/// Create a new JWT token.
pub fn create_token(
    secret: &[u8],
    user_id: &str,
    role: &str,
    session_id: &str,
) -> anyhow::Result<(String, u64)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let exp = now + JWT_EXPIRY_SECS;

    let claims = Claims {
        sub: user_id.to_string(),
        role: role.to_string(),
        jti: session_id.to_string(),
        iat: now,
        exp,
    };

    // Pin the algorithm explicitly so a future swap of the secret type
    // (HMAC → public key) can't quietly downgrade signing, and so the
    // decode side has a single fixed expectation to match.
    let header = Header::new(Algorithm::HS256);
    let token = encode(&header, &claims, &EncodingKey::from_secret(secret))
        .map_err(|e| anyhow::anyhow!("failed to create JWT: {e}"))?;

    Ok((token, exp))
}

/// Validate a JWT token and return its claims.
/// Does NOT check server-side session revocation — caller must do that.
pub fn validate_token(secret: &[u8], token: &str) -> anyhow::Result<Claims> {
    // Pin to HS256 on decode. Without this, `Validation::default()`
    // would accept any algorithm the token's `alg` header claims —
    // including the `none` family attacks against poorly-written
    // jsonwebtoken consumers.
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;

    let data = decode::<Claims>(token, &DecodingKey::from_secret(secret), &validation)
        .map_err(|e| anyhow::anyhow!("invalid token: {e}"))?;

    Ok(data.claims)
}

/// SHA-256 hash of a token for storage (never store raw tokens).
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_validate_token() {
        let secret = generate_jwt_secret();
        let (token, _exp) = create_token(&secret, "user1", "admin", "sess1").unwrap();

        let claims = validate_token(&secret, &token).unwrap();
        assert_eq!(claims.sub, "user1");
        assert_eq!(claims.role, "admin");
        assert_eq!(claims.jti, "sess1");
    }

    #[test]
    fn test_invalid_secret_fails() {
        let secret1 = generate_jwt_secret();
        let secret2 = generate_jwt_secret();
        let (token, _) = create_token(&secret1, "user1", "admin", "sess1").unwrap();

        assert!(validate_token(&secret2, &token).is_err());
    }

    #[test]
    fn test_malformed_token_fails() {
        let secret = generate_jwt_secret();
        assert!(validate_token(&secret, "not.a.jwt").is_err());
        assert!(validate_token(&secret, "").is_err());
    }

    #[test]
    fn test_hash_token_deterministic() {
        let token = "test-token-value";
        let h1 = hash_token(token);
        let h2 = hash_token(token);
        assert_eq!(h1, h2);
        assert_ne!(h1, hash_token("different-token"));
    }

    #[test]
    fn test_jwt_secret_persists_across_loads() {
        // Stops the regression where every server restart invalidated
        // every issued token.
        let dir = tempfile::tempdir().unwrap();
        let secret1 = load_or_create_jwt_secret(dir.path()).unwrap();
        let secret2 = load_or_create_jwt_secret(dir.path()).unwrap();
        assert_eq!(secret1, secret2, "second load must return the same key");
        assert_eq!(secret1.len(), 32);

        let (token, _) = create_token(&secret1, "u", "admin", "s").unwrap();
        assert!(validate_token(&secret2, &token).is_ok());
    }

    #[test]
    fn test_jwt_secret_regenerated_when_truncated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("jwt_secret"), b"too-short").unwrap();
        let secret = load_or_create_jwt_secret(dir.path()).unwrap();
        assert_eq!(secret.len(), 32);
    }
}
