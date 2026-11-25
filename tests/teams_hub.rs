//! Teams end to end: two PeckBoard servers (two databases, two identity
//! keys) and the real team hub, run in-process on a local port. Covers the
//! whole life of a membership — create, invite, ask to join, compare the
//! code, approve, promote, remove — and that a server refuses a history
//! that does not continue the one it already verified.
//!
//! One test function on purpose: the hub address is a process-wide
//! environment variable.

use std::path::Path;

use peckboard::db::Db;
use peckboard::service::teams;
use peckboard_skills_hub::{Config, Hub, api};

struct Server {
    db: Db,
    dir: tempfile::TempDir,
}

impl Server {
    fn new() -> Self {
        Self {
            db: Db::in_memory().unwrap(),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    async fn key(&self) -> String {
        let v = teams::overview(&self.db, self.dir()).await.unwrap();
        v["server_key"].as_str().unwrap().to_string()
    }
}

async fn start_hub() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::new("team.test", dir.path().to_path_buf());
    cfg.new_key_pow_bits = 4;
    let hub = Hub::open(cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::router(hub)).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn two_servers_form_a_team_through_the_real_hub() {
    let (hub_url, _hub_dir) = start_hub().await;
    // SAFETY: set once, before any other thread reads the environment in
    // this test binary (this is its only test).
    unsafe { std::env::set_var("PECKBOARD_TEAM_HUB_URL", &hub_url) };

    let (office, laptop, stranger) = (Server::new(), Server::new(), Server::new());
    let laptop_key = laptop.key().await;

    // The office server founds the team and is its owner.
    let team = teams::create(&office.db, office.dir(), "  Acme ", "office")
        .await
        .unwrap();
    assert_eq!(team.name, "Acme");
    assert_eq!(team.state, "member");
    assert_eq!(team.role.as_deref(), Some("owner"));
    assert_eq!(team.members.len(), 1);
    assert!(team.members[0].is_self);
    let id = team.id.clone();

    // It invites one server. The link carries the secret; the hub does not.
    let invite = teams::create_invite(&office.db, office.dir(), &id, None)
        .await
        .unwrap();
    let link = invite["link"].as_str().unwrap().to_string();
    assert!(link.starts_with(&format!("{hub_url}/join#{id}.")), "{link}");

    // The laptop looks at the invite, then asks to join.
    let preview = teams::preview_invite(&link).await.unwrap();
    assert_eq!(preview["name"], "Acme");
    assert_eq!(preview["members"], 1);
    let pending = teams::request_join(&laptop.db, laptop.dir(), &link, "laptop")
        .await
        .unwrap();
    assert_eq!(pending.state, "pending");
    let spoken_code = pending.pending_code.clone().expect("a code to read out");
    assert!(pending.members.is_empty(), "nothing verified yet");

    // Still pending after a refresh: nobody approved it.
    let still = teams::refresh(&laptop.db, laptop.dir(), &id).await.unwrap();
    assert_eq!(still.state, "pending");

    // Another server asks with the same link, then takes its request back:
    // it is gone for the people who could approve it.
    teams::request_join(&stranger.db, stranger.dir(), &link, "wrong one")
        .await
        .unwrap();
    teams::forget(&stranger.db, stranger.dir(), &id)
        .await
        .unwrap();

    // The office sees the request, vouched for by the invite it created,
    // with the same code the laptop shows.
    let listed = teams::list_requests(&office.db, office.dir(), &id)
        .await
        .unwrap();
    let requests = listed["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request["label"], "laptop");
    assert_eq!(request["key"], laptop_key.as_str());
    assert_eq!(request["can_approve"], true);
    assert_eq!(request["genuine"], true);
    assert_eq!(request["code"], spoken_code.as_str());
    let request_id = request["request_id"].as_str().unwrap();

    let approved = teams::approve_request(&office.db, office.dir(), &id, request_id)
        .await
        .unwrap();
    assert_eq!(approved.members.len(), 2);
    // The invite is used up on both sides.
    let open = teams::list_invites(&office.db, &id).await.unwrap();
    assert_eq!(open["invites"], serde_json::json!([]));
    let err = teams::request_join(&stranger.db, stranger.dir(), &link, "late")
        .await
        .unwrap_err();
    assert_eq!(err.code, "invite_invalid", "{}", err.message);

    // The laptop learns it is in, having checked the whole history itself.
    let joined = teams::refresh(&laptop.db, laptop.dir(), &id).await.unwrap();
    assert_eq!(joined.state, "member");
    assert_eq!(joined.role.as_deref(), Some("member"));
    assert_eq!(joined.members.len(), 2);
    assert!(joined.pending_code.is_none());

    // A plain member cannot invite or rename; an owner can promote it.
    let err = teams::create_invite(&laptop.db, laptop.dir(), &id, None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "not_allowed");
    let err = teams::rename(&laptop.db, laptop.dir(), &id, "Mine")
        .await
        .unwrap_err();
    assert_eq!(err.code, "not_allowed", "{}", err.message);
    teams::set_role(&office.db, office.dir(), &id, &laptop_key, "admin")
        .await
        .unwrap();
    teams::rename(&office.db, office.dir(), &id, "Acme Ltd")
        .await
        .unwrap();
    let promoted = teams::refresh(&laptop.db, laptop.dir(), &id).await.unwrap();
    assert_eq!(promoted.name, "Acme Ltd");
    assert_eq!(promoted.role.as_deref(), Some("admin"));
    teams::create_invite(&laptop.db, laptop.dir(), &id, Some(1))
        .await
        .expect("an admin can invite");

    // The last owner cannot leave.
    let err = teams::leave(&office.db, office.dir(), &id)
        .await
        .unwrap_err();
    assert_eq!(err.code, "bad_record", "{}", err.message);

    // A history that does not continue the one the laptop verified is
    // refused, and nothing it knew is thrown away.
    let mut row = laptop.db.get_team(&id).await.unwrap().unwrap();
    let real_head = row.log_head.clone();
    row.log_head = "0".repeat(64);
    laptop.db.save_team(row, None).await.unwrap();
    let err = teams::refresh(&laptop.db, laptop.dir(), &id)
        .await
        .unwrap_err();
    assert_eq!(err.code, "history_mismatch", "{}", err.message);
    let kept = teams::get(&laptop.db, laptop.dir(), &id).await.unwrap();
    assert_eq!(kept.state, "member");
    assert_eq!(kept.members.len(), 2);
    assert!(kept.last_error.is_some(), "the page shows what went wrong");
    let mut row = laptop.db.get_team(&id).await.unwrap().unwrap();
    row.log_head = real_head;
    laptop.db.save_team(row, None).await.unwrap();

    // Removed by the owner: the laptop finds out on its next refresh and
    // can then drop the team from its list.
    teams::remove_member(&office.db, office.dir(), &id, &laptop_key)
        .await
        .unwrap();
    let removed = teams::refresh(&laptop.db, laptop.dir(), &id).await.unwrap();
    assert_eq!(removed.state, "removed");
    assert_eq!(removed.role, None);
    let err = teams::forget(&office.db, office.dir(), &id)
        .await
        .unwrap_err();
    assert_eq!(err.code, "bad_request", "a member must leave first");
    teams::forget(&laptop.db, laptop.dir(), &id).await.unwrap();
    let after = teams::overview(&laptop.db, laptop.dir()).await.unwrap();
    assert_eq!(after["teams"], serde_json::json!([]));

    // An invite link for some other service is refused before any request.
    let foreign = link.replace(&hub_url, "https://elsewhere.example");
    let err = teams::preview_invite(&foreign).await.unwrap_err();
    assert_eq!(err.code, "bad_request", "{}", err.message);
}
