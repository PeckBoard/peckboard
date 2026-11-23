//! End-to-end tests of the hub's HTTP contract against an in-process
//! router, with real Ed25519 signing.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use peckboard_skills_hub::{Config, Hub, admin, api, auth};
use serde_json::{Value, json};
use tower::ServiceExt;

struct T {
    hub: Arc<Hub>,
    app: Router,
    _dir: tempfile::TempDir,
}

fn setup_with(f: impl FnOnce(&mut Config)) -> T {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::new("skills.test", dir.path().to_path_buf());
    // Cheap PoW for tests; the default (20) is checked separately.
    cfg.new_key_pow_bits = 6;
    f(&mut cfg);
    let hub = Hub::open(cfg).unwrap();
    let app = api::router(hub.clone());
    T {
        hub,
        app,
        _dir: dir,
    }
}

fn setup() -> T {
    setup_with(|_| {})
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
    call(app, Request::get(uri).body(Body::empty()).unwrap()).await
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    let (s, b) = get(app, uri).await;
    (s, serde_json::from_str(&b).unwrap_or(Value::Null))
}

async fn post_json(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (s, b) = call(app, req).await;
    (s, serde_json::from_str(&b).unwrap_or(Value::Null))
}

fn new_key() -> SigningKey {
    SigningKey::from_bytes(&rand::random::<[u8; 32]>())
}

fn pk(sk: &SigningKey) -> String {
    STANDARD.encode(sk.verifying_key().to_bytes())
}

fn url_key(sk: &SigningKey) -> String {
    // Query-safe: standard base64 can contain '+' and '/'.
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes())
}

async fn challenge(app: &Router, sk: &SigningKey) -> (String, u32) {
    let (s, v) = get_json(app, &format!("/api/challenge?public_key={}", url_key(sk))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["expires_in"], 300);
    (
        v["nonce"].as_str().unwrap().to_string(),
        v["pow_difficulty"].as_u64().unwrap() as u32,
    )
}

fn skill(name: &str, version: &str, groups: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: A test skill\nversion: {version}\ntags: [test]\ngroups: [{groups}]\n---\n{body}\n"
    )
}

fn publish_body(sk: &SigningKey, signer: &SigningKey, nonce: &str, pow: &str, md: &str) -> Value {
    let sig = signer.sign(auth::publish_message(nonce, md).as_bytes());
    json!({
        "skill_md": md,
        "public_key": pk(sk),
        "nonce": nonce,
        "pow": pow,
        "signature": STANDARD.encode(sig.to_bytes()),
    })
}

/// Challenge, solve, sign, publish.
async fn publish(app: &Router, sk: &SigningKey, md: &str) -> (StatusCode, Value) {
    let (nonce, bits) = challenge(app, sk).await;
    let pow = auth::solve_pow(&nonce, bits);
    post_json(app, "/api/publish", &publish_body(sk, sk, &nonce, &pow, md)).await
}

async fn yank(app: &Router, sk: &SigningKey, name: &str, version: &str) -> (StatusCode, Value) {
    let (nonce, _) = challenge(app, sk).await;
    let sig = sk.sign(auth::yank_message(&nonce, name, version).as_bytes());
    let body = json!({
        "name": name, "version": version, "public_key": pk(sk),
        "nonce": nonce, "signature": STANDARD.encode(sig.to_bytes()),
    });
    post_json(app, "/api/yank", &body).await
}

#[tokio::test]
async fn publish_happy_path_lists_serves_and_groups() {
    let t = setup();
    let sk = new_key();
    let md = skill(
        "pr-review",
        "1.2.0",
        "review-pack",
        "# Steps\n\n1. Read the diff.",
    );
    let (s, v) = publish(&t.app, &sk, &md).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["name"], "pr-review");
    assert_eq!(v["version"], "1.2.0");
    assert_eq!(
        v["url"],
        "https://skills.test/skills/pr-review/1.2.0/SKILL.md"
    );

    let (s, body) = get(&t.app, "/skills/pr-review/1.2.0/SKILL.md").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, md);

    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/index.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.headers()["cache-control"], "public, max-age=60");
    let (_, idx) = get_json(&t.app, "/index.json").await;
    assert_eq!(idx["schema"], 1);
    assert!(idx["generated_at"].is_string());
    let s0 = &idx["skills"][0];
    assert_eq!(s0["name"], "pr-review");
    assert_eq!(s0["description"], "A test skill");
    assert_eq!(s0["tags"], json!(["test"]));
    assert_eq!(s0["groups"], json!(["review-pack"]));
    assert_eq!(s0["versions"], json!(["1.2.0"]));
    assert_eq!(s0["latest"], "1.2.0");
    assert_eq!(
        idx["groups"],
        json!([{"slug": "review-pack", "name": "review-pack", "description": "", "skills": ["pr-review"]}])
    );
    let (s, g) = get_json(&t.app, "/groups/review-pack.json").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(g["skills"], json!(["pr-review"]));
    assert_eq!(
        t.hub
            .store()
            .group("review-pack")
            .unwrap()
            .unwrap()
            .owner_key,
        pk(&sk)
    );

    // The key is known now: no more proof of work.
    assert_eq!(challenge(&t.app, &sk).await.1, 0);
}

#[tokio::test]
async fn replayed_nonce_is_refused() {
    let t = setup();
    let sk = new_key();
    let md = skill("replay", "1.0.0", "", "body");
    let (nonce, bits) = challenge(&t.app, &sk).await;
    let pow = auth::solve_pow(&nonce, bits);
    let body = publish_body(&sk, &sk, &nonce, &pow, &md);
    let (s, _) = post_json(&t.app, "/api/publish", &body).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = post_json(&t.app, "/api/publish", &body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["code"], "nonce_invalid");
}

#[tokio::test]
async fn wrong_signature_is_refused() {
    let t = setup();
    let sk = new_key();
    let other = new_key();
    let md = skill("forged", "1.0.0", "", "body");
    let (nonce, bits) = challenge(&t.app, &sk).await;
    let pow = auth::solve_pow(&nonce, bits);
    let (s, v) = post_json(
        &t.app,
        "/api/publish",
        &publish_body(&sk, &other, &nonce, &pow, &md),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["code"], "bad_signature");
    // A signature over different content fails too.
    let (nonce, bits) = challenge(&t.app, &sk).await;
    let pow = auth::solve_pow(&nonce, bits);
    let mut body = publish_body(&sk, &sk, &nonce, &pow, &md);
    body["skill_md"] = json!(skill("forged", "1.0.0", "", "tampered"));
    let (s, v) = post_json(&t.app, "/api/publish", &body).await;
    assert_eq!(
        (s, v["code"].clone()),
        (StatusCode::UNAUTHORIZED, json!("bad_signature"))
    );
}

#[tokio::test]
async fn name_owned_by_another_key_conflicts() {
    let t = setup();
    let (a, b) = (new_key(), new_key());
    let (s, _) = publish(&t.app, &a, &skill("shared-name", "1.0.0", "", "a")).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = publish(&t.app, &b, &skill("shared-name", "1.1.0", "", "b")).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["code"], "name_taken");
}

#[tokio::test]
async fn same_version_twice_conflicts() {
    let t = setup();
    let sk = new_key();
    let (s, _) = publish(&t.app, &sk, &skill("twice", "1.0.0", "", "one")).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = publish(&t.app, &sk, &skill("twice", "1.0.0", "", "two")).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["code"], "version_exists");
}

#[tokio::test]
async fn proprietary_content_is_refused_with_redacted_findings() {
    let t = setup();
    let sk = new_key();
    let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let md = skill(
        "leaky",
        "1.0.0",
        "",
        &format!("Deploy:\nexport GH={token}\nssh pg01.prod.internal"),
    );
    let (s, v) = publish(&t.app, &sk, &md).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["code"], "content_findings");
    let findings = v["findings"].as_array().unwrap();
    let kinds: Vec<&str> = findings
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"github_token"), "{kinds:?}");
    assert!(kinds.contains(&"internal_host"), "{kinds:?}");
    assert_eq!(findings[0]["line"], 2);
    assert!(!v.to_string().contains(token));
    // Nothing was stored.
    assert_eq!(
        get(&t.app, "/skills/leaky/1.0.0/SKILL.md").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn daily_publish_limit_is_enforced() {
    let t = setup_with(|c| c.publishes_per_day = 3);
    let sk = new_key();
    for i in 0..3 {
        let (s, v) = publish(&t.app, &sk, &skill("busy", &format!("1.0.{i}"), "", "x")).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let (s, v) = publish(&t.app, &sk, &skill("busy", "1.0.9", "", "x")).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(v["code"], "rate_limited");
}

#[tokio::test]
async fn new_keys_must_pay_proof_of_work() {
    // The production default for a never-seen key.
    let t = setup_with(|c| c.new_key_pow_bits = peckboard_skills_hub::DEFAULT_NEW_KEY_POW_BITS);
    let sk = new_key();
    let (nonce, bits) = challenge(&t.app, &sk).await;
    assert_eq!(bits, 20);
    let md = skill("fresh", "1.0.0", "", "x");
    let (s, v) = post_json(
        &t.app,
        "/api/publish",
        &publish_body(&sk, &sk, &nonce, "", &md),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(v["code"], "pow_required");

    // Solved (cheap difficulty), it goes through; afterwards none is asked.
    let t = setup_with(|c| c.new_key_pow_bits = 8);
    let (nonce, bits) = challenge(&t.app, &sk).await;
    assert_eq!(bits, 8);
    let (s, v) = post_json(
        &t.app,
        "/api/publish",
        &publish_body(&sk, &sk, &nonce, "wrong", &md),
    )
    .await;
    // "wrong" almost surely misses 8 bits; if it happens to hit, fine.
    if s != StatusCode::CREATED {
        assert_eq!(v["code"], "pow_required");
        let (nonce, _) = challenge(&t.app, &sk).await;
        let pow = auth::solve_pow(&nonce, 8);
        let (s, v) = post_json(
            &t.app,
            "/api/publish",
            &publish_body(&sk, &sk, &nonce, &pow, &md),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    assert_eq!(challenge(&t.app, &sk).await.1, 0);
}

#[tokio::test]
async fn yank_hides_from_index_but_keeps_serving() {
    let t = setup();
    let sk = new_key();
    for v in ["1.0.0", "1.1.0"] {
        let (s, _) = publish(&t.app, &sk, &skill("yanky", v, "", "x")).await;
        assert_eq!(s, StatusCode::CREATED);
    }
    let (s, v) = yank(&t.app, &new_key(), "yanky", "1.1.0").await;
    assert_eq!(
        (s, v["code"].clone()),
        (StatusCode::FORBIDDEN, json!("not_owner"))
    );

    let (s, v) = yank(&t.app, &sk, "yanky", "1.1.0").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, idx) = get_json(&t.app, "/index.json").await;
    assert_eq!(idx["skills"][0]["versions"], json!(["1.0.0"]));
    assert_eq!(idx["skills"][0]["latest"], "1.0.0");
    assert_eq!(
        get(&t.app, "/skills/yanky/1.1.0/SKILL.md").await.0,
        StatusCode::OK
    );

    // Yanking the last listed version drops the skill from the index.
    let (s, _) = yank(&t.app, &sk, "yanky", "1.0.0").await;
    assert_eq!(s, StatusCode::OK);
    let (_, idx) = get_json(&t.app, "/index.json").await;
    assert_eq!(idx["skills"], json!([]));
}

#[tokio::test]
async fn import_takedown_and_ban() {
    let t = setup();
    let src = tempfile::tempdir().unwrap();
    let w = |p: &str, text: &str| {
        let path = src.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    w(
        "skills/alpha/1.0.0/SKILL.md",
        &skill("alpha", "1.0.0", "", "a1"),
    );
    w(
        "skills/alpha/1.1.0/SKILL.md",
        &skill("alpha", "1.1.0", "", "a2"),
    );
    w(
        "skills/beta/0.1.0/SKILL.md",
        &skill("beta", "0.1.0", "", "b"),
    );
    // Path and frontmatter disagree: skipped.
    w(
        "skills/gamma/1.0.0/SKILL.md",
        &skill("gamma", "2.0.0", "", "g"),
    );
    w(
        "groups/starter.json",
        r#"{"slug":"starter","name":"Starter pack","description":"First skills","skills":["beta"]}"#,
    );
    let owner = admin::admin_key(t.hub.cfg.data_dir.as_path()).unwrap();
    // Stable across calls (kept in the data dir).
    assert_eq!(
        admin::admin_key(t.hub.cfg.data_dir.as_path()).unwrap(),
        owner
    );
    let rep = admin::import(&mut t.hub.store(), &t.hub.repos, src.path(), &owner).unwrap();
    assert_eq!(rep.imported.len(), 3, "{rep:?}");
    assert_eq!(rep.skipped.len(), 1, "{rep:?}");
    assert_eq!(rep.groups, 1);

    let (_, idx) = get_json(&t.app, "/index.json").await;
    let names: Vec<&str> = idx["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    assert_eq!(idx["skills"][0]["versions"], json!(["1.1.0", "1.0.0"]));
    assert_eq!(idx["skills"][1]["groups"], json!(["starter"]));
    assert_eq!(
        idx["groups"],
        json!([{"slug":"starter","name":"Starter pack","description":"First skills","skills":["beta"]}])
    );
    assert_eq!(
        get(&t.app, "/skills/beta/0.1.0/SKILL.md").await.1,
        skill("beta", "0.1.0", "", "b")
    );
    // Re-import is a no-op.
    let rep = admin::import(&mut t.hub.store(), &t.hub.repos, src.path(), &owner).unwrap();
    assert!(rep.imported.is_empty());

    // Transfer: the author's install takes over an imported skill and can
    // publish to it; others still can't.
    let author = new_key();
    admin::transfer(&t.hub.store(), "beta", &pk(&author)).unwrap();
    let (s, b) = publish(&t.app, &author, &skill("beta", "0.2.0", "", "b2")).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, _) = publish(&t.app, &new_key(), &skill("beta", "0.3.0", "", "b3")).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(admin::transfer(&t.hub.store(), "nope", &pk(&author)).is_err());
    // Takedown: gone from the index and no longer served.
    assert_eq!(admin::takedown(&t.hub.store(), "alpha", None).unwrap(), 2);
    let (_, idx) = get_json(&t.app, "/index.json").await;
    assert_eq!(idx["skills"].as_array().unwrap().len(), 1);
    assert_eq!(
        get(&t.app, "/skills/alpha/1.0.0/SKILL.md").await.0,
        StatusCode::NOT_FOUND
    );

    // Ban: the key's next publish (and challenge) is refused.
    let sk = new_key();
    let (s, _) = publish(&t.app, &sk, &skill("soon-banned", "1.0.0", "", "x")).await;
    assert_eq!(s, StatusCode::CREATED);
    let (nonce, _) = challenge(&t.app, &sk).await;
    admin::ban(&t.hub.store(), &pk(&sk), true).unwrap();
    let md = skill("soon-banned", "1.0.1", "", "x");
    let (s, v) = post_json(
        &t.app,
        "/api/publish",
        &publish_body(&sk, &sk, &nonce, "", &md),
    )
    .await;
    assert_eq!(
        (s, v["code"].clone()),
        (StatusCode::FORBIDDEN, json!("banned"))
    );
    let (s, v) = get_json(
        &t.app,
        &format!("/api/challenge?public_key={}", url_key(&sk)),
    )
    .await;
    assert_eq!(
        (s, v["code"].clone()),
        (StatusCode::FORBIDDEN, json!("banned"))
    );
}

#[tokio::test]
async fn oversized_and_malformed_requests() {
    let t = setup();
    let sk = new_key();
    let big = skill("big", "1.0.0", "", &"x".repeat(600 * 1024));
    let (s, v) = publish(&t.app, &sk, &big).await;
    assert_eq!(
        (s, v["code"].clone()),
        (StatusCode::PAYLOAD_TOO_LARGE, json!("too_large"))
    );
    let (s, v) = publish(&t.app, &sk, "no frontmatter").await;
    assert_eq!(
        (s, v["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("bad_request"))
    );
    assert!(v["error"].as_str().unwrap().contains("skill name"));
    let (s, v) = get_json(&t.app, "/api/challenge?public_key=nope").await;
    assert_eq!(
        (s, v["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("bad_request"))
    );
}

#[tokio::test]
async fn publishes_are_git_commits_with_tags() {
    let t = setup();
    let sk = new_key();
    for v in ["1.0.0", "1.1.0"] {
        let (s, _) = publish(&t.app, &sk, &skill("stored", v, "", &format!("body {v}"))).await;
        assert_eq!(s, StatusCode::CREATED);
    }
    let repo = git2::Repository::open_bare(t.hub.cfg.data_dir.join("repos/stored.git")).unwrap();
    let main = repo.refname_to_id("refs/heads/main").unwrap();
    assert_eq!(repo.refname_to_id("refs/tags/v1.1.0").unwrap(), main);
    let c = repo.find_commit(main).unwrap();
    assert_eq!(c.parent_count(), 1);
    let handle = t.hub.store().handle(&pk(&sk)).unwrap();
    assert!(handle.starts_with("k-") && handle.len() == 10, "{handle}");
    assert_eq!(c.author().name(), Some(handle.as_str()));
    assert_eq!(
        c.author().email(),
        Some(format!("{handle}@keys.skills.peckboard.com").as_str())
    );
    // No blob files any more.
    assert!(!t.hub.cfg.data_dir.join("blobs").exists());
}

#[tokio::test]
async fn backups_rotate() {
    let t = setup();
    let sk = new_key();
    assert_eq!(
        publish(&t.app, &sk, &skill("backed", "1.0.0", "", "x"))
            .await
            .0,
        StatusCode::CREATED
    );
    let dir = t.hub.cfg.data_dir.join("backups");
    for _ in 0..9 {
        let p = peckboard_skills_hub::backup::backup_if_due(
            &t.hub.store,
            &t.hub.repos,
            &dir,
            std::time::Duration::ZERO,
        )
        .unwrap()
        .unwrap();
        assert!(p.exists());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.iter().filter(|n| n.ends_with(".sqlite")).count(), 7);
    let tars: Vec<&String> = names.iter().filter(|n| n.ends_with(".repos.tar")).collect();
    assert_eq!(tars.len(), 7, "{names:?}");
    // The tarball holds the skill's repo.
    let mut ar = tar::Archive::new(std::fs::File::open(dir.join(tars[0])).unwrap());
    let paths: Vec<String> = ar
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        paths.iter().any(|p| p.starts_with("repos/backed.git/")),
        "{paths:?}"
    );
    // Not due again right away with the real interval.
    assert!(
        peckboard_skills_hub::backup::backup_if_due(
            &t.hub.store,
            &t.hub.repos,
            &dir,
            peckboard_skills_hub::backup::EVERY
        )
        .unwrap()
        .is_none()
    );
}

// ── signed-header requests, forks, merge requests ───────────────────────

/// The signed headers for `method path` with `body`, signed by `signer`
/// on behalf of `sk`.
async fn signed_req(
    app: &Router,
    sk: &SigningKey,
    signer: &SigningKey,
    method: &str,
    path: &str,
    body: &str,
) -> Request<Body> {
    let (nonce, bits) = challenge(app, sk).await;
    let pow = auth::solve_pow(&nonce, bits);
    let path_only = path.split('?').next().unwrap();
    let sig =
        signer.sign(auth::request_message(method, path_only, &nonce, body.as_bytes()).as_bytes());
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-peck-key", pk(sk))
        .header("x-peck-nonce", nonce)
        .header("x-peck-pow", pow)
        .header("x-peck-signature", STANDARD.encode(sig.to_bytes()))
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn signed(
    app: &Router,
    sk: &SigningKey,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let raw = body.map(|b| b.to_string()).unwrap_or_default();
    let req = signed_req(app, sk, sk, method, path, &raw).await;
    let (s, b) = call(app, req).await;
    (s, serde_json::from_str(&b).unwrap_or(Value::Null))
}

fn code(r: &(StatusCode, Value)) -> (StatusCode, String) {
    (r.0, r.1["code"].as_str().unwrap_or("").to_string())
}

async fn fork(app: &Router, sk: &SigningKey, name: &str) -> String {
    let (s, v) = signed(app, sk, "POST", "/api/fork", Some(json!({ "name": name }))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    v["fork"].as_str().unwrap().to_string()
}

async fn open_mr(app: &Router, sk: &SigningKey, fork: &str) -> (StatusCode, Value) {
    signed(
        app,
        sk,
        "POST",
        "/api/merge-requests",
        Some(json!({ "fork": fork, "title": "Improve it", "description": "Please merge." })),
    )
    .await
}

#[tokio::test]
async fn fork_publish_merge_request_and_merge_with_publish() {
    let t = setup();
    let (owner, alice, other) = (new_key(), new_key(), new_key());
    let base = "# Steps\n\n1. Read the diff.\n2. Check tests.\n\nDone.";
    let (s, _) = publish(&t.app, &owner, &skill("pr-review", "1.0.0", "", base)).await;
    assert_eq!(s, StatusCode::CREATED);

    let r = signed(
        &t.app,
        &alice,
        "POST",
        "/api/handle",
        Some(json!({"handle": "alice"})),
    )
    .await;
    assert_eq!(r.0, StatusCode::OK, "{}", r.1);
    let f = fork(&t.app, &alice, "pr-review").await;
    assert_eq!(f, "pr-review@alice");
    let r = signed(
        &t.app,
        &alice,
        "POST",
        "/api/fork",
        Some(json!({"name": "pr-review"})),
    )
    .await;
    assert_eq!(code(&r), (StatusCode::CONFLICT, "already_forked".into()));
    let r = signed(
        &t.app,
        &other,
        "POST",
        "/api/fork",
        Some(json!({"name": "pr-review@alice"})),
    )
    .await;
    assert_eq!(r.0, StatusCode::BAD_REQUEST);

    // Forks aren't catalog entries; the original counts them.
    let (_, idx) = get_json(&t.app, "/index.json").await;
    assert_eq!(idx["skills"].as_array().unwrap().len(), 1);
    assert_eq!(idx["skills"][0]["forks"], 1);

    // Only the fork's owner publishes to it.
    let changed = "# Steps\n\n1. Read the diff.\n2. Check tests and docs.\n\nDone.";
    let fork_md = skill(&f, "1.0.1", "", changed);
    let r = publish(&t.app, &other, &fork_md).await;
    assert_eq!(code(&r), (StatusCode::FORBIDDEN, "not_owner".into()));
    let r = publish(&t.app, &alice, &fork_md).await;
    assert_eq!(r.0, StatusCode::CREATED, "{}", r.1);
    assert_eq!(
        get(&t.app, "/skills/pr-review@alice/1.0.1/SKILL.md")
            .await
            .1,
        fork_md
    );

    let (_, forks) = get_json(&t.app, "/api/skills/pr-review/forks").await;
    assert_eq!(forks[0]["fork"], "pr-review@alice");
    assert_eq!(forks[0]["owner_handle"], "alice");
    assert_eq!(forks[0]["latest"], "1.0.1");
    assert_eq!(
        (forks[0]["ahead"].clone(), forks[0]["behind"].clone()),
        (json!(1), json!(0))
    );
    let (_, info) = get_json(&t.app, "/api/skills/pr-review@alice").await;
    assert_eq!(info["parent"], "pr-review");
    assert_eq!(info["owner_handle"], "alice");

    // Only the fork's owner opens a merge request from it.
    let r = open_mr(&t.app, &other, &f).await;
    assert_eq!(code(&r), (StatusCode::FORBIDDEN, "not_owner".into()));
    let (s, mr) = open_mr(&t.app, &alice, &f).await;
    assert_eq!(s, StatusCode::CREATED, "{mr}");
    assert_eq!(mr["state"], "open");
    assert_eq!(mr["target"], "pr-review");
    assert_eq!(mr["author_handle"], "alice");
    assert_eq!(mr["target_owner_key"], pk(&owner));
    assert_eq!(mr["mergeable"], true);
    let diff = mr["diff"].as_str().unwrap();
    assert!(diff.contains("+2. Check tests and docs."), "{diff}");
    assert!(diff.contains("-2. Check tests."), "{diff}");
    assert!(!diff.contains("@alice"), "{diff}");
    assert_eq!(mr["comments"], json!([]));
    let id = mr["id"].as_i64().unwrap();

    // Only the target's owner merges.
    let path = format!("/api/merge-requests/{id}/merge");
    let r = signed(
        &t.app,
        &alice,
        "POST",
        &path,
        Some(json!({"publish": true})),
    )
    .await;
    assert_eq!(code(&r), (StatusCode::FORBIDDEN, "not_owner".into()));
    let (s, merged) = signed(
        &t.app,
        &owner,
        "POST",
        &path,
        Some(json!({"publish": true, "bump": "minor"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{merged}");
    assert_eq!(merged["state"], "merged");
    assert_eq!(merged["merged_version"], "1.1.0");
    let merged_commit = merged["merged_commit"].as_str().unwrap().to_string();

    let (_, idx) = get_json(&t.app, "/index.json").await;
    assert_eq!(idx["skills"][0]["latest"], "1.1.0");
    assert_eq!(idx["skills"][0]["versions"], json!(["1.1.0", "1.0.0"]));
    let (_, md) = get(&t.app, "/skills/pr-review/1.1.0/SKILL.md").await;
    assert!(md.starts_with("---\nname: pr-review\n"), "{md}");
    assert!(
        md.contains("version: 1.1.0\n") && md.contains("and docs"),
        "{md}"
    );

    let (_, hist) = get_json(&t.app, "/api/skills/pr-review/history").await;
    let top = &hist[0];
    assert_eq!(top["commit"], merged_commit.as_str());
    assert_eq!(top["parents"].as_array().unwrap().len(), 2);
    assert_eq!(top["tag"], "v1.1.0");
    assert_eq!(top["author_key"], pk(&owner));
    assert!(
        top["message"].as_str().unwrap().starts_with("Merge !"),
        "{top}"
    );
    // The fork's commit is part of the history now (shared, not copied).
    assert!(
        hist.as_array()
            .unwrap()
            .iter()
            .any(|c| c["author_handle"] == "alice")
    );
    assert_eq!(hist.as_array().unwrap().len(), 3);

    let (_, info) = get_json(&t.app, "/api/skills/pr-review").await;
    assert_eq!(info["latest"], "1.1.0");
    assert_eq!(info["owner_key"], pk(&owner));
    assert!(info.get("parent").is_none());

    // Merged: not mergeable again.
    let r = signed(
        &t.app,
        &owner,
        "POST",
        &path,
        Some(json!({"publish": false})),
    )
    .await;
    assert_eq!(code(&r), (StatusCode::CONFLICT, "not_mergeable".into()));
}

#[tokio::test]
async fn conflicting_merge_request_sync_and_resolution() {
    let t = setup();
    let (owner, forker) = (new_key(), new_key());
    let body = |step: &str, extra: &str| format!("Line one.\n\nStep: {step}.\n\nTail.{extra}");
    assert_eq!(
        publish(
            &t.app,
            &owner,
            &skill("lint", "1.0.0", "", &body("old", ""))
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let f = fork(&t.app, &forker, "lint").await;
    assert!(f.starts_with("lint@k-"), "{f}");
    let r = publish(
        &t.app,
        &forker,
        &skill(&f, "1.0.1", "", &body("fork change", "")),
    )
    .await;
    assert_eq!(r.0, StatusCode::CREATED, "{}", r.1);
    let r = publish(
        &t.app,
        &owner,
        &skill("lint", "1.1.0", "", &body("upstream change", "")),
    )
    .await;
    assert_eq!(r.0, StatusCode::CREATED);

    let (s, mr) = open_mr(&t.app, &forker, &f).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(mr["mergeable"], false);
    let id = mr["id"].as_i64().unwrap();
    let first_head = mr["head_commit"].as_str().unwrap().to_string();
    let r = signed(
        &t.app,
        &owner,
        "POST",
        &format!("/api/merge-requests/{id}/merge"),
        Some(json!({"publish": true})),
    )
    .await;
    assert_eq!(code(&r), (StatusCode::CONFLICT, "not_mergeable".into()));

    // Sync conflicts and changes nothing.
    let sync = format!("/api/forks/{f}/sync");
    let r = signed(&t.app, &owner, "POST", &sync, None).await;
    assert_eq!(code(&r), (StatusCode::FORBIDDEN, "not_owner".into()));
    let (s, v) = signed(&t.app, &forker, "POST", &sync, None).await;
    assert_eq!(
        (s, v["status"].clone()),
        (StatusCode::OK, json!("conflict"))
    );
    let (_, hist) = get_json(&t.app, &format!("/api/skills/{f}/history")).await;
    assert_eq!(hist[0]["commit"], first_head.as_str());

    // The fork's owner publishes a version that takes the upstream line and
    // keeps their own addition elsewhere: the MR follows and merges.
    let resolved = skill(
        &f,
        "1.0.2",
        "",
        &body("upstream change", "\n\nExtra: fork addition."),
    );
    let r = publish(&t.app, &forker, &resolved).await;
    assert_eq!(r.0, StatusCode::CREATED, "{}", r.1);
    let (_, mr) = get_json(&t.app, &format!("/api/merge-requests/{id}")).await;
    assert_ne!(mr["head_commit"], first_head.as_str());
    assert_eq!(mr["mergeable"], true, "{mr}");

    // Now sync merges cleanly too, and the MR stays mergeable.
    let (s, v) = signed(&t.app, &forker, "POST", &sync, None).await;
    assert_eq!((s, v["status"].clone()), (StatusCode::OK, json!("merged")));
    let (_, mr) = get_json(&t.app, &format!("/api/merge-requests/{id}")).await;
    assert_eq!(mr["mergeable"], true, "{mr}");
    let (_, forks) = get_json(&t.app, "/api/skills/lint/forks").await;
    assert_eq!(forks[0]["behind"], 0);

    // Merge without publishing: a merge commit, no new version.
    let (s, merged) = signed(
        &t.app,
        &owner,
        "POST",
        &format!("/api/merge-requests/{id}/merge"),
        Some(json!({"publish": false})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{merged}");
    assert_eq!(merged["merged_version"], Value::Null);
    let (_, idx) = get_json(&t.app, "/index.json").await;
    assert_eq!(idx["skills"][0]["latest"], "1.1.0");
    let (_, d) = get_json(&t.app, "/api/skills/lint/diff?from=1.1.0&to=main").await;
    let diff = d["diff"].as_str().unwrap();
    assert!(diff.contains("+Extra: fork addition."), "{diff}");
    assert!(
        !diff.contains("name:") && !diff.contains("version:"),
        "{diff}"
    );
}

#[tokio::test]
async fn merge_request_comments_close_and_listing() {
    let t = setup();
    let (owner, author, stranger) = (new_key(), new_key(), new_key());
    assert_eq!(
        publish(&t.app, &owner, &skill("notes", "1.0.0", "", "a\n\nb"))
            .await
            .0,
        StatusCode::CREATED
    );
    let f = fork(&t.app, &author, "notes").await;
    let r = publish(&t.app, &author, &skill(&f, "1.0.1", "", "a\n\nc")).await;
    assert_eq!(r.0, StatusCode::CREATED);
    let (_, mr) = open_mr(&t.app, &author, &f).await;
    let id = mr["id"].as_i64().unwrap();
    let r = open_mr(&t.app, &author, &f).await;
    assert_eq!(r.0, StatusCode::CONFLICT);

    let comments = format!("/api/merge-requests/{id}/comments");
    let r = signed(
        &t.app,
        &stranger,
        "POST",
        &comments,
        Some(json!({"body": "drive-by"})),
    )
    .await;
    assert_eq!(code(&r), (StatusCode::FORBIDDEN, "not_owner".into()));
    let (s, c) = signed(
        &t.app,
        &author,
        "POST",
        &comments,
        Some(json!({"body": "Ready."})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    assert_eq!(c["author_key"], pk(&author));
    assert!(c["author_handle"].as_str().unwrap().starts_with("k-"));
    let (s, _) = signed(
        &t.app,
        &owner,
        "POST",
        &comments,
        Some(json!({"body": "Thanks!"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let r = signed(
        &t.app,
        &owner,
        "POST",
        &comments,
        Some(json!({"body": "  "})),
    )
    .await;
    assert_eq!(r.0, StatusCode::BAD_REQUEST);
    let (_, detail) = get_json(&t.app, &format!("/api/merge-requests/{id}")).await;
    let bodies: Vec<&str> = detail["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, vec!["Ready.", "Thanks!"]);

    // Lists: by target, by owner, by author; no diff/comments in lists.
    let (_, l) = get_json(&t.app, "/api/merge-requests?target=notes").await;
    assert_eq!(l.as_array().unwrap().len(), 1);
    assert!(l[0].get("diff").is_none() && l[0].get("comments").is_none());
    let (_, l) = get_json(
        &t.app,
        &format!(
            "/api/merge-requests?key={}&role=owner&state=all",
            url_key(&owner)
        ),
    )
    .await;
    assert_eq!(l[0]["id"], id);
    let (_, l) = get_json(
        &t.app,
        &format!("/api/merge-requests?key={}&role=author", url_key(&author)),
    )
    .await;
    assert_eq!(l[0]["id"], id);
    let (_, l) = get_json(
        &t.app,
        &format!("/api/merge-requests?key={}&role=author", url_key(&owner)),
    )
    .await;
    assert_eq!(l, json!([]));

    // Closing: target owner or author only.
    let close = format!("/api/merge-requests/{id}/close");
    let r = signed(&t.app, &stranger, "POST", &close, None).await;
    assert_eq!(code(&r), (StatusCode::FORBIDDEN, "not_owner".into()));
    let (s, v) = signed(&t.app, &author, "POST", &close, None).await;
    assert_eq!((s, v["state"].clone()), (StatusCode::OK, json!("closed")));
    let (_, l) = get_json(&t.app, "/api/merge-requests?target=notes&state=closed").await;
    assert_eq!(l.as_array().unwrap().len(), 1);
    let (_, l) = get_json(&t.app, "/api/merge-requests?target=notes").await;
    assert_eq!(l, json!([]));
    let r = signed(
        &t.app,
        &owner,
        "POST",
        &format!("/api/merge-requests/{id}/merge"),
        Some(json!({"publish": false})),
    )
    .await;
    assert_eq!(code(&r), (StatusCode::CONFLICT, "not_mergeable".into()));
    // A closed MR frees the fork for a new one.
    assert_eq!(open_mr(&t.app, &author, &f).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn handles_default_set_and_taken() {
    let t = setup();
    let (a, b) = (new_key(), new_key());
    let (s, v) = get_json(&t.app, &format!("/api/publishers/{}", url_key(&a))).await;
    assert_eq!(s, StatusCode::OK);
    let default = v["handle"].as_str().unwrap().to_string();
    let expect = format!(
        "k-{}",
        &auth::sha256_hex(&a.verifying_key().to_bytes())[..8]
    );
    assert_eq!(default, expect);
    assert_eq!(v["public_key"], pk(&a));

    let r = signed(
        &t.app,
        &a,
        "POST",
        "/api/handle",
        Some(json!({"handle": "alice"})),
    )
    .await;
    assert_eq!(
        (r.0, r.1["handle"].clone()),
        (StatusCode::OK, json!("alice"))
    );
    let r = signed(
        &t.app,
        &b,
        "POST",
        "/api/handle",
        Some(json!({"handle": "alice"})),
    )
    .await;
    assert_eq!(code(&r), (StatusCode::CONFLICT, "handle_taken".into()));
    for bad in ["Alice", "a", "no spaces", "k-12345678"] {
        let r = signed(
            &t.app,
            &b,
            "POST",
            "/api/handle",
            Some(json!({"handle": bad})),
        )
        .await;
        assert_eq!(
            code(&r),
            (StatusCode::BAD_REQUEST, "bad_request".into()),
            "{bad}"
        );
    }
    // Re-setting your own handle is fine; so is switching.
    let r = signed(
        &t.app,
        &a,
        "POST",
        "/api/handle",
        Some(json!({"handle": "alice"})),
    )
    .await;
    assert_eq!(r.0, StatusCode::OK);
    let (_, v) = get_json(&t.app, &format!("/api/publishers/{}", url_key(&a))).await;
    assert_eq!(v["handle"], "alice");
}

#[tokio::test]
async fn diff_between_versions() {
    let t = setup();
    let sk = new_key();
    for (v, line) in [("1.0.0", "old line"), ("1.1.0", "new line")] {
        let (s, _) = publish(
            &t.app,
            &sk,
            &skill("diffy", v, "", &format!("Intro.\n\n{line}")),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }
    let (s, d) = get_json(&t.app, "/api/skills/diffy/diff?from=1.0.0&to=1.1.0").await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let diff = d["diff"].as_str().unwrap();
    assert!(
        diff.contains("--- a/SKILL.md") && diff.contains("+++ b/SKILL.md"),
        "{diff}"
    );
    assert!(
        diff.contains("-old line") && diff.contains("+new line"),
        "{diff}"
    );
    assert!(
        diff.contains("-version: 1.0.0") && diff.contains("+version: 1.1.0"),
        "{diff}"
    );
    // `main` and commit ids resolve too.
    let (_, d2) = get_json(&t.app, "/api/skills/diffy/diff?from=1.0.0&to=main").await;
    assert_eq!(d2["diff"], d["diff"]);
    let from = d["from"].as_str().unwrap();
    let (s, d3) = get_json(
        &t.app,
        &format!("/api/skills/diffy/diff?from={}&to=1.1.0", &from[..12]),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d3["diff"], d["diff"]);
    // The current head is readable as text, uncached.
    let (s, head) = get(&t.app, "/skills/diffy/main/SKILL.md").await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        head.contains("new line") && head.contains("version: 1.1.0"),
        "{head}"
    );
    assert_eq!(
        get(&t.app, "/skills/nope/main/SKILL.md").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&t.app, "/api/skills/diffy/diff?from=9.9.9&to=main")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&t.app, "/api/skills/diffy/diff?from=1.0.0").await.0,
        StatusCode::BAD_REQUEST
    );
    // A taken-down version can't be diffed.
    admin::takedown(&t.hub.store(), "diffy", Some("1.0.0")).unwrap();
    assert_eq!(
        get(
            &t.app,
            &format!("/api/skills/diffy/diff?from={from}&to=main")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn signed_headers_refuse_replay_and_forgery() {
    let t = setup();
    let sk = new_key();
    let body = json!({"handle": "replayer"}).to_string();
    let req = signed_req(&t.app, &sk, &sk, "POST", "/api/handle", &body).await;
    let headers = req.headers().clone();
    let (s, _) = call(&t.app, req).await;
    assert_eq!(s, StatusCode::OK);
    // The same signed request again: the nonce is spent.
    let mut again = Request::post("/api/handle")
        .body(Body::from(body.clone()))
        .unwrap();
    *again.headers_mut() = headers;
    let (s, b) = call(&t.app, again).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(b.contains("nonce_invalid"), "{b}");

    // Signed for another path, by another key, or over another body.
    let other = new_key();
    let mut req = signed_req(&t.app, &sk, &sk, "POST", "/api/fork", &body).await;
    *req.uri_mut() = "/api/handle".parse().unwrap();
    let (s, b) = call(&t.app, req).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(b.contains("bad_signature"), "{b}");
    let req = signed_req(&t.app, &sk, &other, "POST", "/api/handle", &body).await;
    assert!(call(&t.app, req).await.1.contains("bad_signature"));
    let req = signed_req(&t.app, &sk, &sk, "POST", "/api/handle", &body).await;
    let (parts, _) = req.into_parts();
    let tampered = Request::from_parts(parts, Body::from(json!({"handle": "evil"}).to_string()));
    assert!(call(&t.app, tampered).await.1.contains("bad_signature"));
    // Unsigned.
    let (s, v) = post_json(&t.app, "/api/handle", &json!({"handle": "x"})).await;
    assert_eq!(
        (s, v["code"].clone()),
        (StatusCode::UNAUTHORIZED, json!("bad_signature"))
    );
}

#[tokio::test]
async fn blob_storage_migrates_into_git() {
    let dir = tempfile::tempdir().unwrap();
    let owner = pk(&new_key());
    let v1 = skill("legacy", "1.0.0", "pack", "first");
    let v2 = skill("legacy", "1.1.0", "pack", "second");
    {
        // A data dir as the blob-storage release left it.
        let conn = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE publishers (public_key TEXT PRIMARY KEY, first_seen TEXT NOT NULL,
                banned INTEGER NOT NULL DEFAULT 0, publish_count_day INTEGER NOT NULL DEFAULT 0,
                day TEXT NOT NULL DEFAULT '');
            CREATE TABLE skills (name TEXT PRIMARY KEY, owner_key TEXT NOT NULL, created_at TEXT NOT NULL);
            CREATE TABLE versions (name TEXT NOT NULL REFERENCES skills(name), version TEXT NOT NULL,
                sha256 TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', tags TEXT NOT NULL DEFAULT '[]',
                "groups" TEXT NOT NULL DEFAULT '[]', published_at TEXT NOT NULL,
                yanked INTEGER NOT NULL DEFAULT 0, removed INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (name, version));
            CREATE TABLE "groups" (slug TEXT PRIMARY KEY, name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '', owner_key TEXT NOT NULL);
            CREATE TABLE audit_log (id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT NOT NULL,
                action TEXT NOT NULL, public_key TEXT, detail TEXT NOT NULL DEFAULT '');
            "#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO publishers (public_key, first_seen) VALUES (?1, '2025-01-01T00:00:00Z')",
            [&owner],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skills VALUES ('legacy', ?1, '2025-01-01T00:00:00Z')",
            [&owner],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO \"groups\" VALUES ('pack', 'pack', '', ?1)",
            [&owner],
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("blobs")).unwrap();
        // Inserted newest first: migration goes by published_at.
        for (v, text, at) in [
            ("1.1.0", &v2, "2025-02-01T00:00:00Z"),
            ("1.0.0", &v1, "2025-01-01T00:00:00Z"),
        ] {
            let sha = auth::sha256_hex(text.as_bytes());
            std::fs::write(dir.path().join("blobs").join(&sha), text).unwrap();
            conn.execute(
                r#"INSERT INTO versions (name, version, sha256, description, tags, "groups", published_at)
                   VALUES ('legacy', ?1, ?2, 'A test skill', '["test"]', '["pack"]', ?3)"#,
                [v, &sha, at],
            )
            .unwrap();
        }
        // One version whose blob went missing: left alone, not fatal.
        conn.execute(
            "INSERT INTO versions (name, version, sha256, published_at) VALUES ('legacy', '0.9.0', 'abcd', '2024-12-01T00:00:00Z')",
            [],
        )
        .unwrap();
    }
    let open = || {
        let mut cfg = Config::new("skills.test", dir.path().to_path_buf());
        cfg.new_key_pow_bits = 6;
        let hub = Hub::open(cfg).unwrap();
        let app = api::router(hub.clone());
        (hub, app)
    };
    let (hub, app) = open();
    assert!(dir.path().join("repos/legacy.git/HEAD").is_file());
    assert_eq!(get(&app, "/skills/legacy/1.0.0/SKILL.md").await.1, v1);
    assert_eq!(get(&app, "/skills/legacy/1.1.0/SKILL.md").await.1, v2);
    let (_, idx) = get_json(&app, "/index.json").await;
    assert_eq!(idx["skills"][0]["latest"], "1.1.0");
    assert_eq!(idx["groups"][0]["skills"], json!(["legacy"]));
    let (_, hist) = get_json(&app, "/api/skills/legacy/history").await;
    let hist = hist.as_array().unwrap().clone();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0]["tag"], "v1.1.0");
    assert_eq!(hist[1]["tag"], "v1.0.0");
    assert_eq!(hist[1]["at"], "2025-01-01T00:00:00Z");
    assert_eq!(hist[0]["author_key"], owner.as_str());
    drop((hub, app));
    // Reopening doesn't migrate twice.
    let (_hub, app) = open();
    let (_, again) = get_json(&app, "/api/skills/legacy/history").await;
    assert_eq!(again.as_array().unwrap().len(), 2);
    // The version whose blob was missing stays unserved.
    assert_eq!(
        get(&app, "/skills/legacy/0.9.0/SKILL.md").await.0,
        StatusCode::NOT_FOUND
    );
}
