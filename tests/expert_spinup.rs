//! Integration test for the `spin_up_experts` MCP tool: on a small temp
//! project it partitions the codebase into more than one long-lived expert
//! session, each persisted with `is_expert=true`, a scope_path, and a
//! captured knowledge_summary — and those experts never leak into the plain
//! chat session list.
//!
//! Uses a `mock:*` model on the project so the test is deterministic and the
//! created experts inherit a non-Claude model id. No live dispatcher is wired
//! (expert_dispatcher = None), so capture is the in-handler structural pass.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::ws::broadcaster::Broadcaster;

fn write_source(root: &std::path::Path, rel: &str, bytes: usize) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "x".repeat(bytes)).unwrap();
}

#[tokio::test]
async fn spin_up_experts_creates_persistent_hidden_experts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Several topic dirs of source so the partition produces >1 expert.
    for dir in ["auth", "billing", "core", "ui"] {
        write_source(root, &format!("{dir}/mod.rs"), 20_000);
    }
    // Ignored output must not become a topic.
    write_source(root, "target/junk.rs", 999_999);

    let db = Arc::new(Db::in_memory().unwrap());
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: root.to_string_lossy().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "p1".into(),
        name: "Project".into(),
        context: String::new(),
        folder_id: "f1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "task".into(),
        model: Some("mock:happy-path".into()),
        effort: None,
        parallel_instructions: false,
        auto_notify_changes: true,
        worker_communication: false,
        created_at: ts.clone(),
        last_accessed_at: ts.clone(),
    })
    .await
    .unwrap();

    let registry = McpToolRegistry::new();
    let ctx = ToolCallContext {
        session_id: "caller".into(),
        project_id: Some("p1".into()),
        card_id: None,
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: None,
        expert_dispatcher: None,
    };

    let result = registry
        .handle_tool_call("spin_up_experts", serde_json::json!({}), &ctx)
        .await
        .unwrap();

    let experts = result["experts"].as_array().unwrap();
    assert!(
        experts.len() > 1,
        "expected more than one expert, got {}",
        experts.len()
    );
    for e in experts {
        assert!(e["session_id"].as_str().is_some());
        let scope = e["scope_path"].as_str().unwrap();
        assert!(!scope.is_empty(), "scope_path must be set");
        assert!(
            !scope.contains("target"),
            "ignored dirs must not be in scope"
        );
        assert!(
            e["knowledge_summary"]
                .as_str()
                .unwrap()
                .contains("Knowledge area"),
            "knowledge_summary must be captured"
        );
    }

    // Persistence + invariants: the knowledge experts from the spin-up are
    // stored, marked is_expert, carry the mock model, and are HIDDEN from the
    // plain chat session list. spin_up_experts also ensures this project's
    // question-expert (consult-before-ask), so the stored set is the knowledge
    // experts plus that one question-expert.
    let stored = db.list_expert_sessions_by_project("p1").await.unwrap();
    let knowledge: Vec<_> = stored
        .iter()
        .filter(|s| s.expert_kind.as_deref() == Some("knowledge"))
        .collect();
    assert_eq!(knowledge.len(), experts.len());
    for s in &knowledge {
        assert!(s.is_expert);
        assert_eq!(s.model.as_deref(), Some("mock:happy-path"));
        assert!(s.scope_path.is_some());
        assert!(s.knowledge_summary.is_some());
        assert!(!s.is_worker);
    }

    // Exactly one per-project question-expert was created, and it is the
    // permanent stable-id one.
    let question: Vec<_> = stored
        .iter()
        .filter(|s| s.expert_kind.as_deref() == Some("question"))
        .collect();
    assert_eq!(question.len(), 1, "exactly one project question-expert");
    assert!(question[0].is_expert);
    assert!(question[0].is_permanent);
    assert!(!question[0].is_worker);

    let plain = db.list_plain_sessions().await.unwrap();
    assert!(
        plain.is_empty(),
        "experts must not appear in the plain chat session list"
    );
}

#[tokio::test]
async fn spin_up_experts_rejects_out_of_scope_project() {
    let db = Arc::new(Db::in_memory().unwrap());
    let registry = McpToolRegistry::new();
    // Token scoped to p1; caller tries to target p2.
    let ctx = ToolCallContext {
        session_id: "caller".into(),
        project_id: Some("p1".into()),
        card_id: None,
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: None,
        expert_dispatcher: None,
    };
    let err = registry
        .handle_tool_call(
            "spin_up_experts",
            serde_json::json!({ "project_id": "p2" }),
            &ctx,
        )
        .await;
    assert!(err.is_err(), "cross-project target must be rejected");
}
