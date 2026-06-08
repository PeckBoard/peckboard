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
}
