use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

/// Generate a JWT secret key. In production this should be persisted.
/// For now, generates a random 256-bit key on startup.
pub fn generate_jwt_secret() -> Vec<u8> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut key = vec![0u8; 32];
    rng.fill(&mut key[..]);
    key
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

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret),
    )
    .map_err(|e| anyhow::anyhow!("failed to create JWT: {e}"))?;

    Ok((token, exp))
}

/// Validate a JWT token and return its claims.
/// Does NOT check server-side session revocation — caller must do that.
pub fn validate_token(secret: &[u8], token: &str) -> anyhow::Result<Claims> {
    let mut validation = Validation::default();
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
}
