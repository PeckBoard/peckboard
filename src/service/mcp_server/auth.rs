//! In-memory bearer-token registry for the MCP HTTP endpoint.

use std::collections::HashMap;

use tokio::sync::Mutex;

/// Metadata associated with an issued MCP token.
pub struct McpTokenInfo {
    pub session_id: String,
    pub project_id: Option<String>,
}

/// A simple in-memory registry mapping token hashes to session metadata.
pub struct McpTokenRegistry {
    tokens: Mutex<HashMap<String, McpTokenInfo>>, // token_hash -> info
}

impl Default for McpTokenRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl McpTokenRegistry {
    pub fn new() -> Self {
        McpTokenRegistry {
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a new bearer token for the given session/project.
    /// Returns the raw token (caller must pass it to the worker).
    pub async fn issue_token(&self, session_id: String, project_id: Option<String>) -> String {
        use rand::Rng;
        use sha2::Digest;

        let mut raw = [0u8; 24];
        rand::thread_rng().fill(&mut raw);
        let token = hex::encode(raw);

        let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));

        self.tokens.lock().await.insert(
            hash,
            McpTokenInfo {
                session_id,
                project_id,
            },
        );
        token
    }

    /// Look up a token by its SHA-256 hash.
    pub async fn lookup(&self, token: &str) -> Option<(String, Option<String>)> {
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));
        let guard = self.tokens.lock().await;
        guard
            .get(&hash)
            .map(|info| (info.session_id.clone(), info.project_id.clone()))
    }

    /// Revoke all tokens belonging to a session.
    pub async fn revoke_by_session(&self, session_id: &str) {
        self.tokens
            .lock()
            .await
            .retain(|_, info| info.session_id != session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_token_registry_issue_and_lookup() {
        let registry = McpTokenRegistry::new();
        let token = registry
            .issue_token("sess-1".into(), Some("proj-a".into()))
            .await;

        assert_eq!(token.len(), 48); // 24 bytes => 48 hex chars

        let info = registry.lookup(&token).await;
        assert!(info.is_some());
        let (sid, pid) = info.unwrap();
        assert_eq!(sid, "sess-1");
        assert_eq!(pid.as_deref(), Some("proj-a"));

        // Unknown token returns None
        assert!(registry.lookup("bogus").await.is_none());
    }

    #[tokio::test]
    async fn test_token_registry_revoke_by_session() {
        let registry = McpTokenRegistry::new();
        let t1 = registry.issue_token("sess-1".into(), None).await;
        let t2 = registry
            .issue_token("sess-1".into(), Some("proj-b".into()))
            .await;
        let t3 = registry.issue_token("sess-2".into(), None).await;

        registry.revoke_by_session("sess-1").await;

        assert!(registry.lookup(&t1).await.is_none());
        assert!(registry.lookup(&t2).await.is_none());
        assert!(registry.lookup(&t3).await.is_some());
    }
}
