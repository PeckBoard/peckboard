use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

/// Event payload broadcast to WebSocket clients.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WsEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub session_id: String,
    pub data: serde_json::Value,
}

/// Manages WebSocket client subscriptions and event fan-out.
pub struct Broadcaster {
    /// session_id → set of client IDs subscribed to it
    subscriptions: Mutex<HashMap<String, HashSet<u64>>>,
    /// Broadcast channel for all events
    tx: broadcast::Sender<WsEvent>,
}

impl Broadcaster {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        Arc::new(Broadcaster {
            subscriptions: Mutex::new(HashMap::new()),
            tx,
        })
    }

    /// Subscribe a client to a session's events.
    pub async fn subscribe(&self, client_id: u64, session_id: &str) {
        let mut subs = self.subscriptions.lock().await;
        subs.entry(session_id.to_string())
            .or_default()
            .insert(client_id);
    }

    /// Unsubscribe a client from a session's events.
    pub async fn unsubscribe(&self, client_id: u64, session_id: &str) {
        let mut subs = self.subscriptions.lock().await;
        if let Some(clients) = subs.get_mut(session_id) {
            clients.remove(&client_id);
            if clients.is_empty() {
                subs.remove(session_id);
            }
        }
    }

    /// Remove a client from all subscriptions.
    pub async fn remove_client(&self, client_id: u64) {
        let mut subs = self.subscriptions.lock().await;
        subs.retain(|_, clients| {
            clients.remove(&client_id);
            !clients.is_empty()
        });
    }

    /// Broadcast an event to all subscribed clients.
    pub fn broadcast(&self, event: WsEvent) {
        // Ignore send errors (no active receivers).
        let _ = self.tx.send(event);
    }

    /// Get a receiver for broadcast events.
    pub fn subscribe_all(&self) -> broadcast::Receiver<WsEvent> {
        self.tx.subscribe()
    }

    /// Check if any clients are subscribed to a session.
    pub async fn has_subscribers(&self, session_id: &str) -> bool {
        let subs = self.subscriptions.lock().await;
        subs.get(session_id)
            .map(|c| !c.is_empty())
            .unwrap_or(false)
    }
}
