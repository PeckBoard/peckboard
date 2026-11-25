//! End-to-end tests of the team endpoints against an in-process router,
//! with real Ed25519 signing — the same code path a PeckBoard server uses.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use peckboard_skills_hub::{Config, Hub, api, auth};
use peckboard_team_format::{
    Action, Invite, Record, Role, gate, join_proof, log_message, replay, verification_code,
};
use serde_json::{Value, json};
use tower::ServiceExt;

struct T {
    app: Router,
    _hub: Arc<Hub>,
    _dir: tempfile::TempDir,
}

fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::new("team.test", dir.path().to_path_buf());
    cfg.new_key_pow_bits = 4;
    let hub = Hub::open(cfg).unwrap();
    T {
        app: api::router(hub.clone()),
        _hub: hub,
        _dir: dir,
    }
}

fn new_key() -> SigningKey {
    SigningKey::from_bytes(&rand::random::<[u8; 32]>())
}

fn pk(sk: &SigningKey) -> String {
    STANDARD.encode(sk.verifying_key().to_bytes())
}

fn verify(key: &str, msg: &str, signature: &str) -> bool {
    auth::parse_public_key(key).is_some_and(|(vk, _)| auth::verify(&vk, msg, signature))
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// One signed request: challenge, proof of work, `X-Peck-*` headers.
async fn signed(
    app: &Router,
    sk: &SigningKey,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let url_key = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
    let (s, c) = send(
        app,
        Request::get(format!("/api/challenge?public_key={url_key}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    let nonce = c["nonce"].as_str().unwrap();
    let pow = auth::solve_pow(nonce, c["pow_difficulty"].as_u64().unwrap() as u32);
    let raw = body.map(Value::to_string).unwrap_or_default();
    let sig = sk.sign(auth::request_message(method, path, nonce, raw.as_bytes()).as_bytes());
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(auth::H_KEY, pk(sk))
        .header(auth::H_NONCE, nonce)
        .header(auth::H_POW, pow)
        .header(auth::H_SIGNATURE, STANDARD.encode(sig.to_bytes()))
        .header("content-type", "application/json")
        .body(Body::from(raw))
        .unwrap();
    send(app, req).await
}

/// A signed log record following `(len, head)`; `None` founds a team.
fn record(sk: &SigningKey, tip: Option<(u64, &str)>, action: &Action) -> Record {
    let body = serde_json::to_string(action).unwrap();
    let (seq, prev) = tip.map_or((0, String::new()), |(l, h)| (l, h.to_string()));
    let at = "2026-11-25T10:00:00Z".to_string();
    let sig = sk.sign(log_message(seq, &prev, &at, &body).as_bytes());
    Record {
        seq,
        prev,
        at,
        body,
        signer: pk(sk),
        signature: STANDARD.encode(sig.to_bytes()),
    }
}

async fn create_team(app: &Router, owner: &SigningKey) -> String {
    let r = record(
        owner,
        None,
        &Action::Create {
            name: "Acme".into(),
            founder: pk(owner),
            label: "office server".into(),
            salt: hex::encode(rand::random::<[u8; 16]>()),
        },
    );
    let (s, v) = signed(
        app,
        owner,
        "POST",
        "/api/teams",
        Some(&json!({ "record": r })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["name"], "Acme");
    v["team_id"].as_str().unwrap().to_string()
}

/// The team's log as a member reads it, replayed and checked.
async fn read_team(app: &Router, member: &SigningKey, id: &str) -> peckboard_team_format::Team {
    let (s, v) = signed(app, member, "GET", &format!("/api/teams/{id}/log"), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let records: Vec<Record> = serde_json::from_value(v["records"].clone()).unwrap();
    let team = replay(&records, &verify).unwrap();
    assert_eq!(team.id, id, "the id is the hash of the first record");
    team
}

async fn append(
    app: &Router,
    signer: &SigningKey,
    id: &str,
    action: &Action,
    request_id: Option<&str>,
) -> (StatusCode, Value) {
    let team = read_team(app, signer, id).await;
    let r = record(signer, Some((team.len, &team.head)), action);
    signed(
        app,
        signer,
        "POST",
        &format!("/api/teams/{id}/log"),
        Some(&json!({ "record": r, "request_id": request_id })),
    )
    .await
}

fn invite(id: &str) -> Invite {
    Invite {
        hub: "https://team.test".into(),
        team_id: id.to_string(),
        invite_id: format!("inv_{}", hex::encode(rand::random::<[u8; 6]>())),
        secret: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
    }
}

async fn register_invite(app: &Router, by: &SigningKey, inv: &Invite) -> (StatusCode, Value) {
    let expires = (chrono::Utc::now() + chrono::Duration::days(2)).to_rfc3339();
    signed(
        app,
        by,
        "POST",
        &format!("/api/teams/{}/invites", inv.team_id),
        Some(&json!({ "invite_id": inv.invite_id, "gate": gate(&inv.secret), "expires_at": expires })),
    )
    .await
}

async fn ask_to_join(
    app: &Router,
    joiner: &SigningKey,
    inv: &Invite,
    label: &str,
) -> (StatusCode, Value) {
    signed(
        app,
        joiner,
        "POST",
        &format!("/api/teams/{}/join", inv.team_id),
        Some(&json!({
            "invite_id": inv.invite_id,
            "gate": gate(&inv.secret),
            "proof": join_proof(&inv.secret, &inv.team_id, &pk(joiner)),
            "label": label,
        })),
    )
    .await
}

#[tokio::test]
async fn invite_request_verify_approve_makes_a_member() {
    let t = setup();
    let (owner, joiner) = (new_key(), new_key());
    let id = create_team(&t.app, &owner).await;

    // The owner registers an invite; the link travels outside the hub.
    let inv = invite(&id);
    let (s, v) = register_invite(&t.app, &owner, &inv).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let link = inv.link();
    let pasted = Invite::parse(&link).unwrap();

    // The invited server previews the team with the invite alone.
    let preview = Request::get(format!("/api/teams/{id}/preview"))
        .header(
            "x-peck-invite",
            format!("{}.{}", pasted.invite_id, gate(&pasted.secret)),
        )
        .body(Body::empty())
        .unwrap();
    let (s, v) = send(&t.app, preview).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "Acme");
    assert_eq!(v["members"], 1);

    // Not a member yet: it cannot read the team.
    let (s, v) = signed(
        &t.app,
        &joiner,
        "GET",
        &format!("/api/teams/{id}/log"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "not_member");

    let (s, v) = ask_to_join(&t.app, &joiner, &pasted, "laptop").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let request_id = v["request_id"].as_str().unwrap().to_string();
    let state_path = format!("/api/teams/{id}/requests/{request_id}");
    let (_, v) = signed(&t.app, &joiner, "GET", &state_path, None).await;
    assert_eq!(v["state"], "pending");

    // The owner sees the request, checks the proof with the secret only it
    // and the invited server hold, and reads the same code the joiner does.
    let (s, v) = signed(
        &t.app,
        &owner,
        "GET",
        &format!("/api/teams/{id}/requests"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let req = &v["requests"][0];
    assert_eq!(req["label"], "laptop");
    let joiner_key = req["key"].as_str().unwrap();
    assert_eq!(joiner_key, pk(&joiner));
    assert_eq!(req["proof"], join_proof(&inv.secret, &id, joiner_key));
    assert_eq!(
        verification_code(&inv.secret, &id, joiner_key),
        verification_code(&pasted.secret, &id, &pk(&joiner)),
    );

    let add = Action::Add {
        key: joiner_key.to_string(),
        role: Role::Member,
        label: "laptop".into(),
    };
    let (s, v) = append(&t.app, &owner, &id, &add, Some(&request_id)).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // The joiner learns it was approved and can now read and check the log.
    let (_, v) = signed(&t.app, &joiner, "GET", &state_path, None).await;
    assert_eq!(v["state"], "approved");
    let team = read_team(&t.app, &joiner, &id).await;
    assert_eq!(team.role_of(&pk(&joiner)), Some(Role::Member));
    assert_eq!(team.role_of(&pk(&owner)), Some(Role::Owner));
    let (s, v) = signed(&t.app, &joiner, "GET", "/api/teams", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["teams"][0]["team_id"], id);
    assert_eq!(v["teams"][0]["role"], "member");

    // The invite is used up.
    let (s, v) = ask_to_join(&t.app, &new_key(), &pasted, "late").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "invite_invalid");
}

#[tokio::test]
async fn a_leaked_link_gives_a_different_code_and_no_membership() {
    let t = setup();
    let (owner, coworker, thief) = (new_key(), new_key(), new_key());
    let id = create_team(&t.app, &owner).await;
    let inv = invite(&id);
    register_invite(&t.app, &owner, &inv).await;

    // The thief asks first; the coworker can still ask.
    let (s, _) = ask_to_join(&t.app, &thief, &inv, "laptop").await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = ask_to_join(&t.app, &coworker, &inv, "laptop").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let coworker_request = v["request_id"].as_str().unwrap().to_string();

    let (_, v) = signed(
        &t.app,
        &owner,
        "GET",
        &format!("/api/teams/{id}/requests"),
        None,
    )
    .await;
    let requests = v["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    // Same label, different keys, different codes: the code the coworker
    // reads out matches only their own request.
    let spoken = verification_code(&inv.secret, &id, &pk(&coworker));
    let matching: Vec<&Value> = requests
        .iter()
        .filter(|r| verification_code(&inv.secret, &id, r["key"].as_str().unwrap()) == spoken)
        .collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0]["key"], pk(&coworker));

    // Without the link, a request does not even get filed.
    let mut guess = inv.clone();
    guess.secret = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let (s, v) = ask_to_join(&t.app, &new_key(), &guess, "guess").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");

    // Approving the coworker closes the thief's request.
    let add = Action::Add {
        key: pk(&coworker),
        role: Role::Member,
        label: "laptop".into(),
    };
    let (s, v) = append(&t.app, &owner, &id, &add, Some(&coworker_request)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = signed(
        &t.app,
        &owner,
        "GET",
        &format!("/api/teams/{id}/requests"),
        None,
    )
    .await;
    assert_eq!(v["requests"], json!([]));
    let (s, _) = signed(&t.app, &thief, "GET", &format!("/api/teams/{id}/log"), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_hub_refuses_changes_the_log_rules_forbid() {
    let t = setup();
    let (owner, member, stranger) = (new_key(), new_key(), new_key());
    let id = create_team(&t.app, &owner).await;
    let add_member = Action::Add {
        key: pk(&member),
        role: Role::Member,
        label: "laptop".into(),
    };
    let (s, v) = append(&t.app, &owner, &id, &add_member, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let team = read_team(&t.app, &owner, &id).await;

    // A stranger signs a well-formed record: not a member, so no.
    let add_self = Action::Add {
        key: pk(&stranger),
        role: Role::Owner,
        label: "me".into(),
    };
    let r = record(&stranger, Some((team.len, &team.head)), &add_self);
    let path = format!("/api/teams/{id}/log");
    let (s, v) = signed(
        &t.app,
        &stranger,
        "POST",
        &path,
        Some(&json!({ "record": r })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "not_member");

    // A plain member cannot add anyone, or register an invite.
    let (s, v) = append(&t.app, &member, &id, &add_self, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "not_allowed");
    let (s, v) = register_invite(&t.app, &member, &invite(&id)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");

    // A member cannot send a record signed with the owner's name on it.
    let mut forged = record(&member, Some((team.len, &team.head)), &add_self);
    forged.signer = pk(&owner);
    let (s, v) = signed(
        &t.app,
        &member,
        "POST",
        &path,
        Some(&json!({ "record": forged })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // Two changes made from the same state: the second is told to reload.
    let rename = record(
        &owner,
        Some((team.len, &team.head)),
        &Action::Rename {
            name: "Acme Ltd".into(),
        },
    );
    let stale = record(
        &owner,
        Some((team.len, &team.head)),
        &Action::Rename {
            name: "Other".into(),
        },
    );
    let (s, _) = signed(
        &t.app,
        &owner,
        "POST",
        &path,
        Some(&json!({ "record": rename })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = signed(
        &t.app,
        &owner,
        "POST",
        &path,
        Some(&json!({ "record": stale })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "log_conflict");

    // The last owner cannot leave; a removed member loses access at once.
    let (s, v) = append(
        &t.app,
        &owner,
        &id,
        &Action::Remove { key: pk(&owner) },
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, _) = append(
        &t.app,
        &owner,
        &id,
        &Action::Remove { key: pk(&member) },
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = signed(&t.app, &member, "GET", &path, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (_, v) = signed(&t.app, &member, "GET", "/api/teams", None).await;
    assert_eq!(v["teams"], json!([]));
    assert_eq!(read_team(&t.app, &owner, &id).await.name, "Acme Ltd");
}
