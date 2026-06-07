use std::sync::Arc;
use std::time::Duration;

use crate::db::Db;
use crate::provider::manager::SessionManager;
use crate::ws::broadcaster::Broadcaster;

/// Start the worker watchdog loop. Runs every 60 seconds and cleans up orphaned
/// worker sessions whose associated cards no longer reference them.
pub async fn start_watchdog(
    db: Db,
    session_manager: SessionManager,
    broadcaster: Arc<Broadcaster>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    // The broadcaster is kept for future use (e.g., notifying on cleanup).
    let _broadcaster = broadcaster;
    loop {
        interval.tick().await;
        sweep_orphans(&db, &session_manager).await;
    }
}

/// Scan all worker sessions and remove those whose cards no longer reference them.
///
/// A worker session is considered orphaned when:
/// - Its `card_id` is set but the card does not exist, OR
/// - The card exists but its `worker_session_id` does not match this session
async fn sweep_orphans(db: &Db, session_manager: &SessionManager) {
    let worker_sessions = match db.list_worker_sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::error!("Watchdog: failed to list worker sessions: {e}");
            return;
        }
    };

    if worker_sessions.is_empty() {
        return;
    }

    let mut cleaned = 0u32;

    for session in &worker_sessions {
        let is_orphan = match &session.card_id {
            Some(card_id) => {
                match db.get_card(card_id).await {
                    Ok(Some(card)) => {
                        // Card exists but doesn't reference this session
                        // (check both current and last worker session)
                        card.worker_session_id.as_deref() != Some(&session.id)
                            && card.last_worker_session_id.as_deref() != Some(&session.id)
                    }
                    Ok(None) => {
                        // Card doesn't exist
                        true
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Watchdog: failed to get card {} for session {}: {e}",
                            card_id,
                            session.id
                        );
                        false // Don't clean up on error
                    }
                }
            }
            None => {
                // Worker session with no card_id is orphaned
                true
            }
        };

        if is_orphan {
            // Cancel any running process for this session
            session_manager.cancel(&session.id).await;

            // Delete events for this session
            match db.delete_events_by_session(&session.id).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            "Watchdog: deleted {count} event(s) for orphaned worker session {}",
                            session.id
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Watchdog: failed to delete events for session {}: {e}",
                        session.id
                    );
                }
            }

            // Delete the session itself
            match db.delete_session(&session.id).await {
                Ok(true) => {
                    cleaned += 1;
                    tracing::info!(
                        "Watchdog: cleaned up orphaned worker session {} (card_id: {:?})",
                        session.id,
                        session.card_id
                    );
                }
                Ok(false) => {
                    tracing::debug!("Watchdog: session {} already deleted", session.id);
                }
                Err(e) => {
                    tracing::error!("Watchdog: failed to delete session {}: {e}", session.id);
                }
            }
        }
    }

    if cleaned > 0 {
        tracing::info!("Watchdog: cleaned up {cleaned} orphaned worker session(s)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::*;

    async fn setup() -> Db {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        db.create_project(NewProject {
            id: "p1".into(),
            name: "Test Project".into(),
            context: "test".into(),
            folder_id: "f1".into(),
            worker_count: 2,
            status: "active".into(),
            default_workflow: None,
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
        })
        .await
        .unwrap();

        db
    }

    #[tokio::test]
    async fn test_sweep_orphans_no_sessions() {
        let db = setup().await;
        let sm = SessionManager::new(std::sync::Arc::new(
            crate::provider::registry::ProviderRegistry::new(),
        ));
        // Should not panic with no worker sessions
        sweep_orphans(&db, &sm).await;
    }

    #[tokio::test]
    async fn test_sweep_orphans_cleans_no_card_session() {
        let db = setup().await;
        let sm = SessionManager::new(std::sync::Arc::new(
            crate::provider::registry::ProviderRegistry::new(),
        ));
        let ts = chrono::Utc::now().to_rfc3339();

        // Create a worker session with no card_id (orphaned worker)
        db.create_session(NewSession {
            id: "ws1".into(),
            name: "worker-1".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
        })
        .await
        .unwrap();

        // Verify session exists
        assert!(db.get_session("ws1").await.unwrap().is_some());

        sweep_orphans(&db, &sm).await;

        // Session should be cleaned up (worker with no card is orphaned)
        assert!(db.get_session("ws1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_sweep_orphans_keeps_valid_session() {
        let db = setup().await;
        let sm = SessionManager::new(std::sync::Arc::new(
            crate::provider::registry::ProviderRegistry::new(),
        ));
        let ts = chrono::Utc::now().to_rfc3339();

        // Create the worker session first (no card_id yet)
        db.create_session(NewSession {
            id: "ws1".into(),
            name: "worker-1".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
        })
        .await
        .unwrap();

        // Create a card that references the worker session
        db.create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Test Card".into(),
            description: "test".into(),
            step: "in_progress".into(),
            priority: 1,
            workflow: None,
            model: None,
            effort: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

        // Set the card's worker_session_id to point to ws1
        db.update_card(
            "c1",
            UpdateCard {
                worker_session_id: Some(Some("ws1".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Now update the session to point to the card
        db.update_session(
            "ws1",
            UpdateSession {
                card_id: Some(Some("c1".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        sweep_orphans(&db, &sm).await;

        // Session should still exist because the card references it
        assert!(db.get_session("ws1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_sweep_orphans_cleans_mismatched_session() {
        let db = setup().await;
        let sm = SessionManager::new(std::sync::Arc::new(
            crate::provider::registry::ProviderRegistry::new(),
        ));
        let ts = chrono::Utc::now().to_rfc3339();

        // Create two worker sessions
        db.create_session(NewSession {
            id: "ws1".into(),
            name: "worker-1".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
        })
        .await
        .unwrap();

        db.create_session(NewSession {
            id: "ws2".into(),
            name: "worker-2".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
        })
        .await
        .unwrap();

        // Create a card that references ws2 (not ws1)
        db.create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Test Card".into(),
            description: "test".into(),
            step: "in_progress".into(),
            priority: 1,
            workflow: None,
            model: None,
            effort: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

        db.update_card(
            "c1",
            UpdateCard {
                worker_session_id: Some(Some("ws2".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Both sessions point to card c1, but only ws2 is referenced back
        db.update_session(
            "ws1",
            UpdateSession {
                card_id: Some(Some("c1".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        db.update_session(
            "ws2",
            UpdateSession {
                card_id: Some(Some("c1".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        sweep_orphans(&db, &sm).await;

        // ws1 should be cleaned up (card references ws2, not ws1)
        assert!(db.get_session("ws1").await.unwrap().is_none());
        // ws2 should still exist (card references it)
        assert!(db.get_session("ws2").await.unwrap().is_some());
    }
}
