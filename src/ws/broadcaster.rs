use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, broadcast};

/// Event payload broadcast to WebSocket clients.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WsEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub session_id: String,
    pub data: serde_json::Value,
}

/// Which of one user's live connections a per-user frame reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAudience {
    /// Every connection of the user — data refreshes (`views-changed`).
    AllTabs,
    /// The tab(s) the user is looking at — navigations (`ui-navigate`):
    /// every connection whose page last reported itself visible; when none
    /// is visible, only the one that was most recently visible or connected,
    /// so a backgrounded browser still follows the agent but two hidden tabs
    /// never both jump.
    Foreground,
}

/// A frame addressed to specific connections of ONE user. Its only
/// constructor is [`Broadcaster::send_to_user`], which resolves the targets
/// from the user's own registered connections, and it travels on a channel
/// separate from [`WsEvent`]'s — so a per-user frame can never be delivered
/// by the global / per-session fan-out by mistake.
#[derive(Debug, Clone)]
pub struct UserFrame {
    targets: Arc<[u64]>,
    event: WsEvent,
}

impl UserFrame {
    /// Whether connection `client_id` is one of this frame's recipients.
    pub fn is_for(&self, client_id: u64) -> bool {
        self.targets.contains(&client_id)
    }

    pub fn event(&self) -> &WsEvent {
        &self.event
    }
}

/// One authenticated WS connection, for per-user targeting.
struct Connection {
    user_id: String,
    visible: bool,
    /// Monotonic stamp of the last connect / became-visible.
    last_active: u64,
}

/// Manages WebSocket client subscriptions and event fan-out.
pub struct Broadcaster {
    /// session_id → set of client IDs subscribed to it
    subscriptions: Mutex<HashMap<String, HashSet<u64>>>,
    /// Broadcast channel for all events
    tx: broadcast::Sender<WsEvent>,
    /// Per-user frames ([`UserFrame`]); each connection forwards only the
    /// ones naming its own client id.
    user_tx: broadcast::Sender<UserFrame>,
    /// client_id → owner + visibility. A sync mutex: held only for map
    /// edits, never across an await.
    connections: std::sync::Mutex<HashMap<u64, Connection>>,
    activity: AtomicU64,
}

impl Broadcaster {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        let (user_tx, _) = broadcast::channel(256);
        Arc::new(Broadcaster {
            subscriptions: Mutex::new(HashMap::new()),
            tx,
            user_tx,
            connections: std::sync::Mutex::new(HashMap::new()),
            activity: AtomicU64::new(1),
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
        self.lock_connections().remove(&client_id);
        let mut subs = self.subscriptions.lock().await;
        subs.retain(|_, clients| {
            clients.remove(&client_id);
            !clients.is_empty()
        });
    }

    fn lock_connections(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Connection>> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record an authenticated connection of `user_id` (the JWT subject),
    /// so [`Self::send_to_user`] can reach it. Starts not-visible until the
    /// page reports otherwise.
    pub fn register_connection(&self, client_id: u64, user_id: &str) {
        let stamp = self.activity.fetch_add(1, Ordering::Relaxed);
        self.lock_connections().insert(
            client_id,
            Connection {
                user_id: user_id.to_string(),
                visible: false,
                last_active: stamp,
            },
        );
    }

    /// The page behind `client_id` became visible / hidden.
    pub fn set_visible(&self, client_id: u64, visible: bool) {
        let stamp = self.activity.fetch_add(1, Ordering::Relaxed);
        if let Some(c) = self.lock_connections().get_mut(&client_id) {
            if visible {
                c.last_active = stamp;
            }
            c.visible = visible;
        }
    }

    /// Send `event` to `user_id`'s live connections only (see
    /// [`UserAudience`]). Returns how many connections it was addressed to
    /// — 0 when the user has no browser open.
    pub fn send_to_user(&self, user_id: &str, audience: UserAudience, event: WsEvent) -> usize {
        let targets: Vec<u64> = {
            let conns = self.lock_connections();
            let mine = conns.iter().filter(|(_, c)| c.user_id == user_id);
            match audience {
                UserAudience::AllTabs => mine.map(|(id, _)| *id).collect(),
                UserAudience::Foreground => {
                    let visible: Vec<u64> = mine
                        .clone()
                        .filter(|(_, c)| c.visible)
                        .map(|(id, _)| *id)
                        .collect();
                    if visible.is_empty() {
                        mine.max_by_key(|(_, c)| c.last_active)
                            .map(|(id, _)| *id)
                            .into_iter()
                            .collect()
                    } else {
                        visible
                    }
                }
            }
        };
        let n = targets.len();
        if n > 0 {
            let _ = self.user_tx.send(UserFrame {
                targets: targets.into(),
                event,
            });
        }
        n
    }

    /// Receiver for per-user frames (the WS handler filters by client id).
    pub fn subscribe_user_frames(&self) -> broadcast::Receiver<UserFrame> {
        self.user_tx.subscribe()
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

    /// Check if a specific client is subscribed to a session. The send
    /// decision in the WS handler must be per-client — checking only
    /// "does anyone subscribe" would fan every session's events out to
    /// every connected client (including non-admins who are denied
    /// subscribe in the first place).
    pub async fn is_subscribed(&self, client_id: u64, session_id: &str) -> bool {
        let subs = self.subscriptions.lock().await;
        subs.get(session_id)
            .map(|c| c.contains(&client_id))
            .unwrap_or(false)
    }

    /// Every session `client_id` is currently subscribed to. Used by the WS
    /// handler when the broadcast channel reports a lag: the dropped events
    /// are unrecoverable from the channel, so each affected session has to be
    /// re-resumed from the event log.
    pub async fn sessions_for_client(&self, client_id: u64) -> Vec<String> {
        let subs = self.subscriptions.lock().await;
        subs.iter()
            .filter(|(_, clients)| clients.contains(&client_id))
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: &str) -> WsEvent {
        WsEvent {
            event_type: t.into(),
            session_id: "src".into(),
            data: serde_json::json!({}),
        }
    }

    #[test]
    fn per_user_frames_reach_only_that_users_connections() {
        let b = Broadcaster::new();
        let mut global = b.subscribe_all();
        let mut rx = b.subscribe_user_frames();
        b.register_connection(1, "alice");
        b.register_connection(2, "alice");
        b.register_connection(3, "bob");

        // AllTabs: both of alice's connections, never bob's, never global.
        assert_eq!(
            b.send_to_user("alice", UserAudience::AllTabs, ev("views-changed")),
            2
        );
        let f = rx.try_recv().unwrap();
        assert!(f.is_for(1) && f.is_for(2) && !f.is_for(3));
        assert!(global.try_recv().is_err(), "never on the global channel");

        // Foreground, nothing visible: only the most recently connected.
        assert_eq!(
            b.send_to_user("alice", UserAudience::Foreground, ev("ui-navigate")),
            1
        );
        let f = rx.try_recv().unwrap();
        assert!(f.is_for(2) && !f.is_for(1));

        // A visible tab wins over a newer hidden one.
        b.set_visible(1, true);
        b.register_connection(4, "alice");
        assert_eq!(
            b.send_to_user("alice", UserAudience::Foreground, ev("ui-navigate")),
            1
        );
        assert!(rx.try_recv().unwrap().is_for(1));

        // All hidden again: 4 connected after 1 last became visible, so
        // 4 is the most recently active and alone gets it.
        b.set_visible(1, false);
        b.send_to_user("alice", UserAudience::Foreground, ev("ui-navigate"));
        assert!(rx.try_recv().unwrap().is_for(4));

        // No browser at all: nothing sent.
        assert_eq!(
            b.send_to_user("carol", UserAudience::AllTabs, ev("views-changed")),
            0
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn removed_connections_are_no_longer_targeted() {
        let b = Broadcaster::new();
        b.register_connection(7, "alice");
        b.remove_client(7).await;
        assert_eq!(
            b.send_to_user("alice", UserAudience::AllTabs, ev("views-changed")),
            0
        );
    }
}
