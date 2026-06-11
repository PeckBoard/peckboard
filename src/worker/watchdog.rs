use std::sync::Arc;
use std::time::Duration;

use crate::db::Db;
use crate::provider::manager::SessionManager;
use crate::ws::broadcaster::Broadcaster;

/// How recent `last_activity` must be (in seconds) for the watchdog to
/// skip a session as "still settling". Long enough that a slow completion
/// handler (queue drain + worker bookkeeping + plugin dispatch) cannot
/// finish inside the window; short enough that genuinely dead sessions
/// still get cleaned up promptly. Exposed for tests.
pub const ORPHAN_GRACE_SECS: i64 = 90;

/// Start the worker watchdog loop. Runs every 60 seconds and:
///   1. Sweeps orphan worker sessions whose card no longer references
///      them (`sweep_orphans`).
///   2. Sweeps stale `card.worker_session_id` references that point at a
///      dead or missing session (`sweep_stale_card_refs`). Without this
///      pass a card with a vanished worker silently consumes a slot
///      forever, which is exactly the "stuck in in_progress" symptom the
///      user just hit when a disk-space failure killed a worker mid-run.
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
        sweep_stale_card_refs(&db, &session_manager).await;
    }
}

/// Detect cards that still carry a `worker_session_id` even though the
/// referenced session is gone or has stopped streaming. Without this
/// pass, a worker that dies without emitting an `agent-end` event (a
/// disk-space failure, a kernel OOM kill, a network blip that drops the
/// streaming task) leaves the card pinned to a dead worker — the
/// orchestrator's filter
/// (`worker_session_id.is_some()` counts as an active slot) then refuses
/// to spawn a replacement and the card sits idle in `in_progress`
/// forever.
///
/// Conditions for clearing:
///   - card has a non-empty `worker_session_id`,
///   - card is not in a terminal step,
///   - the referenced session either doesn't exist OR is not in-flight
///     AND its `last_activity` is older than `ORPHAN_GRACE_SECS`,
///   - the session lock can be acquired (i.e. no handler mid-flight on
///     this session — same guard `sweep_orphans` uses).
///
/// The clear uses `Db::clear_card_worker_if_matches` so a concurrent
/// orchestrator spawn that's already replaced the assignment can't be
/// clobbered. This pass is the runtime equivalent of
/// `security::repair_dangling_sessions`, which only runs at startup.
async fn sweep_stale_card_refs(db: &Db, session_manager: &SessionManager) {
    let projects = match db.list_projects().await {
        Ok(projects) => projects,
        Err(e) => {
            tracing::error!("Watchdog: failed to list projects for stale-ref sweep: {e}");
            return;
        }
    };

    let mut cleared = 0u32;

    for project in &projects {
        let cards = match db.list_cards_by_project(&project.id).await {
            Ok(cards) => cards,
            Err(e) => {
                tracing::warn!(project_id = %project.id, "Watchdog: list_cards_by_project failed: {e}");
                continue;
            }
        };

        for card in &cards {
            let Some(session_id) = card.worker_session_id.as_deref() else {
                continue;
            };
            if card.step == "done" || card.step == "wont_do" {
                continue;
            }

            // Session lookup: if it doesn't exist anymore, the ref is
            // definitely stale. If it does, only clear when it's not
            // running AND its last_activity is outside the grace window
            // (so an in-flight handler doesn't race us).
            let session = match db.get_session(session_id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        card_id = %card.id,
                        session_id = %session_id,
                        "Watchdog: get_session failed during stale-ref sweep: {e}"
                    );
                    continue;
                }
            };
            if let Some(session) = session.as_ref() {
                if session_manager.is_running(session_id).await {
                    continue;
                }
                if let Some(secs) = seconds_since(&session.last_activity)
                    && secs < ORPHAN_GRACE_SECS
                {
                    continue;
                }
            }

            // Per-session lock: skip if a handler is mid-flight.
            let _guard = match session_manager.try_lock_session(session_id).await {
                Some(g) => g,
                None => {
                    tracing::debug!(
                        card_id = %card.id,
                        session_id = %session_id,
                        "Watchdog: stale-ref sweep skipping locked session"
                    );
                    continue;
                }
            };
            // Re-check under the lock — a handler may have flipped state.
            if session_manager.is_running(session_id).await {
                continue;
            }

            match db.clear_card_worker_if_matches(&card.id, session_id).await {
                Ok(Some(_)) => {
                    cleared += 1;
                    tracing::warn!(
                        card_id = %card.id,
                        session_id = %session_id,
                        project_id = %project.id,
                        "Watchdog: cleared stale worker_session_id from card \"{}\"",
                        card.title
                    );
                }
                Ok(None) => {
                    // A concurrent spawn already changed the ref; nothing to do.
                }
                Err(e) => {
                    tracing::error!(
                        card_id = %card.id,
                        session_id = %session_id,
                        "Watchdog: clear_card_worker_if_matches failed: {e}"
                    );
                }
            }
        }
    }

    if cleared > 0 {
        tracing::info!("Watchdog: cleared {cleared} stale card worker ref(s)");
    }
}

/// Scan all worker sessions and remove those whose cards no longer reference
/// them, gated by two safety checks to avoid racing live handlers:
///
/// 1. **Grace period**: sessions whose `last_activity` is within
///    `ORPHAN_GRACE_SECS` are skipped. This covers the window between
///    `handle_worker_done` clearing the card's `worker_session_id` and
///    finishing its bookkeeping (queue drain, plugin dispatch, etc.) —
///    during which the session would otherwise look orphaned.
///
/// 2. **Per-session lock**: sessions whose `SessionManager` lock cannot
///    be acquired right now are skipped — a `send_or_queue` /
///    `drain_queued` / orchestrator respawn is mid-flight. Sweeping
///    while a handler runs would race on the event log.
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
        // Grace period: skip sessions that were active recently.
        if let Some(secs) = seconds_since(&session.last_activity)
            && secs < ORPHAN_GRACE_SECS
        {
            tracing::debug!(
                session_id = %session.id,
                age_secs = secs,
                "Watchdog: skipping session inside grace window"
            );
            continue;
        }

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

        if !is_orphan {
            continue;
        }

        // Per-session lock: skip if a handler is mid-flight.
        let _guard = match session_manager.try_lock_session(&session.id).await {
            Some(g) => g,
            None => {
                tracing::debug!(
                    session_id = %session.id,
                    "Watchdog: skipping orphan with locked session (handler in flight)"
                );
                continue;
            }
        };

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

    if cleaned > 0 {
        tracing::info!("Watchdog: cleaned up {cleaned} orphaned worker session(s)");
    }
}

/// Seconds since `last_activity` (RFC3339), or None if the timestamp is
/// unparseable. Used by the watchdog grace check.
fn seconds_since(rfc3339: &str) -> Option<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(rfc3339).ok()?;
    Some(
        chrono::Utc::now()
            .signed_duration_since(parsed.with_timezone(&chrono::Utc))
            .num_seconds(),
    )
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
            workflow: "task".into(),
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

    /// Timestamp older than the grace window so the watchdog will sweep it.
    fn old_ts() -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(ORPHAN_GRACE_SECS + 60)).to_rfc3339()
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
            created_at: ts,
            last_activity: old_ts(),
            ..Default::default()
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
            ..Default::default()
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
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
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

        // Create two worker sessions (both with old last_activity so the
        // grace check doesn't preserve them in this orphan-sweep test).
        let old = old_ts();
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
            last_activity: old.clone(),
            ..Default::default()
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
            last_activity: old,
            ..Default::default()
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
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
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

    /// Make a card whose `worker_session_id` points at the given session.
    /// Helper for the stale-card-ref tests; pairs with `seed_session`.
    async fn seed_card_with_worker(db: &Db, card_id: &str, session_id: Option<&str>) {
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_card(NewCard {
            id: card_id.into(),
            project_id: "p1".into(),
            title: format!("card-{card_id}"),
            description: "".into(),
            step: "in_progress".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts,
        })
        .await
        .unwrap();
        if let Some(sid) = session_id {
            db.update_card(
                card_id,
                UpdateCard {
                    worker_session_id: Some(Some(sid.into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
    }

    async fn seed_session(db: &Db, session_id: &str, last_activity: &str) {
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: session_id.into(),
            name: format!("worker-{session_id}"),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: None,
            conversation_id: None,
            created_at: ts,
            last_activity: last_activity.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    /// A session that exists, isn't running, but had recent activity is
    /// still inside the grace window — a completion handler may be mid-
    /// flight. Don't clear yet.
    #[tokio::test]
    async fn stale_card_ref_inside_grace_window_is_preserved() {
        let db = setup().await;
        let sm = SessionManager::new(std::sync::Arc::new(
            crate::provider::registry::ProviderRegistry::new(),
        ));
        let fresh_ts = chrono::Utc::now().to_rfc3339();
        seed_session(&db, "ws-fresh", &fresh_ts).await;
        seed_card_with_worker(&db, "c1", Some("ws-fresh")).await;

        sweep_stale_card_refs(&db, &sm).await;

        let card = db.get_card("c1").await.unwrap().unwrap();
        assert_eq!(
            card.worker_session_id.as_deref(),
            Some("ws-fresh"),
            "ref inside grace must be preserved"
        );
    }

    /// A session that exists, isn't running, and last touched long ago
    /// is dead by any reasonable definition. Clear it.
    #[tokio::test]
    async fn stale_card_ref_outside_grace_window_is_cleared() {
        let db = setup().await;
        let sm = SessionManager::new(std::sync::Arc::new(
            crate::provider::registry::ProviderRegistry::new(),
        ));
        let stale_ts = old_ts();
        seed_session(&db, "ws-old", &stale_ts).await;
        seed_card_with_worker(&db, "c1", Some("ws-old")).await;

        sweep_stale_card_refs(&db, &sm).await;

        let card = db.get_card("c1").await.unwrap().unwrap();
        assert!(
            card.worker_session_id.is_none(),
            "stale ref must be cleared"
        );
        assert_eq!(card.last_worker_session_id.as_deref(), Some("ws-old"));
    }

    /// Terminal cards never run a worker, so don't bother touching them
    /// — even if their `worker_session_id` happens to point at a stale
    /// session that's otherwise sweep-eligible.
    #[tokio::test]
    async fn stale_card_ref_terminal_cards_are_skipped() {
        let db = setup().await;
        let sm = SessionManager::new(std::sync::Arc::new(
            crate::provider::registry::ProviderRegistry::new(),
        ));
        let stale_ts = old_ts();
        seed_session(&db, "ws-old", &stale_ts).await;
        seed_card_with_worker(&db, "c1", Some("ws-old")).await;
        db.update_card(
            "c1",
            UpdateCard {
                step: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        sweep_stale_card_refs(&db, &sm).await;

        // Even though the session is stale, the card is terminal so
        // the sweep leaves it alone.
        let card = db.get_card("c1").await.unwrap().unwrap();
        assert_eq!(card.worker_session_id.as_deref(), Some("ws-old"));
    }

    /// If the per-session lock is held — i.e. the completion listener,
    /// `send_or_queue`, or the orchestrator respawn is mid-flight — the
    /// sweep MUST skip the card. Otherwise we'd race the live handler.
    #[tokio::test]
    async fn stale_card_ref_skips_locked_session() {
        let db = setup().await;
        let sm = SessionManager::new(std::sync::Arc::new(
            crate::provider::registry::ProviderRegistry::new(),
        ));
        let stale_ts = old_ts();
        seed_session(&db, "ws-locked", &stale_ts).await;
        seed_card_with_worker(&db, "c1", Some("ws-locked")).await;

        let lock_held = sm.lock_session("ws-locked").await;

        sweep_stale_card_refs(&db, &sm).await;

        let card = db.get_card("c1").await.unwrap().unwrap();
        // Lock blocks the sweep; the ref must still be in place.
        assert_eq!(card.worker_session_id.as_deref(), Some("ws-locked"));

        drop(lock_held);
        // After releasing the lock the next sweep clears it.
        sweep_stale_card_refs(&db, &sm).await;
        let card = db.get_card("c1").await.unwrap().unwrap();
        assert!(card.worker_session_id.is_none());
    }
}
