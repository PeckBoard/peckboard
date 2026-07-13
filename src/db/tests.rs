#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::db::models::*;

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn test_db() -> Db {
        Db::in_memory().expect("failed to create test db")
    }

    // ── Folders ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_folder_crud() {
        let db = test_db();
        let ts = now();

        let folder = db
            .create_folder(NewFolder {
                id: "f1".into(),
                name: "Test Folder".into(),
                path: "/tmp/test".into(),
                created_at: ts.clone(),
            })
            .await
            .unwrap();

        assert_eq!(folder.name, "Test Folder");
        assert_eq!(folder.path, "/tmp/test");

        let found = db.get_folder("f1").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Test Folder");

        let all = db.list_folders().await.unwrap();
        assert_eq!(all.len(), 1);

        assert!(db.delete_folder("f1").await.unwrap());
        assert!(db.get_folder("f1").await.unwrap().is_none());
    }

    // ── Sessions ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_session_crud() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        let session = db
            .create_session(NewSession {
                id: "s1".into(),
                name: "Session 1".into(),
                folder_id: "f1".into(),
                model: Some("opus".into()),
                effort: None,
                is_worker: false,
                project_id: None,
                card_id: None,
                conversation_id: None,
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(session.name, "Session 1");
        assert_eq!(session.model, Some("opus".into()));

        let updated = db
            .update_session(
                "s1",
                UpdateSession {
                    name: Some("Renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.unwrap().name, "Renamed");

        let by_folder = db.list_sessions_by_folder("f1").await.unwrap();
        assert_eq!(by_folder.len(), 1);

        assert!(db.delete_session("s1").await.unwrap());
    }

    #[tokio::test]
    async fn test_expert_sessions() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        db.create_project(NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();

        // A plain session — must never show up in expert lists.
        db.create_session(NewSession {
            id: "plain".into(),
            name: "Chat".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

        // A project-scoped knowledge expert.
        let expert = db
            .create_session(NewSession {
                id: "exp-p1".into(),
                name: "Auth expert".into(),
                folder_id: "f1".into(),
                project_id: Some("p1".into()),
                is_expert: true,
                expert_kind: Some("knowledge".into()),
                knowledge_summary: Some("Knows the auth layer".into()),
                knowledge_area: Some("authentication".into()),
                scope_path: Some("src/auth".into()),
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();

        // All new fields round-trip.
        assert!(expert.is_expert);
        assert_eq!(expert.expert_kind.as_deref(), Some("knowledge"));
        assert_eq!(
            expert.knowledge_summary.as_deref(),
            Some("Knows the auth layer")
        );
        assert_eq!(expert.knowledge_area.as_deref(), Some("authentication"));
        assert_eq!(expert.scope_path.as_deref(), Some("src/auth"));
        assert!(!expert.is_permanent);

        // A global (project_id NULL) permanent question-expert.
        db.create_session(NewSession {
            id: "exp-global".into(),
            name: "Question expert".into(),
            folder_id: "f1".into(),
            project_id: None,
            is_expert: true,
            expert_kind: Some("question".into()),
            is_permanent: true,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

        // Plain list excludes both experts.
        let plain = db.list_plain_sessions().await.unwrap();
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].id, "plain");
        let plain_by_folder = db.list_plain_sessions_by_folder("f1").await.unwrap();
        assert_eq!(plain_by_folder.len(), 1);
        assert_eq!(plain_by_folder[0].id, "plain");
    }

    // ── Projects ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_project_crud() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        let project = db
            .create_project(NewProject {
                id: "p1".into(),
                name: "My Project".into(),
                context: "context".into(),
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
                budget_usd_cents: None,
                budget_period: None,
                worktree_isolation: false,
            })
            .await
            .unwrap();

        assert_eq!(project.name, "My Project");
        assert_eq!(project.worker_count, 2);

        let updated = db
            .update_project(
                "p1",
                UpdateProject {
                    status: Some("archived".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.unwrap().status, "archived");

        let by_folder = db.list_projects_by_folder("f1").await.unwrap();
        assert_eq!(by_folder.len(), 1);

        assert!(db.delete_project("p1").await.unwrap());
    }

    // ── Cards ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_card_crud() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        db.create_project(NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();

        let card = db
            .create_card(NewCard {
                id: "c1".into(),
                project_id: "p1".into(),
                title: "Fix bug".into(),
                description: "It's broken".into(),
                step: "backlog".into(),
                priority: 1,
                workflow: "task".into(),
                model: None,
                effort: None,
                blocked: false,
                block_reason: None,
                created_at: ts.clone(),
                updated_at: ts.clone(),
                system_prompt_name: None,
            })
            .await
            .unwrap();

        assert_eq!(card.title, "Fix bug");
        assert_eq!(card.step, "backlog");

        let updated = db
            .update_card(
                "c1",
                UpdateCard {
                    step: Some("in_progress".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.unwrap().step, "in_progress");

        let by_project = db.list_cards_by_project("p1").await.unwrap();
        assert_eq!(by_project.len(), 1);

        assert!(db.delete_card("c1").await.unwrap());
    }

    /// The worker-claim must be atomic: only one of two competing claims
    /// can land, and a claim on an already-assigned card must fail. This
    /// is the guard that prevents the orchestrator's concurrent spawn
    /// paths from assigning two workers to one card.
    #[tokio::test]
    async fn test_claim_card_for_worker_is_exclusive() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();
        for wid in ["w1", "w2", "w3"] {
            db.create_session(NewSession {
                id: wid.into(),
                name: format!("worker {wid}"),
                folder_id: "f1".into(),
                is_worker: true,
                project_id: Some("p1".into()),
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }

        db.create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Race me".into(),
            description: "".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            system_prompt_name: None,
        })
        .await
        .unwrap();

        // First claim wins and applies the step advance.
        let first = db
            .claim_card_for_worker("c1", "w1", Some("in_progress".into()), &ts)
            .await
            .unwrap();
        assert!(first);
        let card = db.get_card("c1").await.unwrap().unwrap();
        assert_eq!(card.worker_session_id.as_deref(), Some("w1"));
        assert_eq!(card.last_worker_session_id.as_deref(), Some("w1"));
        assert_eq!(card.step, "in_progress");

        // Second claim loses and changes nothing.
        let second = db
            .claim_card_for_worker("c1", "w2", Some("research".into()), &ts)
            .await
            .unwrap();
        assert!(!second);
        let card = db.get_card("c1").await.unwrap().unwrap();
        assert_eq!(card.worker_session_id.as_deref(), Some("w1"));
        assert_eq!(card.step, "in_progress");

        // After release (worker done / spawn rollback), the card is
        // claimable again.
        db.update_card(
            "c1",
            UpdateCard {
                worker_session_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let reclaimed = db
            .claim_card_for_worker("c1", "w3", None, &ts)
            .await
            .unwrap();
        assert!(reclaimed);
    }

    // ── Card dependencies ────────────────────────────────────────────

    #[tokio::test]
    async fn test_card_dependencies() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();

        let mk = |id: &str| NewCard {
            id: id.into(),
            project_id: "p1".into(),
            title: id.into(),
            description: "".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            system_prompt_name: None,
        };
        db.create_card(mk("a")).await.unwrap();
        db.create_card(mk("b")).await.unwrap();
        db.create_card(mk("c")).await.unwrap();

        // c depends on a and b; self-edge is dropped.
        db.set_card_dependencies("c", vec!["a".into(), "b".into(), "c".into()])
            .await
            .unwrap();
        let mut deps = db.list_card_dependencies("c").await.unwrap();
        deps.sort();
        assert_eq!(deps, vec!["a".to_string(), "b".to_string()]);

        // Replacing the set removes the old edges.
        db.set_card_dependencies("c", vec!["a".into()])
            .await
            .unwrap();
        assert_eq!(db.list_card_dependencies("c").await.unwrap(), vec!["a"]);

        // Project-wide edge listing.
        let edges = db.list_dependencies_by_project("p1").await.unwrap();
        assert_eq!(edges, vec![("c".to_string(), "a".to_string())]);

        // Deleting a prerequisite cascades its edges away (FK ON DELETE
        // CASCADE), so the dependent is no longer stranded.
        assert!(db.delete_card("a").await.unwrap());
        assert!(db.list_card_dependencies("c").await.unwrap().is_empty());
        assert!(
            db.list_dependencies_by_project("p1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    // ── Events ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_event_crud() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        db.create_session(NewSession {
            id: "s1".into(),
            name: "Session".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

        db.create_event(NewEvent {
            id: "e1".into(),
            session_id: "s1".into(),
            seq: 1,
            ts: 1000,
            kind: "message".into(),
            data: r#"{"text":"hello"}"#.into(),
        })
        .await
        .unwrap();

        db.create_event(NewEvent {
            id: "e2".into(),
            session_id: "s1".into(),
            seq: 2,
            ts: 2000,
            kind: "tool_use".into(),
            data: "{}".into(),
        })
        .await
        .unwrap();

        let all = db.list_events_by_session("s1", None).await.unwrap();
        assert_eq!(all.len(), 2);

        let after = db.list_events_by_session("s1", Some(1)).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].seq, 2);

        let deleted = db.delete_events_by_session("s1").await.unwrap();
        assert_eq!(deleted, 2);
    }

    // ── Users ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_user_crud() {
        let db = test_db();
        let ts = now();

        let user = db
            .create_user(NewUser {
                id: "u1".into(),
                username: "admin".into(),
                email: Some("admin@test.com".into()),
                password_hash: "hash123".into(),
                role: "admin".into(),
                created_at: ts.clone(),
                updated_at: ts.clone(),
            })
            .await
            .unwrap();

        assert_eq!(user.username, "admin");

        let by_name = db.get_user_by_username("admin").await.unwrap();
        assert!(by_name.is_some());

        let updated = db
            .update_user(
                "u1",
                UpdateUser {
                    email: Some(Some("new@test.com".into())),
                    updated_at: Some(now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.unwrap().email, Some("new@test.com".into()));

        let count = db.count_users().await.unwrap();
        assert_eq!(count, 1);

        let all = db.list_users().await.unwrap();
        assert_eq!(all.len(), 1);

        assert!(db.delete_user("u1").await.unwrap());
        assert_eq!(db.count_users().await.unwrap(), 0);
    }

    // ── Auth Sessions ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_auth_session_crud() {
        let db = test_db();
        let ts = now();

        db.create_user(NewUser {
            id: "u1".into(),
            username: "user".into(),
            email: None,
            password_hash: "hash".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

        let auth = db
            .create_auth_session(NewAuthSession {
                id: "as1".into(),
                user_id: "u1".into(),
                token_hash: "tokenhash".into(),
                created_at: 1000,
                expires_at: 9999,
                user_agent: Some("test-agent".into()),
                ip_address: None,
            })
            .await
            .unwrap();

        assert_eq!(auth.user_id, "u1");
        assert!(auth.last_used_at.is_none());

        assert!(db.update_auth_session_last_used("as1", 5000).await.unwrap());

        let found = db.get_auth_session("as1").await.unwrap().unwrap();
        assert_eq!(found.last_used_at, Some(5000));

        // Test expiry cleanup
        let expired_count = db.delete_expired_auth_sessions(10000).await.unwrap();
        assert_eq!(expired_count, 1);
        assert!(db.get_auth_session("as1").await.unwrap().is_none());
    }

    // ── Push Subscriptions ───────────────────────────────────────────

    #[tokio::test]
    async fn test_push_subscription_crud() {
        let db = test_db();
        let ts = now();

        db.create_push_subscription(NewPushSubscription {
            endpoint: "https://push.example.com/sub1".into(),
            p256dh: "key1".into(),
            auth_key: "auth1".into(),
            created_at: ts,
        })
        .await
        .unwrap();

        let all = db.list_push_subscriptions().await.unwrap();
        assert_eq!(all.len(), 1);

        assert!(
            db.delete_push_subscription("https://push.example.com/sub1")
                .await
                .unwrap()
        );
        assert_eq!(db.list_push_subscriptions().await.unwrap().len(), 0);
    }

    // ── Queued Messages ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_queued_message_crud() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        db.create_session(NewSession {
            id: "s1".into(),
            name: "S".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

        db.upsert_queued_message(NewQueuedMessage {
            session_id: "s1".into(),
            text: "hello".into(),
            queued_at: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

        let msg = db.get_queued_message("s1").await.unwrap().unwrap();
        assert_eq!(msg.text, "hello");

        // Upsert should replace
        db.upsert_queued_message(NewQueuedMessage {
            session_id: "s1".into(),
            text: "updated".into(),
            queued_at: now(),
            model: Some("mock:echo".into()),
            effort: Some("medium".into()),
        })
        .await
        .unwrap();

        let msg = db.get_queued_message("s1").await.unwrap().unwrap();
        assert_eq!(msg.text, "updated");
        assert_eq!(msg.model.as_deref(), Some("mock:echo"));
        assert_eq!(msg.effort.as_deref(), Some("medium"));

        assert!(db.delete_queued_message("s1").await.unwrap());
        assert!(db.get_queued_message("s1").await.unwrap().is_none());
    }

    /// Bulk-delete every queued message belonging to a worker on the
    /// given project. Used by the pause flow to ensure a cancel's
    /// completion listener can't drain a buffered message into a fresh
    /// agent run on a paused project.
    #[tokio::test]
    async fn test_delete_queued_messages_for_project_scopes_correctly() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        for pid in ["p1", "p2"] {
            db.create_project(NewProject {
                id: pid.into(),
                name: pid.into(),
                context: "".into(),
                folder_id: "f1".into(),
                worker_count: 1,
                status: "active".into(),
                workflow: "task".into(),
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: true,
                worker_communication: false,
                created_at: ts.clone(),
                last_accessed_at: ts.clone(),
                budget_usd_cents: None,
                budget_period: None,
                worktree_isolation: false,
            })
            .await
            .unwrap();
        }

        // Workers on p1 (target) and p2 (kept), plus a plain
        // non-worker session (also kept — pause only affects worker
        // queues).
        let seed = |id: &str, project: Option<&str>, is_worker: bool| {
            let id = id.to_string();
            let project = project.map(|s| s.to_string());
            let ts2 = ts.clone();
            let db_ref = &db;
            async move {
                db_ref
                    .create_session(NewSession {
                        id: id.clone(),
                        name: id.clone(),
                        folder_id: "f1".into(),
                        model: None,
                        effort: None,
                        is_worker,
                        project_id: project,
                        card_id: None,
                        conversation_id: None,
                        created_at: ts2.clone(),
                        last_activity: ts2.clone(),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                db_ref
                    .upsert_queued_message(NewQueuedMessage {
                        session_id: id,
                        text: "msg".into(),
                        queued_at: ts2,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
            }
        };
        seed("w1", Some("p1"), true).await;
        seed("w2", Some("p1"), true).await;
        seed("w3", Some("p2"), true).await;
        seed("plain", None, false).await;

        let deleted = db.delete_queued_messages_for_project("p1").await.unwrap();
        assert_eq!(deleted, 2, "should delete both p1-worker queues");

        assert!(db.get_queued_message("w1").await.unwrap().is_none());
        assert!(db.get_queued_message("w2").await.unwrap().is_none());
        assert!(
            db.get_queued_message("w3").await.unwrap().is_some(),
            "p2 worker preserved"
        );
        assert!(
            db.get_queued_message("plain").await.unwrap().is_some(),
            "plain session preserved"
        );
    }

    /// `clear_card_worker_if_matches` must be a no-op when the card's
    /// worker_session_id doesn't match — that's the load-bearing race
    /// guard: a stale completion listener firing after the orchestrator
    /// already reassigned must NOT clobber the new assignment.
    #[tokio::test]
    async fn test_clear_card_worker_if_matches_is_conditional() {
        let db = test_db();
        let ts = now();
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
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();
        db.create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "T".into(),
            description: "".into(),
            step: "in_progress".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            system_prompt_name: None,
        })
        .await
        .unwrap();
        // FK target: the card's worker_session_id REFERENCES sessions(id),
        // so the replacement-worker row must exist before we point the
        // card at it.
        db.create_session(NewSession {
            id: "new-worker".into(),
            name: "new-worker".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: Some("c1".into()),
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
        db.update_card(
            "c1",
            UpdateCard {
                worker_session_id: Some(Some("new-worker".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Stale completion handler firing for old-worker must NOT clear
        // the new-worker assignment.
        let result = db
            .clear_card_worker_if_matches("c1", "old-worker")
            .await
            .unwrap();
        assert!(result.is_none(), "non-matching clear must be a no-op");
        let card = db.get_card("c1").await.unwrap().unwrap();
        assert_eq!(card.worker_session_id.as_deref(), Some("new-worker"));

        // Matching clear DOES wipe the ref.
        let result = db
            .clear_card_worker_if_matches("c1", "new-worker")
            .await
            .unwrap();
        assert!(result.is_some(), "matching clear must return updated card");
        let card = db.get_card("c1").await.unwrap().unwrap();
        assert!(card.worker_session_id.is_none());
        // And stamps last_worker_session_id for crash-count joins.
        assert_eq!(card.last_worker_session_id.as_deref(), Some("new-worker"));
    }

    // ── Announcements ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_announcement_crud() {
        let db = test_db();
        let ts = now();

        db.create_announcement(NewAnnouncement {
            id: "a1".into(),
            kind: "info".into(),
            title: "Test".into(),
            message: "Hello world".into(),
            detail: None,
            created_at: ts,
        })
        .await
        .unwrap();

        let all = db.list_announcements().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Test");

        assert!(db.delete_announcement("a1").await.unwrap());
        assert_eq!(db.list_announcements().await.unwrap().len(), 0);
    }

    // ── worker-session tabs are first-class ────────────────────────

    #[tokio::test]
    async fn test_user_tabs_includes_worker_sessions() {
        // Worker sessions (`is_worker=true`) are excluded from
        // `list_plain_sessions` (powers GET /api/sessions), but the
        // tab strip still needs to reach them — the user gets to a
        // worker session by clicking "Details" on a kanban card. The
        // tabs API/store must therefore treat worker sessions as
        // legitimate tab targets. Regression test for the bug where
        // the frontend's "drop tabs not in the sessions list"
        // cleanup loop closed worker-session tabs the moment the
        // plain-sessions list loaded.
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_user(NewUser {
            id: "u".into(),
            username: "u".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "worker-s".into(),
            name: "Worker for card A".into(),
            folder_id: "f".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

        // The upsert must accept a worker session (it's a real row
        // in `sessions`; the `is_worker` filter is purely on the
        // listing endpoint).
        let tab = db
            .upsert_user_tab("u", "session", "worker-s")
            .await
            .unwrap();
        assert!(tab.is_some(), "worker-session tab should be accepted");

        // And the underlying session must still be retrievable by id —
        // that's how `GET /api/me/tabs` resolves the denormalized name.
        let session = db.get_session("worker-s").await.unwrap();
        assert_eq!(
            session.map(|s| s.name).as_deref(),
            Some("Worker for card A"),
            "worker session should be reachable by id so /api/me/tabs can render its label"
        );

        // Sanity-check: `list_plain_sessions` excludes the worker (this
        // is the listing the SessionList view uses), but the tab list
        // is independent and still surfaces it.
        assert!(
            db.list_plain_sessions().await.unwrap().is_empty(),
            "worker session must NOT appear in the plain sessions list"
        );
        assert_eq!(
            db.list_user_tabs("u").await.unwrap().len(),
            1,
            "worker-session tab must survive even though plain-sessions list is empty"
        );
    }

    // ── delete_session cascades to user_tabs ──────────────────────

    #[tokio::test]
    async fn test_delete_session_clears_user_tabs() {
        // Tabs are polymorphic (item_type + item_id) so there is no FK
        // cascade. Without an explicit cleanup the frontend tab strip
        // renders an orphan chip labelled "Session" — guard against
        // regression here at the DB layer.
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_user(NewUser {
            id: "u".into(),
            username: "u".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s".into(),
            name: "S".into(),
            folder_id: "f".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        db.upsert_user_tab("u", "session", "s").await.unwrap();
        assert_eq!(db.list_user_tabs("u").await.unwrap().len(), 1);

        assert!(db.delete_session("s").await.unwrap());
        assert!(
            db.list_user_tabs("u").await.unwrap().is_empty(),
            "user_tabs row for the deleted session should be gone"
        );
    }

    // ── repeating-task tabs are a first-class kind ──────────────────

    #[tokio::test]
    async fn test_upsert_user_tab_accepts_repeating_task() {
        // Regression: extending TabType to include repeating_task means
        // the DB-layer existence check has to recognise the new item_type
        // and look the id up in the right table. Without that branch,
        // every repeating-task tab POST would 404 even for a real task.
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_user(NewUser {
            id: "u".into(),
            username: "u".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_repeating_task(NewRepeatingTask {
            id: "rt".into(),
            name: "Nightly sweep".into(),
            description: "".into(),
            folder_id: "f".into(),
            prompt: "do the thing".into(),
            schedule_kind: "interval".into(),
            schedule_value: "{\"minutes\":60}".into(),
            model: None,
            effort: None,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

        let tab = db
            .upsert_user_tab("u", "repeating_task", "rt")
            .await
            .unwrap();
        assert!(
            tab.is_some(),
            "repeating-task tab should be accepted for a real task id"
        );

        // Wrong id → existence check fails → no tab written. The frontend
        // treats this as 404 and rolls back the optimistic insert.
        let missing = db
            .upsert_user_tab("u", "repeating_task", "nope")
            .await
            .unwrap();
        assert!(
            missing.is_none(),
            "repeating-task tab for a missing task must be refused"
        );
    }

    #[tokio::test]
    async fn test_delete_repeating_task_clears_user_tabs() {
        // Mirrors test_delete_session_clears_user_tabs: tabs are
        // polymorphic so there's no FK cascade. delete_repeating_task
        // has to remove every user_tabs row pointing at the deleted
        // task or the strip leaves orphan chips behind.
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_user(NewUser {
            id: "u".into(),
            username: "u".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_repeating_task(NewRepeatingTask {
            id: "rt".into(),
            name: "Nightly sweep".into(),
            description: "".into(),
            folder_id: "f".into(),
            prompt: "do the thing".into(),
            schedule_kind: "interval".into(),
            schedule_value: "{\"minutes\":60}".into(),
            model: None,
            effort: None,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        db.upsert_user_tab("u", "repeating_task", "rt")
            .await
            .unwrap();
        assert_eq!(db.list_user_tabs("u").await.unwrap().len(), 1);

        assert!(db.delete_repeating_task("rt").await.unwrap());
        assert!(
            db.list_user_tabs("u").await.unwrap().is_empty(),
            "user_tabs row for the deleted task should be gone"
        );
    }

    #[tokio::test]
    async fn test_upsert_user_tab_accepts_report_kind() {
        // Reports are file-backed; the DB layer trusts the route to
        // have validated the on-disk path. From the DB's perspective
        // the kind is just another allowed item_type.
        let db = test_db();
        let ts = now();
        db.create_user(NewUser {
            id: "u".into(),
            username: "u".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        let tab = db
            .upsert_user_tab("u", "report", "2026-06-11/sample.md")
            .await
            .unwrap();
        assert!(tab.is_some(), "report tab should be accepted");
        assert_eq!(db.list_user_tabs("u").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_upsert_user_tab_rejects_unknown_kind() {
        // The DB-side existence-check switch is the last line of
        // defense if the route layer's validate_item_type ever
        // regresses. Anything not in the allowed set must be refused.
        let db = test_db();
        let ts = now();
        db.create_user(NewUser {
            id: "u".into(),
            username: "u".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        // The DB layer returns Ok(None) (treated as 404) for unknown
        // kinds rather than letting them be written. Whether the
        // CHECK constraint would also catch it is a defence-in-depth
        // detail; we want the explicit refusal at the existence-check
        // step so the error is consistent with the "missing item" path.
        let res = db.upsert_user_tab("u", "doodad", "anything").await.unwrap();
        assert!(res.is_none(), "unknown item_type must be refused");
    }

    #[tokio::test]
    async fn test_delete_project_clears_user_tabs() {
        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_user(NewUser {
            id: "u".into(),
            username: "u".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p".into(),
            name: "P".into(),
            context: "".into(),
            folder_id: "f".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();
        db.upsert_user_tab("u", "project", "p").await.unwrap();
        assert_eq!(db.list_user_tabs("u").await.unwrap().len(), 1);

        assert!(db.delete_project("p").await.unwrap());
        assert!(db.list_user_tabs("u").await.unwrap().is_empty());
    }

    // ── Todos table: replace-all + load order ──────────────────────

    #[tokio::test]
    async fn test_todos_replace_all_and_ordered_read() {
        use crate::todo::{TodoItem, TodoSnapshot, TodoStatus};

        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "S".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

        // Empty load -> empty vec.
        assert!(db.list_session_todos("s1").await.unwrap().is_empty());

        // First snapshot populates the table.
        let first = TodoSnapshot {
            todos: vec![
                TodoItem {
                    content: "a".into(),
                    status: TodoStatus::InProgress,
                    active_form: Some("Doing a".into()),
                },
                TodoItem {
                    content: "b".into(),
                    status: TodoStatus::Pending,
                    active_form: None,
                },
            ],
        };
        db.replace_session_todos("s1", first).await.unwrap();

        let loaded = db.list_session_todos("s1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "a");
        assert_eq!(loaded[0].status, TodoStatus::InProgress);
        assert_eq!(loaded[0].active_form.as_deref(), Some("Doing a"));
        assert_eq!(loaded[1].content, "b");
        assert_eq!(loaded[1].status, TodoStatus::Pending);

        // Second snapshot REPLACES, doesn't append. The new list is
        // shorter AND reordered AND drops "a" entirely — none of "a"
        // may survive, and the new ordering must hold.
        let second = TodoSnapshot {
            todos: vec![
                TodoItem {
                    content: "c".into(),
                    status: TodoStatus::Done,
                    active_form: None,
                },
                TodoItem {
                    content: "b".into(),
                    status: TodoStatus::InProgress,
                    active_form: Some("Doing b".into()),
                },
            ],
        };
        db.replace_session_todos("s1", second).await.unwrap();

        let loaded = db.list_session_todos("s1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "c");
        assert_eq!(loaded[0].status, TodoStatus::Done);
        assert_eq!(loaded[1].content, "b");
        assert_eq!(loaded[1].status, TodoStatus::InProgress);
        assert!(loaded.iter().all(|t| t.content != "a"));

        // Empty snapshot clears the list.
        db.replace_session_todos("s1", TodoSnapshot::default())
            .await
            .unwrap();
        assert!(db.list_session_todos("s1").await.unwrap().is_empty());
    }

    // ── Project todos aggregation ─────────────────────────────────

    #[tokio::test]
    async fn test_list_project_todos_aggregates_across_cards_and_fallback_session() {
        use crate::todo::{TodoItem, TodoSnapshot, TodoStatus};

        let db = test_db();
        let ts = now();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f-pt".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        db.create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();

        // Two cards each with their own worker session. Card "a" uses the
        // active `worker_session_id`; card "b" exercises the
        // `last_worker_session_id` fallback (the orchestrator clears the
        // active one between dispatches but we still want its last snapshot
        // to roll up). A third card "c" has no session at all and must be
        // omitted, and a fourth card "d" has a session that never reported
        // any todos and must also be omitted.
        for cid in ["a", "b", "c", "d"] {
            db.create_card(NewCard {
                id: cid.into(),
                project_id: "p1".into(),
                title: format!("Card {cid}"),
                description: "".into(),
                step: "backlog".into(),
                // Same priority for stable order; the call orders by
                // priority asc and ties fall through to insertion order.
                priority: 1,
                workflow: "task".into(),
                model: None,
                effort: None,
                blocked: false,
                block_reason: None,
                created_at: ts.clone(),
                updated_at: ts.clone(),
                system_prompt_name: None,
            })
            .await
            .unwrap();
        }

        for sid in ["s-a", "s-b", "s-d"] {
            db.create_session(NewSession {
                id: sid.into(),
                name: sid.into(),
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
        }

        db.update_card(
            "a",
            UpdateCard {
                worker_session_id: Some(Some("s-a".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        db.update_card(
            "b",
            UpdateCard {
                // Active is cleared; only the last_* fallback is set.
                last_worker_session_id: Some(Some("s-b".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        db.update_card(
            "d",
            UpdateCard {
                worker_session_id: Some(Some("s-d".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        db.replace_session_todos(
            "s-a",
            TodoSnapshot {
                todos: vec![TodoItem {
                    content: "todo from a".into(),
                    status: TodoStatus::InProgress,
                    active_form: Some("Doing a".into()),
                }],
            },
        )
        .await
        .unwrap();
        db.replace_session_todos(
            "s-b",
            TodoSnapshot {
                todos: vec![
                    TodoItem {
                        content: "todo from b 1".into(),
                        status: TodoStatus::Pending,
                        active_form: None,
                    },
                    TodoItem {
                        content: "todo from b 2".into(),
                        status: TodoStatus::Done,
                        active_form: None,
                    },
                ],
            },
        )
        .await
        .unwrap();
        // s-d intentionally has no todos.

        let groups = db.list_project_todos("p1").await.unwrap();
        let by_card: std::collections::HashMap<String, Vec<String>> = groups
            .iter()
            .map(|g| {
                (
                    g.card_id.clone(),
                    g.todos.iter().map(|t| t.content.clone()).collect(),
                )
            })
            .collect();

        assert_eq!(by_card.len(), 2);
        assert_eq!(by_card.get("a").unwrap(), &vec!["todo from a".to_string()]);
        assert_eq!(
            by_card.get("b").unwrap(),
            &vec!["todo from b 1".to_string(), "todo from b 2".to_string()]
        );
        assert!(!by_card.contains_key("c"));
        assert!(!by_card.contains_key("d"));

        // Card titles are denormalized for the frontend group labels.
        let a = groups.iter().find(|g| g.card_id == "a").unwrap();
        assert_eq!(a.card_title, "Card a");
    }

    #[tokio::test]
    async fn test_upsert_user_tab_rejects_unknown_item() {
        // Tabs are polymorphic so we can't lean on FKs — upsert must
        // refuse unknown item_ids itself, otherwise stale URLs and
        // cross-device delete races silently write orphan rows that
        // render as phantom "Session" chips.
        let db = test_db();
        let ts = now();

        db.create_user(NewUser {
            id: "u".into(),
            username: "u".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

        assert!(
            db.upsert_user_tab("u", "session", "missing")
                .await
                .unwrap()
                .is_none(),
            "upserting a tab for a non-existent session must return None"
        );
        assert!(
            db.upsert_user_tab("u", "project", "missing")
                .await
                .unwrap()
                .is_none(),
            "upserting a tab for a non-existent project must return None"
        );
        assert!(
            db.list_user_tabs("u").await.unwrap().is_empty(),
            "no row should have been written for either rejected upsert"
        );
    }

    // ── Delete non-existent returns false ─────────────────────────

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let db = test_db();

        assert!(!db.delete_folder("nonexistent").await.unwrap());
        assert!(!db.delete_session("nonexistent").await.unwrap());
        assert!(!db.delete_project("nonexistent").await.unwrap());
        assert!(!db.delete_card("nonexistent").await.unwrap());
        assert!(!db.delete_user("nonexistent").await.unwrap());
        assert!(!db.delete_auth_session("nonexistent").await.unwrap());
        assert!(!db.delete_push_subscription("nonexistent").await.unwrap());
        assert!(!db.delete_queued_message("nonexistent").await.unwrap());
        assert!(!db.delete_announcement("nonexistent").await.unwrap());
    }

    // ── completed_at stamping on step transitions ────────────────────

    /// Moving a card into the `done` step must stamp `completed_at`;
    /// moving back out must clear it. Two cards finished at distinct
    /// times must therefore be distinguishable for the Done-column
    /// "newest first" sort.
    #[tokio::test]
    async fn test_completed_at_stamped_on_done_transition() {
        let db = test_db();
        let ts = now();
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
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();

        for cid in ["older", "newer"] {
            db.create_card(NewCard {
                id: cid.into(),
                project_id: "p1".into(),
                title: cid.into(),
                description: "".into(),
                step: "in_progress".into(),
                priority: 1,
                workflow: "task".into(),
                model: None,
                effort: None,
                blocked: false,
                block_reason: None,
                created_at: ts.clone(),
                updated_at: ts.clone(),
                system_prompt_name: None,
            })
            .await
            .unwrap();
        }

        // Finish "older" first, sleep so the rfc3339 timestamps sort
        // unambiguously, then finish "newer".
        let older = db
            .update_card(
                "older",
                UpdateCard {
                    step: Some("done".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            older.completed_at.is_some(),
            "completed_at must stamp on done"
        );

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let newer = db
            .update_card(
                "newer",
                UpdateCard {
                    step: Some("done".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(newer.completed_at.is_some());
        assert!(
            newer.completed_at.as_deref().unwrap() > older.completed_at.as_deref().unwrap(),
            "later transition must produce a later timestamp"
        );

        // Sorting the project's `done` cards by completed_at DESC must
        // put the more recently finished one first — the property the
        // Kanban "Done" column relies on.
        let mut done: Vec<_> = db
            .list_cards_by_project("p1")
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.step == "done")
            .collect();
        done.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));
        assert_eq!(done[0].id, "newer");
        assert_eq!(done[1].id, "older");

        // Reopening must clear completed_at so a future re-finish gets
        // a fresh timestamp instead of inheriting the original one.
        let reopened = db
            .update_card(
                "older",
                UpdateCard {
                    step: Some("in_progress".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            reopened.completed_at.is_none(),
            "leaving done must clear completed_at"
        );

        // Updating an already-done card without changing step must NOT
        // re-stamp completed_at — otherwise priority edits would
        // shuffle the Done order spuriously.
        let same = db
            .update_card(
                "newer",
                UpdateCard {
                    priority: Some(2),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(same.completed_at, newer.completed_at);
    }

    // ── Pagination ────────────────────────────────────────────────────

    async fn seed_paginated_sessions(db: &Db, folder: &str, count: usize) -> Vec<Session> {
        let ts_base = chrono::Utc::now();
        db.create_folder(NewFolder {
            id: folder.into(),
            name: folder.into(),
            path: format!("/tmp/{folder}"),
            created_at: ts_base.to_rfc3339(),
        })
        .await
        .unwrap();
        let mut sessions = Vec::with_capacity(count);
        // Insert in reverse age order — session 0 ends up with the OLDEST
        // last_activity, session N-1 with the NEWEST — so the expected
        // page order is just the reversed insertion order, which makes
        // each assertion below easy to reason about.
        for i in 0..count {
            let ts = (ts_base - chrono::Duration::seconds((count - i) as i64)).to_rfc3339();
            let s = db
                .create_session(NewSession {
                    id: format!("s{i:03}"),
                    name: format!("Session {i}"),
                    folder_id: folder.into(),
                    model: None,
                    effort: None,
                    is_worker: false,
                    project_id: None,
                    card_id: None,
                    conversation_id: None,
                    created_at: ts.clone(),
                    last_activity: ts,
                    ..Default::default()
                })
                .await
                .unwrap();
            sessions.push(s);
        }
        sessions
    }

    #[tokio::test]
    async fn list_plain_sessions_page_returns_newest_first_and_respects_limit() {
        let db = test_db();
        seed_paginated_sessions(&db, "f1", 5).await;

        let page = db.list_plain_sessions_page(None, 3).await.unwrap();
        assert_eq!(page.len(), 3, "limit must cap row count");
        // Newest first: we inserted s000 oldest, s004 newest, so the
        // first page is s004, s003, s002.
        let ids: Vec<&str> = page.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s004", "s003", "s002"]);
    }

    #[tokio::test]
    async fn list_plain_sessions_page_walks_full_history_with_cursor() {
        let db = test_db();
        seed_paginated_sessions(&db, "f1", 7).await;

        // Page 1
        let p1 = db.list_plain_sessions_page(None, 3).await.unwrap();
        let last = p1.last().unwrap();
        let cursor = (last.last_activity.clone(), last.id.clone());

        // Page 2 — must start with the row immediately older than the
        // page-1 tail and continue in strict order.
        let p2 = db.list_plain_sessions_page(Some(cursor), 3).await.unwrap();
        let p2_ids: Vec<&str> = p2.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(p2_ids, vec!["s003", "s002", "s001"]);

        // Page 3 — fewer than `limit` rows confirms end-of-list.
        let last2 = p2.last().unwrap();
        let cursor2 = (last2.last_activity.clone(), last2.id.clone());
        let p3 = db.list_plain_sessions_page(Some(cursor2), 3).await.unwrap();
        assert_eq!(
            p3.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["s000"],
        );
    }

    #[tokio::test]
    async fn list_plain_sessions_page_breaks_ties_by_id() {
        // Two sessions with the same last_activity. The keyset cursor
        // must still walk past them deterministically — otherwise the
        // second page could either repeat or skip the duplicate.
        let db = test_db();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp/f1".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        for id in ["a", "b", "c"] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                model: None,
                effort: None,
                is_worker: false,
                project_id: None,
                card_id: None,
                conversation_id: None,
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }

        let p1 = db.list_plain_sessions_page(None, 2).await.unwrap();
        assert_eq!(p1.len(), 2);
        let cursor = (p1[1].last_activity.clone(), p1[1].id.clone());
        let p2 = db.list_plain_sessions_page(Some(cursor), 2).await.unwrap();
        assert_eq!(p2.len(), 1);

        // Together the pages must cover every row exactly once.
        let mut all: Vec<&str> = p1.iter().chain(p2.iter()).map(|s| s.id.as_str()).collect();
        all.sort();
        assert_eq!(all, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn list_plain_sessions_page_skips_workers_and_experts() {
        let db = test_db();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp/f1".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        // A plain session that should appear, plus one worker + one
        // expert that should not — defence-in-depth against a future
        // edit that drops the filter.
        for (id, is_worker, is_expert) in [
            ("plain", false, false),
            ("worker", true, false),
            ("expert", false, true),
        ] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                model: None,
                effort: None,
                is_worker,
                project_id: None,
                card_id: None,
                conversation_id: None,
                created_at: ts.clone(),
                last_activity: ts.clone(),
                is_expert,
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let page = db.list_plain_sessions_page(None, 10).await.unwrap();
        let ids: Vec<&str> = page.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["plain"]);
    }

    #[tokio::test]
    async fn list_plain_sessions_by_folder_page_scopes_correctly() {
        let db = test_db();
        let ts = chrono::Utc::now().to_rfc3339();
        for folder in ["fa", "fb"] {
            db.create_folder(NewFolder {
                id: folder.into(),
                name: folder.into(),
                path: format!("/tmp/{folder}"),
                created_at: ts.clone(),
            })
            .await
            .unwrap();
            for i in 0..3 {
                db.create_session(NewSession {
                    id: format!("{folder}-{i}"),
                    name: format!("{folder} {i}"),
                    folder_id: folder.into(),
                    model: None,
                    effort: None,
                    is_worker: false,
                    project_id: None,
                    card_id: None,
                    conversation_id: None,
                    created_at: ts.clone(),
                    last_activity: ts.clone(),
                    ..Default::default()
                })
                .await
                .unwrap();
            }
        }
        let page = db
            .list_plain_sessions_by_folder_page("fa", None, 10)
            .await
            .unwrap();
        let ids: Vec<String> = page.iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids.len(), 3);
        for id in ids {
            assert!(id.starts_with("fa-"), "folder filter leaked: {id}");
        }
    }

    #[tokio::test]
    async fn list_events_by_session_before_returns_oldest_first_window() {
        let db = test_db();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp/f1".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "s1".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        // Seed 10 events with sequential seqs (append_event picks
        // them automatically).
        for i in 0..10 {
            db.append_event(
                "s1",
                "user",
                serde_json::json!({ "text": format!("msg{i}") }),
            )
            .await
            .unwrap();
        }

        // Default fetch (before_seq=None) returns the LATEST 4 in
        // ascending order — the chat view's first page.
        let latest = db
            .list_events_by_session_before("s1", None, 4)
            .await
            .unwrap();
        let latest_seqs: Vec<i32> = latest.iter().map(|e| e.seq).collect();
        assert_eq!(latest_seqs, vec![7, 8, 9, 10]);

        // Walk older: ask for events strictly before seq 7.
        let older = db
            .list_events_by_session_before("s1", Some(7), 4)
            .await
            .unwrap();
        let older_seqs: Vec<i32> = older.iter().map(|e| e.seq).collect();
        assert_eq!(older_seqs, vec![3, 4, 5, 6]);

        // Short page when fewer than `limit` events remain — the
        // chat view uses this to hide the "Load older" button.
        let oldest = db
            .list_events_by_session_before("s1", Some(3), 10)
            .await
            .unwrap();
        let oldest_seqs: Vec<i32> = oldest.iter().map(|e| e.seq).collect();
        assert_eq!(oldest_seqs, vec![1, 2]);
    }

    #[tokio::test]
    async fn test_project_workflow_instructions_crud() {
        let db = test_db();
        let ts = now();
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
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "fast-develop-software".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();

        // Nothing set yet.
        assert!(
            db.list_project_workflow_instructions("p1")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.get_project_workflow_instruction("p1", "fast-develop-software", "in_progress")
                .await
                .unwrap()
                .is_none()
        );

        // Insert.
        let row = db
            .upsert_project_workflow_instruction(
                "p1",
                "fast-develop-software",
                "in_progress",
                "Commit to master and push.",
            )
            .await
            .unwrap();
        assert!(row.is_some());
        assert_eq!(
            db.get_project_workflow_instruction("p1", "fast-develop-software", "in_progress")
                .await
                .unwrap()
                .as_deref(),
            Some("Commit to master and push."),
        );

        // Update.
        db.upsert_project_workflow_instruction(
            "p1",
            "fast-develop-software",
            "in_progress",
            "Push to staging instead.",
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_project_workflow_instruction("p1", "fast-develop-software", "in_progress")
                .await
                .unwrap()
                .as_deref(),
            Some("Push to staging instead."),
        );

        // Empty input clears the row rather than storing whitespace.
        let cleared = db
            .upsert_project_workflow_instruction(
                "p1",
                "fast-develop-software",
                "in_progress",
                "  \n\t  ",
            )
            .await
            .unwrap();
        assert!(cleared.is_none());
        assert!(
            db.get_project_workflow_instruction("p1", "fast-develop-software", "in_progress")
                .await
                .unwrap()
                .is_none()
        );

        // Cascade on project delete.
        db.upsert_project_workflow_instruction(
            "p1",
            "fast-develop-software",
            "in_progress",
            "Anything.",
        )
        .await
        .unwrap();
        db.delete_project_cascade("p1").await.unwrap();
        // After delete the table should be empty for that project — the
        // FK CASCADE drops the row.
        assert!(
            db.list_project_workflow_instructions("p1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    // ── Usage events ─────────────────────────────────────────────────

    /// Standard folder → project → card → session chain so usage rows
    /// have a valid FK target (and project/card attribution is derivable
    /// via the session row, not denormalized onto usage_events).
    async fn seed_usage_session(db: &Db, session_id: &str) {
        let ts = now();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();
        db.create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Card".into(),
            description: "".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            system_prompt_name: None,
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: session_id.into(),
            name: "Session".into(),
            folder_id: "f1".into(),
            project_id: Some("p1".into()),
            card_id: Some("c1".into()),
            is_worker: true,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    /// The real capture path: a `ProviderEvent::Usage` flowing through
    /// `emit_event` must land both as an `agent-usage` event AND a mirrored
    /// `usage_events` row linked back to that event, with turn_seq assigned.
    #[tokio::test]
    async fn usage_events_captured_via_emit_event() {
        use crate::provider::agent::emit_event;
        use crate::provider::stream::ProviderEvent;
        use crate::ws::broadcaster::Broadcaster;

        let db = test_db();
        seed_usage_session(&db, "s1").await;
        let broadcaster = Broadcaster::new();

        for _ in 0..2 {
            emit_event(
                &db,
                &broadcaster,
                "s1",
                ProviderEvent::Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: 5,
                    cache_creation_tokens: 3,
                    total_tokens: 128,
                    context_tokens: 108,
                    model: Some("claude:claude-opus-4-8".into()),
                    turn_seq: None,
                },
            )
            .await;
        }

        let rows = db.usage_events_for_session("s1").await.unwrap();
        assert_eq!(rows.len(), 2, "two turns must mirror two usage rows");
        assert_eq!(rows[0].input_tokens, 100);
        assert_eq!(rows[0].output_tokens, 20);
        assert_eq!(rows[0].cache_read_tokens, 5);
        assert_eq!(rows[0].cache_creation_tokens, 3);
        assert_eq!(rows[0].total_tokens, 128);
        assert_eq!(rows[0].context_tokens, 108);
        assert_eq!(rows[0].model.as_deref(), Some("claude:claude-opus-4-8"));
        // turn_seq is auto-assigned per session: 1, 2, …
        assert_eq!(rows[0].turn_seq, Some(1));
        assert_eq!(rows[1].turn_seq, Some(2));
        // Each row back-links to its originating agent-usage event.
        let event_id = rows[0].event_id.clone().expect("event_id linked");
        let ev = db
            .get_event(&event_id)
            .await
            .unwrap()
            .expect("event exists");
        assert_eq!(ev.kind, "agent-usage");
    }

    /// CRUD query helpers: rows are queryable by session and by inclusive
    /// time range, and a query for one session never leaks another's rows.
    #[tokio::test]
    async fn usage_events_query_by_session_and_time_range() {
        let db = test_db();
        seed_usage_session(&db, "s1").await;
        // A second session in the same project to prove isolation.
        db.create_session(NewSession {
            id: "s2".into(),
            name: "Other".into(),
            folder_id: "f1".into(),
            project_id: Some("p1".into()),
            card_id: Some("c1".into()),
            is_worker: true,
            created_at: now(),
            last_activity: now(),
            ..Default::default()
        })
        .await
        .unwrap();

        // Five usage rows on s1 at distinct, increasing timestamps.
        for i in 0..5i64 {
            db.record_usage_event(NewUsageEvent {
                id: format!("u{i}"),
                session_id: "s1".into(),
                event_id: None,
                turn_seq: None,
                ts: 1_000 + i * 10,
                input_tokens: 10 * (i + 1),
                output_tokens: i + 1,
                ..Default::default()
            })
            .await
            .unwrap();
        }
        // One row on s2 inside s1's time window — must not leak.
        db.record_usage_event(NewUsageEvent {
            id: "v0".into(),
            session_id: "s2".into(),
            event_id: None,
            turn_seq: None,
            ts: 1_020,
            input_tokens: 999,
            ..Default::default()
        })
        .await
        .unwrap();

        // By session: all five, oldest-first, isolated from s2.
        let all = db.usage_events_for_session("s1").await.unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].ts, 1_000);
        assert_eq!(all[4].ts, 1_040);
        assert!(all.iter().all(|r| r.session_id == "s1"));

        // By time range: inclusive on both ends — ts in [1_010, 1_030].
        let window = db
            .usage_events_for_session_in_range("s1", 1_010, 1_030)
            .await
            .unwrap();
        let tss: Vec<i64> = window.iter().map(|r| r.ts).collect();
        assert_eq!(tss, vec![1_010, 1_020, 1_030]);

        // s2's row at ts=1_020 stays out of s1's range query.
        assert!(window.iter().all(|r| r.session_id == "s1"));
        let s2 = db.usage_events_for_session("s2").await.unwrap();
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].input_tokens, 999);
    }

    /// Claude accounts: CRUD round-trip, per-account usage attribution, and
    /// delete orphan-nulling its usage rows while reporting the config dir.
    #[tokio::test]
    async fn claude_accounts_crud_and_usage_attribution() {
        let db = test_db();
        seed_usage_session(&db, "s1").await; // satisfies the usage FK

        let acct = db
            .create_claude_account(NewClaudeAccount {
                id: "acc1".into(),
                name: "Work".into(),
                kind: "oauth_token".into(),
                credential: "tok-secret".into(),
                config_dir: Some("/tmp/peck/acc1".into()),
                budget_window_hours: Some(5),
                budget_limit_usd: Some(10.0),
                budget_limit_tokens: None,
                warn_threshold: 0.75,
                critical_threshold: 0.90,
                created_at: 1_000,
                updated_at: 1_000,
                refresh_token: None,
                token_expires_at: None,
            })
            .await
            .unwrap();
        assert_eq!(acct.name, "Work");

        // Listed and fetchable.
        assert_eq!(db.list_claude_accounts().await.unwrap().len(), 1);
        assert_eq!(
            db.get_claude_account("acc1").await.unwrap().unwrap().kind,
            "oauth_token"
        );

        // Two turns billed to the account, one unattributed (Default).
        for (i, acc) in [Some("acc1"), Some("acc1"), None].iter().enumerate() {
            db.record_usage_event(NewUsageEvent {
                id: format!("u{i}"),
                session_id: "s1".into(),
                ts: 5_000 + i as i64,
                total_tokens: 100,
                model: Some("claude-opus-4-8".into()),
                account_id: acc.map(str::to_string),
                ..Default::default()
            })
            .await
            .unwrap();
        }

        // Only the two attributed turns roll up to the account, grouped by model.
        let rows = db.account_usage_since("acc1", 0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turns, 2);
        assert_eq!(rows[0].total_tokens, 200);

        // Update: rename and clear the budget.
        let changed = db
            .update_claude_account(
                "acc1",
                ClaudeAccountChanges {
                    name: Some("Personal".into()),
                    budget_window_hours: Some(None),
                    budget_limit_usd: Some(None),
                    updated_at: Some(2_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changed.name, "Personal");
        assert_eq!(changed.budget_window_hours, None);
        assert_eq!(changed.budget_limit_usd, None);
        // Credential left untouched when not supplied.
        assert_eq!(changed.credential, "tok-secret");

        // Delete returns the config dir and orphan-nulls the usage rows.
        let dir = db.delete_claude_account("acc1").await.unwrap().unwrap();
        assert_eq!(dir.as_deref(), Some("/tmp/peck/acc1"));
        assert!(db.get_claude_account("acc1").await.unwrap().is_none());
        assert!(db.account_usage_since("acc1", 0).await.unwrap().is_empty());
        // The usage rows themselves survive (now unattributed).
        assert_eq!(db.usage_events_for_session("s1").await.unwrap().len(), 3);

        // Deleting a missing account is a clean None, not an error.
        assert!(db.delete_claude_account("nope").await.unwrap().is_none());
    }

    // ── Session ownership ────────────────────────────────────────────

    async fn seed_user(db: &Db, id: &str) {
        let ts = now();
        db.create_user(NewUser {
            id: id.into(),
            username: id.into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn resolve_owner_uses_sole_user() {
        let db = test_db();
        seed_user(&db, "u1").await;
        // No parent session, single user -> that user (the backfill rule).
        assert_eq!(
            db.resolve_spawned_session_owner(None).await.as_deref(),
            Some("u1")
        );
        assert_eq!(
            db.resolve_spawned_session_owner_blocking(None).as_deref(),
            Some("u1")
        );
    }

    #[tokio::test]
    async fn resolve_owner_none_when_multiple_users() {
        let db = test_db();
        seed_user(&db, "u1").await;
        seed_user(&db, "u2").await;
        // Ambiguous -> unowned (NULL), per the documented multi-user policy.
        assert!(db.resolve_spawned_session_owner(None).await.is_none());
        assert!(db.resolve_spawned_session_owner_blocking(None).is_none());
    }

    #[tokio::test]
    async fn resolve_owner_inherits_parent_over_ambiguity() {
        let db = test_db();
        let ts = now();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        seed_user(&db, "u1").await;
        seed_user(&db, "u2").await;
        db.create_session(NewSession {
            id: "parent".into(),
            name: "parent".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            user_id: Some("u2".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        // Parent owner wins even when the sole-user fallback is ambiguous.
        assert_eq!(
            db.resolve_spawned_session_owner(Some("parent"))
                .await
                .as_deref(),
            Some("u2")
        );
        assert_eq!(
            db.resolve_spawned_session_owner_blocking(Some("parent"))
                .as_deref(),
            Some("u2")
        );
        // Unknown / unowned parent falls through to the ambiguous fallback.
        assert!(
            db.resolve_spawned_session_owner(Some("nope"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn chat_style_session_stores_explicit_owner() {
        // Mirrors the authenticated create_session route: an explicit owner is
        // persisted and round-trips through get_session.
        let db = test_db();
        let ts = now();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        seed_user(&db, "u1").await;
        db.create_session(NewSession {
            id: "s1".into(),
            name: "chat".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            user_id: Some("u1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(
            db.get_session("s1")
                .await
                .unwrap()
                .unwrap()
                .user_id
                .as_deref(),
            Some("u1")
        );
    }
}
