//! In-memory bearer-token registry for the MCP HTTP endpoint.
//!
//! Tokens are issued per dispatch (the config file a CLI reads at spawn),
//! then bound to the one agent process that actually starts with them: at
//! process start every other token of that session is revoked
//! ([`McpTokenRegistry::bind_process`]), and the bound token dies with the
//! process ([`McpTokenRegistry::release_process`]). A session therefore has
//! at most one live token while it runs, and none after its process exits —
//! except a token already written for its next queued turn. Nothing is
//! persisted, so a restart revokes everything.

use std::collections::HashMap;
use std::sync::Mutex;

/// Metadata associated with an issued MCP token.
pub struct McpTokenInfo {
    pub session_id: String,
    pub project_id: Option<String>,
}

/// A simple in-memory registry mapping token hashes to session metadata.
pub struct McpTokenRegistry {
    tokens: Mutex<HashMap<String, McpTokenInfo>>, // token_hash -> info
    /// session_id -> hash of the token its running agent process holds.
    bound: Mutex<HashMap<String, String>>,
}

impl Default for McpTokenRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn hash(token: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}

impl McpTokenRegistry {
    pub fn new() -> Self {
        McpTokenRegistry {
            tokens: Mutex::new(HashMap::new()),
            bound: Mutex::new(HashMap::new()),
        }
    }

    fn tokens(&self) -> std::sync::MutexGuard<'_, HashMap<String, McpTokenInfo>> {
        self.tokens.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bound(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.bound.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Issue a new bearer token for the given session/project.
    /// Returns the raw token (caller must pass it to the worker).
    pub async fn issue_token(&self, session_id: String, project_id: Option<String>) -> String {
        use rand::Rng;

        let mut raw = [0u8; 24];
        rand::thread_rng().fill(&mut raw);
        let token = hex::encode(raw);

        self.tokens().insert(
            hash(&token),
            McpTokenInfo {
                session_id,
                project_id,
            },
        );
        token
    }

    /// Look up a token by its SHA-256 hash.
    pub async fn lookup(&self, token: &str) -> Option<(String, Option<String>)> {
        self.tokens()
            .get(&hash(token))
            .map(|info| (info.session_id.clone(), info.project_id.clone()))
    }

    /// Revoke all tokens belonging to a session.
    pub async fn revoke_by_session(&self, session_id: &str) {
        self.tokens()
            .retain(|_, info| info.session_id != session_id);
        self.bound().remove(session_id);
    }

    /// An agent process for `session_id` is starting with `token`: revoke
    /// every other token of the session (earlier dispatches' leftovers) and
    /// remember this one as the process's. A token not issued for this
    /// session is ignored.
    pub fn bind_process(&self, session_id: &str, token: &str) {
        let h = hash(token);
        let mut tokens = self.tokens();
        if tokens.get(&h).is_none_or(|i| i.session_id != session_id) {
            return;
        }
        tokens.retain(|k, info| info.session_id != session_id || *k == h);
        self.bound().insert(session_id.to_string(), h);
    }

    /// The agent process for `session_id` exited: revoke the token it held.
    /// Tokens issued since (for a queued next turn) stay valid.
    pub fn release_process(&self, session_id: &str) {
        if let Some(h) = self.bound().remove(session_id) {
            self.tokens().remove(&h);
        }
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

    #[tokio::test]
    async fn one_live_token_per_agent_process() {
        let r = McpTokenRegistry::new();
        let stale = r.issue_token("s".into(), None).await;
        let used = r.issue_token("s".into(), None).await;
        let other = r.issue_token("other".into(), None).await;

        // The process starts with `used`: the earlier leftover dies.
        r.bind_process("s", &used);
        assert!(r.lookup(&stale).await.is_none());
        assert!(r.lookup(&used).await.is_some());
        assert!(r.lookup(&other).await.is_some());

        // A dispatch during the run issues a token for the next turn.
        let next = r.issue_token("s".into(), None).await;
        // The process exits: its token dies, the next turn's survives.
        r.release_process("s");
        assert!(r.lookup(&used).await.is_none());
        assert!(r.lookup(&next).await.is_some());

        // Binding a token of another session is a no-op.
        r.bind_process("s", &other);
        assert!(r.lookup(&next).await.is_some());
    }
}
