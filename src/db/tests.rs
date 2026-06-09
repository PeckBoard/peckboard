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

        let card = db
            .create_card(NewCard {
                id: "c1".into(),
                project_id: "p1".into(),
                title: "Fix bug".into(),
                description: "It's broken".into(),
                step: "backlog".into(),
                priority: 1,
                workflow: None,
                model: None,
                effort: None,
                created_at: ts.clone(),
                updated_at: ts.clone(),
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

        let mk = |id: &str| NewCard {
            id: id.into(),
            project_id: "p1".into(),
            title: id.into(),
            description: "".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: None,
            model: None,
            effort: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
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
            default_workflow: None,
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
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
                workflow: None,
                model: None,
                effort: None,
                created_at: ts.clone(),
                updated_at: ts.clone(),
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

        for cid in ["older", "newer"] {
            db.create_card(NewCard {
                id: cid.into(),
                project_id: "p1".into(),
                title: cid.into(),
                description: "".into(),
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
}
