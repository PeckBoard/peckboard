//! Skills (`service::skills`): local versioning + groups + session
//! resolution and Claude materialisation, a team source round trip against
//! a local bare git repo (publish → sync → subscribe → pin → guard), and the
//! REST happy path.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::service::skills::{self, Actor, CreateSkill, SkillError, SourceInput, UpdateSkill};
use serde_json::Value;
use tower::ServiceExt;

fn admin() -> Actor {
    Actor {
        user_id: Some("u1".into()),
        is_admin: true,
    }
}

async fn seed_session(db: &Db, id: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    if db.get_folder("f1").await.unwrap().is_none() {
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp/f1".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
    }
    db.create_session(NewSession {
        id: id.into(),
        name: id.into(),
        folder_id: "f1".into(),
        created_at: now.clone(),
        last_activity: now,
        user_id: Some("u1".into()),
        ..Default::default()
    })
    .await
    .unwrap();
}

fn create(name: &str, body: &str, groups: &[&str]) -> CreateSkill {
    CreateSkill {
        name: name.into(),
        description: format!("{name} description"),
        tags: vec!["test".into()],
        groups: groups.iter().map(|g| g.to_string()).collect(),
        body: body.into(),
        version: None,
    }
}

/// Test fixture git (a temp bare repo and a teammate's clone), not an agent
/// spawn.
#[allow(clippy::disallowed_methods)]
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn local_skills_version_group_and_resolve_for_a_session() {
    let db = Db::in_memory().unwrap();
    seed_session(&db, "s1").await;
    let a = skills::create(&db, &admin(), create("alpha", "v1 body", &["g1"]))
        .await
        .unwrap();
    assert_eq!(a.version, "1.0.0");
    assert_eq!(a.groups, vec!["g1"]);
    let id = a.id.clone().unwrap();

    let a2 = skills::update(
        &db,
        &admin(),
        &id,
        UpdateSkill {
            body: Some("v2 body".into()),
            bump: Some("minor".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(a2.version, "1.1.0");
    assert_eq!(a2.versions, vec!["1.1.0", "1.0.0"]);
    assert_eq!(a2.body.as_deref(), Some("v2 body"));
    // History is immutable: the old body is still there.
    let v1 = skills::get_version(&db, &id, "1.0.0").await.unwrap();
    assert_eq!(v1["body"], "v1 body");

    // Someone else may not edit it.
    let other = Actor {
        user_id: Some("u2".into()),
        is_admin: false,
    };
    let err = skills::update(&db, &other, &id, UpdateSkill::default())
        .await
        .unwrap_err();
    assert!(matches!(err, SkillError::Forbidden(_)), "{err}");

    skills::create(&db, &admin(), create("beta", "beta body", &["g1"]))
        .await
        .unwrap();
    // Direct alpha + group g1 (alpha, beta): alpha stays direct, beta comes
    // in through the group.
    let out = skills::set_session_skills(&db, "s1", vec!["alpha".into()], vec!["g1".into()])
        .await
        .unwrap();
    let resolved = out["resolved"].as_array().unwrap();
    let names: Vec<&str> = resolved
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    assert_eq!(resolved[0]["via_group"], Value::Null);
    assert_eq!(resolved[1]["via_group"], "g1");

    let rs = skills::session::resolve(&db, "s1").await.unwrap();
    assert_eq!(rs[0].version, "1.1.0");
    assert_eq!(rs[0].body, "v2 body");
    let prompt = skills::session::prompt_section(&rs, false).unwrap();
    assert!(
        prompt.contains("`alpha` (v1.1.0): alpha description"),
        "{prompt}"
    );
    assert!(prompt.contains("skill_read"));

    // Claude: a per-session plugin dir with one SKILL.md per skill.
    let tmp = tempfile::tempdir().unwrap();
    let dir = skills::session::materialize_claude(tmp.path(), "s1", &rs)
        .unwrap()
        .unwrap();
    assert!(dir.ends_with("peckboard-skills/s1"));
    assert!(dir.join(".claude-plugin/plugin.json").is_file());
    let md = std::fs::read_to_string(dir.join("skills/alpha/SKILL.md")).unwrap();
    assert!(md.starts_with("---\nname: alpha\ndescription: alpha description\n"));
    assert!(md.contains("v2 body"));
    // No skills → the dir is removed.
    assert!(
        skills::session::materialize_claude(tmp.path(), "s1", &[])
            .unwrap()
            .is_none()
    );
    assert!(!dir.exists());

    assert_eq!(
        skills::session::read_for_session(&db, "s1", "beta")
            .await
            .unwrap()
            .body,
        "beta body"
    );
    assert!(
        skills::session::read_for_session(&db, "s1", "nope")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn team_source_publish_sync_subscribe_pin_and_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--quiet", "--bare"]);

    let db = Db::in_memory().unwrap();
    seed_session(&db, "s1").await;
    let team = skills::create_source(
        &db,
        &admin(),
        SourceInput {
            kind: "team".into(),
            name: "Team".into(),
            url: bare.to_string_lossy().to_string(),
            branch: Some("main".into()),
            secret_ref: None,
        },
    )
    .await
    .unwrap();

    // Publish a local skill to the (empty) team repo.
    let local = skills::create(&db, &admin(), create("deploy-notes", "Step one", &[]))
        .await
        .unwrap();
    let lid = local.id.clone().unwrap();
    let r = skills::publish(&db, &data_dir, &admin(), &lid, &team.id)
        .await
        .unwrap();
    assert_eq!(r["committed"], true);
    assert_eq!(r["path"], "skills/deploy-notes/1.0.0/SKILL.md");
    skills::update(
        &db,
        &admin(),
        &lid,
        UpdateSkill {
            body: Some("Step two".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    skills::publish(&db, &data_dir, &admin(), &lid, &team.id)
        .await
        .unwrap();

    // The remote really has both versions and a generated index.
    let check = tmp.path().join("check");
    git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            "--branch",
            "main",
            &bare.to_string_lossy(),
            "check",
        ],
    );
    assert!(check.join("skills/deploy-notes/1.0.0/SKILL.md").is_file());
    assert!(check.join("skills/deploy-notes/1.0.1/SKILL.md").is_file());
    let index: Value =
        serde_json::from_str(&std::fs::read_to_string(check.join("index.json")).unwrap()).unwrap();
    assert_eq!(index["skills"][0]["latest"], "1.0.1");

    // Subscribe to the team copy pinned at 1.0.0.
    let sub = skills::subscribe(
        &db,
        &data_dir,
        &admin(),
        &team.id,
        "deploy-notes",
        Some("1.0.0".into()),
    )
    .await
    .unwrap();
    assert_eq!(sub.scope, "team");
    assert_eq!(sub.version, "1.0.1");
    assert_eq!(sub.pinned_version.as_deref(), Some("1.0.0"));
    let sid = sub.id.clone().unwrap();
    skills::set_session_skills(&db, "s1", vec![sid.clone()], vec![])
        .await
        .unwrap();
    let rs = skills::session::resolve(&db, "s1").await.unwrap();
    assert_eq!(
        (rs[0].version.as_str(), rs[0].body.as_str()),
        ("1.0.0", "Step one\n")
    );

    // Unpin → latest.
    skills::set_subscription(&db, &data_dir, &admin(), &sid, None)
        .await
        .unwrap();
    let rs = skills::session::resolve(&db, "s1").await.unwrap();
    assert_eq!(
        (rs[0].version.as_str(), rs[0].body.as_str()),
        ("1.0.1", "Step two\n")
    );

    // A teammate pushes 1.1.0 straight to the repo; sync moves the head.
    let v110 = check.join("skills/deploy-notes/1.1.0");
    std::fs::create_dir_all(&v110).unwrap();
    std::fs::write(
        v110.join("SKILL.md"),
        "---\nname: deploy-notes\ndescription: newer\nversion: 1.1.0\n---\nStep three\n",
    )
    .unwrap();
    git(&check, &["add", "-A"]);
    git(&check, &["commit", "--quiet", "-m", "1.1.0"]);
    git(&check, &["push", "--quiet", "origin", "HEAD:main"]);
    let src = skills::sync_source(&db, &data_dir, &admin(), &team.id)
        .await
        .unwrap();
    assert!(src.last_sync_at.is_some());
    assert_eq!(src.last_error, None);
    let rs = skills::session::resolve(&db, "s1").await.unwrap();
    assert_eq!(
        (rs[0].version.as_str(), rs[0].body.as_str()),
        ("1.1.0", "Step three\n")
    );

    // The guard refuses a skill carrying a credential.
    let leaky = skills::create(
        &db,
        &admin(),
        create("leaky", "use ghp_abcdefghijklmnopqrstuvwxyz0123456789", &[]),
    )
    .await
    .unwrap();
    let err = skills::publish(&db, &data_dir, &admin(), &leaky.id.unwrap(), &team.id)
        .await
        .unwrap_err();
    match &err {
        SkillError::Proprietary(f) => assert_eq!(f[0].kind, "github_token"),
        other => panic!("expected proprietary_content, got {other}"),
    }
    assert_eq!(err.status(), 422);
    assert_eq!(err.body()["error"], "proprietary_content");

    // Team memory: add, refuse a duplicate and a leak, remove.
    let m = skills::write_memory(
        &db,
        &data_dir,
        &team.id,
        "oncall",
        Some("On-call".into()),
        "Rotate weekly.".into(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(m.title, "On-call");
    let dup = skills::write_memory(&db, &data_dir, &team.id, "oncall", None, "x".into(), true)
        .await
        .unwrap_err();
    assert!(matches!(dup, SkillError::Conflict(_)), "{dup}");
    let leak = skills::write_memory(
        &db,
        &data_dir,
        &team.id,
        "hosts",
        None,
        "db is at 10.1.2.3".into(),
        true,
    )
    .await
    .unwrap_err();
    assert!(matches!(leak, SkillError::Proprietary(_)), "{leak}");
    let list = skills::list_memory(&db, &data_dir, &team.id).await.unwrap();
    assert_eq!(list.len(), 1);
    skills::remove_memory(&db, &data_dir, &team.id, "oncall")
        .await
        .unwrap();
    assert!(
        skills::list_memory(&db, &data_dir, &team.id)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn build_state() -> (Arc<peckboard::state::AppState>, String) {
    use peckboard::auth::rate_limit::RateLimiter;
    use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
    use peckboard::db::models::{NewAuthSession, NewUser};

    let tmp = tempfile::tempdir().unwrap();
    let config = peckboard::config::Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
    };
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(peckboard::plugin::manager::PluginManager::new(
        &config.data_dir,
        db.clone(),
    ));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(peckboard::provider::registry::ProviderRegistry::new());
    let session_manager =
        peckboard::provider::manager::SessionManager::new(provider_registry.clone());
    let push_service = peckboard::service::push::PushService::new(&config.data_dir);
    let now = chrono::Utc::now().to_rfc3339();
    db.create_user(NewUser {
        id: "u1".into(),
        username: "admin".into(),
        email: None,
        password_hash: "h".into(),
        role: "admin".into(),
        created_at: now.clone(),
        updated_at: now,
    })
    .await
    .unwrap();
    let (token, _exp) = create_token(&jwt_secret, "u1", "admin", "as1").unwrap();
    db.create_auth_session(NewAuthSession {
        id: "as1".into(),
        user_id: "u1".into(),
        token_hash: hash_token(&token),
        created_at: 1_000_000,
        expires_at: 1_000_000 + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();
    let state = Arc::new(peckboard::state::AppState {
        background: Default::default(),
        plugin_ws_tickets: Default::default(),
        device_registry: Default::default(),
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config,
        db,
        plugins,
        builtin_plugins: Arc::new(peckboard::plugin::builtin::BuiltinPluginRegistry::new()),
        jwt_secret,
        ssh_vault_key: vec![0u8; 32],
        mfa_vault_key: vec![0u8; 32],
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: peckboard::ws::broadcaster::Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: peckboard::service::mcp_server::McpTokenRegistry::new(),
        push_service,
        remote_access: peckboard::service::remote_access::RemoteAccess::inert(),
        terminals: peckboard::terminal::TerminalManager::inert(),
        tls: Arc::new(peckboard::state::TlsState::new()),
    });
    std::mem::forget(tmp);
    (state, token)
}

#[tokio::test]
async fn rest_create_list_and_source_listing() {
    let (state, token) = build_state().await;
    let app = peckboard::routes::skills::router(state.clone()).with_state(state.clone());
    let call = |method: &str, uri: &str, body: Option<Value>| {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        if body.is_some() {
            b = b.header(header::CONTENT_TYPE, "application/json");
        }
        b.body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .unwrap()
    };
    async fn json(res: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    let res = app
        .clone()
        .oneshot(call(
            "POST",
            "/api/skills",
            Some(serde_json::json!({
                "name": "pr-review-checklist",
                "description": "Use when reviewing a PR",
                "tags": ["review"],
                "groups": ["pr-review"],
                "body": "1. Read the diff."
            })),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created = json(res).await;
    assert_eq!(created["version"], "1.0.0");
    assert_eq!(created["groups"][0], "pr-review");

    // Bad names are a 400, duplicates a 409.
    let res = app
        .clone()
        .oneshot(call(
            "POST",
            "/api/skills",
            Some(serde_json::json!({ "name": "Bad Name", "body": "x" })),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = app
        .clone()
        .oneshot(call(
            "POST",
            "/api/skills",
            Some(serde_json::json!({ "name": "pr-review-checklist", "body": "x" })),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    let res = app
        .clone()
        .oneshot(call("GET", "/api/skills?q=review", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let list = json(res).await;
    assert_eq!(list["skills"][0]["name"], "pr-review-checklist");
    assert_eq!(list["skills"][0]["body"], "1. Read the diff.");

    let res = app
        .clone()
        .oneshot(call("GET", "/api/skill-sources", None))
        .await
        .unwrap();
    let sources = json(res).await;
    assert_eq!(sources["sources"][0]["id"], "public");
    assert_eq!(sources["sources"][0]["name"], "Peckboard Public");

    // Unauthenticated requests are refused.
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/skills")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
