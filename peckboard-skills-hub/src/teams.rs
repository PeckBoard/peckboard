//! Teams of PeckBoard servers (see `API.md`, "Teams"). Served on
//! `team.peckboard.com`; same program, same signed-header authentication
//! ([`crate::api::verify_signed`]) as the skills endpoints.
//!
//! The hub is storage and a gatekeeper, not the authority: a team is its
//! signed membership log (`peckboard-team-format`), which every member
//! server replays and checks for itself. The hub refuses a record that
//! breaks the rules, but it cannot add a member — it has no key any member
//! would accept. Reads are members-only.
//!
//! Joining: an owner or admin registers an invite (the hub stores only a
//! hash of the invite secret), the invited server files a join request with
//! a proof only the inviter can check, and the inviter appends the `add`
//! record once the two people have compared the verification code.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use peckboard_team_format::{Action, LogError, Record, Role, Team, clean_name};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::api::{ApiError, ApiResult, read_body, verify_signed};
use crate::{Hub, auth, now_rfc3339};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS teams (
    id TEXT PRIMARY KEY,
    founder_key TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS teams_founder ON teams(founder_key);
CREATE TABLE IF NOT EXISTS team_log (
    team_id TEXT NOT NULL REFERENCES teams(id),
    seq INTEGER NOT NULL,
    prev TEXT NOT NULL,
    at TEXT NOT NULL,
    body TEXT NOT NULL,
    signer TEXT NOT NULL,
    signature TEXT NOT NULL,
    PRIMARY KEY (team_id, seq)
);
CREATE TABLE IF NOT EXISTS team_members (
    team_id TEXT NOT NULL REFERENCES teams(id),
    public_key TEXT NOT NULL,
    PRIMARY KEY (team_id, public_key)
);
CREATE INDEX IF NOT EXISTS team_members_key ON team_members(public_key);
CREATE TABLE IF NOT EXISTS team_invites (
    team_id TEXT NOT NULL REFERENCES teams(id),
    id TEXT NOT NULL,
    gate TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used_by TEXT,
    PRIMARY KEY (team_id, id)
);
CREATE TABLE IF NOT EXISTS team_join_requests (
    id TEXT PRIMARY KEY,
    team_id TEXT NOT NULL REFERENCES teams(id),
    invite_id TEXT NOT NULL,
    public_key TEXT NOT NULL,
    label TEXT NOT NULL,
    proof TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS team_join_requests_team ON team_join_requests(team_id, state);
"#;

const MAX_SIGNED_BODY: usize = 16 * 1024;
/// Teams one key may found.
const MAX_TEAMS_PER_FOUNDER: i64 = 20;
/// Invites a team may have open at once.
const MAX_OPEN_INVITES: i64 = 20;
/// Join requests one invite accepts before it is approved. More than one,
/// so a stranger who got hold of the link cannot lock the invited coworker
/// out by asking first.
const MAX_REQUESTS_PER_INVITE: i64 = 5;
/// Longest an invite may stay valid.
const MAX_INVITE_DAYS: i64 = 7;
/// Header a not-yet-member presents to preview a team: `<invite_id>.<gate>`.
const H_INVITE: &str = "x-peck-invite";

pub fn routes(r: Router<Arc<Hub>>) -> Router<Arc<Hub>> {
    r.route("/api/teams", post(create).get(mine))
        .route("/api/teams/{id}/log", get(read_log).post(append))
        .route("/api/teams/{id}/preview", get(preview))
        .route(
            "/api/teams/{id}/invites",
            post(add_invite).get(list_invites),
        )
        .route("/api/teams/{id}/invites/{invite}", delete(revoke_invite))
        .route("/api/teams/{id}/join", post(join))
        .route("/api/teams/{id}/requests", get(list_requests))
        .route(
            "/api/teams/{id}/requests/{request}",
            get(request_state).delete(withdraw_request),
        )
        .route("/api/teams/{id}/requests/{request}/reject", post(reject))
}

// ── helpers ─────────────────────────────────────────────────────────────

/// The hub's Ed25519 check for a log record. Stricter than the request
/// signature check on purpose: the key and the signature must be in their
/// one canonical text form (standard padded base64). Every member server
/// re-checks every record, so a record the hub accepted but a member
/// refuses would break the team for that member; and one server must not
/// appear under two spellings of the same key.
fn verify(key: &str, msg: &str, signature: &str) -> bool {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    let canonical_signature = STANDARD
        .decode(signature)
        .is_ok_and(|bytes| bytes.len() == 64 && STANDARD.encode(&bytes) == signature);
    canonical_signature
        && auth::parse_public_key(key)
            .is_some_and(|(vk, canonical)| canonical == key && auth::verify(&vk, msg, signature))
}

fn is_canonical_key(key: &str) -> bool {
    auth::parse_public_key(key).is_some_and(|(_, canonical)| canonical == key)
}

fn not_member() -> ApiError {
    // Same answer whether the team exists or not: a non-member learns
    // nothing about which team ids are real.
    ApiError::new(
        StatusCode::FORBIDDEN,
        "not_member",
        "This server is not a member of that team.",
    )
}

fn invite_invalid() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "invite_invalid",
        "This invite is not valid. It may have expired, been used, or been withdrawn. Ask for a new one.",
    )
}

fn bad_record(e: LogError) -> ApiError {
    match e {
        LogError::OutOfOrder => ApiError::conflict("log_conflict", e.to_string()),
        LogError::NotAllowed(_) => {
            ApiError::new(StatusCode::FORBIDDEN, "not_allowed", e.to_string())
        }
        _ => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_record",
            e.to_string(),
        ),
    }
}

fn valid_team_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_token(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn random_id() -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8], shape: &str) -> ApiResult<T> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("Expected JSON {shape}: {e}")))
}

struct Signed {
    key: String,
    body: axum::body::Bytes,
}

async fn signed(
    hub: &Hub,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Body,
) -> ApiResult<Signed> {
    let body = read_body(body, MAX_SIGNED_BODY).await?;
    let key = verify_signed(hub, method, uri, headers, &body)?;
    Ok(Signed { key, body })
}

fn load_log(conn: &Connection, team_id: &str) -> anyhow::Result<Vec<Record>> {
    let mut stmt = conn.prepare(
        "SELECT seq, prev, at, body, signer, signature FROM team_log
         WHERE team_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map([team_id], |r| {
        Ok(Record {
            seq: r.get::<_, i64>(0)? as u64,
            prev: r.get(1)?,
            at: r.get(2)?,
            body: r.get(3)?,
            signer: r.get(4)?,
            signature: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The team as its stored log says it is; `None` when there is no such team.
fn load_team(conn: &Connection, team_id: &str) -> anyhow::Result<Option<(Team, Vec<Record>)>> {
    if !valid_team_id(team_id) {
        return Ok(None);
    }
    let log = load_log(conn, team_id)?;
    if log.is_empty() {
        return Ok(None);
    }
    let team = peckboard_team_format::replay(&log, &verify)
        .map_err(|e| anyhow::anyhow!("stored log of team {team_id} does not replay: {e}"))?;
    Ok(Some((team, log)))
}

/// The team, for a caller that must be a member of it.
fn team_for_member(conn: &Connection, team_id: &str, key: &str) -> ApiResult<(Team, Vec<Record>)> {
    let (team, log) = load_team(conn, team_id)?.ok_or_else(not_member)?;
    if team.role_of(key).is_none() {
        return Err(not_member());
    }
    Ok((team, log))
}

/// The team, for a caller that must be one of its owners or admins.
fn team_for_manager(conn: &Connection, team_id: &str, key: &str) -> ApiResult<Team> {
    let (team, _) = team_for_member(conn, team_id, key)?;
    if team.role_of(key) == Some(Role::Member) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "not_allowed",
            "Only an owner or admin of the team can do this.",
        ));
    }
    Ok(team)
}

fn store_record(conn: &Connection, team_id: &str, r: &Record) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO team_log (team_id, seq, prev, at, body, signer, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            team_id,
            r.seq as i64,
            r.prev,
            r.at,
            r.body,
            r.signer,
            r.signature
        ],
    )
}

/// Bring the members lookup table in line with the replayed team.
fn sync_members(conn: &Connection, team: &Team) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM team_members WHERE team_id = ?1", [&team.id])?;
    for key in team.members.keys() {
        conn.execute(
            "INSERT INTO team_members (team_id, public_key) VALUES (?1, ?2)",
            params![team.id, key],
        )?;
    }
    Ok(())
}

fn member_views(team: &Team) -> Vec<serde_json::Value> {
    team.members
        .iter()
        .map(|(key, m)| {
            serde_json::json!({
                "key": key, "role": m.role.as_str(), "label": m.label, "added_at": m.added_at,
            })
        })
        .collect()
}

// ── create / list / log ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct RecordReq {
    record: Record,
    /// The join request this `add` record approves, if any.
    #[serde(default)]
    request_id: Option<String>,
}

async fn create(
    State(hub): State<Arc<Hub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: RecordReq = parse(&s.body, "{record}")?;
    if req.record.signer != s.key {
        return Err(ApiError::bad_request(
            "A team must be created by the server that signs the request.",
        ));
    }
    let team = Team::apply(None, &req.record, &verify).map_err(bad_record)?;
    let mut store = hub.store();
    let founded: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM teams WHERE founder_key = ?1",
        [&s.key],
        |r| r.get(0),
    )?;
    if founded >= MAX_TEAMS_PER_FOUNDER {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("One server can create at most {MAX_TEAMS_PER_FOUNDER} teams."),
        ));
    }
    let tx = store.conn_mut().transaction()?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO teams (id, founder_key, created_at) VALUES (?1, ?2, ?3)",
        params![team.id, s.key, now_rfc3339()],
    )?;
    if inserted == 0 {
        return Err(ApiError::conflict(
            "team_exists",
            "This team already exists.",
        ));
    }
    store_record(&tx, &team.id, &req.record)?;
    sync_members(&tx, &team)?;
    tx.commit()?;
    store.audit("team-create", Some(&s.key), &team.id);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "team_id": team.id, "name": team.name })),
    )
        .into_response())
}

/// `GET /api/teams` (signed): the teams the calling server is a member of.
async fn mine(
    State(hub): State<Arc<Hub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let key = verify_signed(&hub, &method, &uri, &headers, b"")?;
    let store = hub.store();
    let ids: Vec<String> = {
        let mut stmt = store
            .conn()
            .prepare("SELECT team_id FROM team_members WHERE public_key = ?1 ORDER BY team_id")?;
        stmt.query_map([&key], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    let mut teams = Vec::new();
    for id in ids {
        if let Some((team, _)) = load_team(store.conn(), &id)?
            && let Some(role) = team.role_of(&key)
        {
            teams.push(serde_json::json!({
                "team_id": team.id, "name": team.name, "role": role.as_str(),
                "members": team.members.len(),
            }));
        }
    }
    Ok(Json(serde_json::json!({ "teams": teams })).into_response())
}

async fn read_log(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let key = verify_signed(&hub, &method, &uri, &headers, b"")?;
    let store = hub.store();
    let (team, log) = team_for_member(store.conn(), &id, &key)?;
    Ok(Json(serde_json::json!({ "team_id": team.id, "records": log })).into_response())
}

async fn append(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: RecordReq = parse(&s.body, "{record, request_id?}")?;
    let mut store = hub.store();
    let (team, _) = team_for_member(store.conn(), &id, &s.key)?;
    if req.record.signer != s.key {
        return Err(ApiError::bad_request(
            "A team change must be signed by the server that sends it.",
        ));
    }
    let action = req.record.action().map_err(bad_record)?;
    if let Action::Add { key, .. } = &action
        && !is_canonical_key(key)
    {
        return Err(ApiError::bad_request(
            "The server to add must be named by its base64 Ed25519 public key.",
        ));
    }
    let next = Team::apply(Some(&team), &req.record, &verify).map_err(bad_record)?;

    // An `add` that approves a join request settles that request and uses
    // up its invite.
    let approved = match (&req.request_id, &action) {
        (Some(rid), Action::Add { key, .. }) => {
            let row: Option<(String, String, String)> = store
                .conn()
                .query_row(
                    "SELECT public_key, invite_id, state FROM team_join_requests
                     WHERE id = ?1 AND team_id = ?2",
                    params![rid, id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match row {
                Some((req_key, invite_id, state)) if req_key == *key && state == "pending" => {
                    Some((rid.clone(), invite_id, req_key))
                }
                _ => {
                    return Err(ApiError::bad_request(
                        "That join request is not open for this server.",
                    ));
                }
            }
        }
        _ => None,
    };

    let tx = store.conn_mut().transaction()?;
    match store_record(&tx, &id, &req.record) {
        Ok(_) => {}
        // Someone else's record took this seq between the replay and now.
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            return Err(bad_record(LogError::OutOfOrder));
        }
        Err(e) => return Err(anyhow::Error::from(e).into()),
    }
    sync_members(&tx, &next)?;
    if let Some((rid, invite_id, key)) = &approved {
        tx.execute(
            "UPDATE team_join_requests SET state = 'approved' WHERE id = ?1",
            [rid],
        )?;
        tx.execute(
            "UPDATE team_invites SET used_by = ?3 WHERE team_id = ?1 AND id = ?2",
            params![id, invite_id, key],
        )?;
        // The other requests filed with the same invite are now moot.
        tx.execute(
            "UPDATE team_join_requests SET state = 'rejected'
             WHERE team_id = ?1 AND invite_id = ?2 AND state = 'pending'",
            params![id, invite_id],
        )?;
    }
    // A removed server's own open invites and the requests against them die
    // with its membership.
    if let Action::Remove { key } = &action {
        tx.execute(
            "DELETE FROM team_invites WHERE team_id = ?1 AND created_by = ?2 AND used_by IS NULL",
            params![id, key],
        )?;
    }
    tx.commit()?;
    store.audit(
        "team-change",
        Some(&s.key),
        &format!("{id} seq {}", req.record.seq),
    );
    Ok(Json(serde_json::json!({
        "team_id": next.id, "len": next.len, "head": next.head,
    }))
    .into_response())
}

// ── invites ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct InviteReq {
    invite_id: String,
    /// `peckboard_team_format::gate(secret)`.
    gate: String,
    /// RFC 3339; at most [`MAX_INVITE_DAYS`] from now.
    expires_at: String,
}

fn parse_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

async fn add_invite(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: InviteReq = parse(&s.body, "{invite_id, gate, expires_at}")?;
    let store = hub.store();
    team_for_manager(store.conn(), &id, &s.key)?;
    if !is_token(&req.invite_id, 8, 64) || !is_hex(&req.gate, 64) {
        return Err(ApiError::bad_request("The invite is malformed."));
    }
    let now = chrono::Utc::now();
    let expires = parse_time(&req.expires_at)
        .filter(|t| *t > now && *t <= now + chrono::Duration::days(MAX_INVITE_DAYS))
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "An invite must expire within {MAX_INVITE_DAYS} days."
            ))
        })?;
    let expires = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let now = now_rfc3339();
    store.conn().execute(
        "DELETE FROM team_invites WHERE team_id = ?1 AND used_by IS NULL AND expires_at <= ?2",
        params![id, now],
    )?;
    let open: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM team_invites WHERE team_id = ?1 AND used_by IS NULL",
        [&id],
        |r| r.get(0),
    )?;
    if open >= MAX_OPEN_INVITES {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("A team can have at most {MAX_OPEN_INVITES} open invites. Withdraw one first."),
        ));
    }
    let inserted = store.conn().execute(
        "INSERT OR IGNORE INTO team_invites (team_id, id, gate, created_by, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, req.invite_id, req.gate, s.key, now, expires],
    )?;
    if inserted == 0 {
        return Err(ApiError::conflict(
            "invite_exists",
            "That invite already exists.",
        ));
    }
    store.audit("team-invite", Some(&s.key), &id);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "invite_id": req.invite_id, "expires_at": expires })),
    )
        .into_response())
}

async fn list_invites(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let key = verify_signed(&hub, &method, &uri, &headers, b"")?;
    let store = hub.store();
    team_for_manager(store.conn(), &id, &key)?;
    let mut stmt = store.conn().prepare(
        "SELECT id, created_by, created_at, expires_at FROM team_invites
         WHERE team_id = ?1 AND used_by IS NULL AND expires_at > ?2 ORDER BY created_at",
    )?;
    let invites: Vec<serde_json::Value> = stmt
        .query_map(params![id, now_rfc3339()], |r| {
            Ok(serde_json::json!({
                "invite_id": r.get::<_, String>(0)?,
                "created_by": r.get::<_, String>(1)?,
                "created_at": r.get::<_, String>(2)?,
                "expires_at": r.get::<_, String>(3)?,
            }))
        })?
        .collect::<Result<_, _>>()?;
    Ok(Json(serde_json::json!({ "invites": invites })).into_response())
}

async fn revoke_invite(
    State(hub): State<Arc<Hub>>,
    Path((id, invite)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let key = verify_signed(&hub, &method, &uri, &headers, b"")?;
    let mut store = hub.store();
    team_for_manager(store.conn(), &id, &key)?;
    let tx = store.conn_mut().transaction()?;
    let gone = tx.execute(
        "DELETE FROM team_invites WHERE team_id = ?1 AND id = ?2 AND used_by IS NULL",
        params![id, invite],
    )?;
    tx.execute(
        "UPDATE team_join_requests SET state = 'rejected'
         WHERE team_id = ?1 AND invite_id = ?2 AND state = 'pending'",
        params![id, invite],
    )?;
    tx.commit()?;
    if gone == 0 {
        return Err(ApiError::not_found("Invite"));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The open invite `invite_id` of `team_id` whose gate matches, if any.
fn open_invite(conn: &Connection, team_id: &str, invite_id: &str, gate: &str) -> ApiResult<()> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT gate FROM team_invites
             WHERE team_id = ?1 AND id = ?2 AND used_by IS NULL AND expires_at > ?3",
            params![team_id, invite_id, now_rfc3339()],
            |r| r.get(0),
        )
        .optional()?;
    match stored {
        // Both sides are hashes of the secret, not the secret: a plain
        // comparison leaks nothing an attacker could use.
        Some(g) if g == gate => Ok(()),
        _ => Err(invite_invalid()),
    }
}

/// `GET /api/teams/<id>/preview` with `X-Peck-Invite: <invite_id>.<gate>`:
/// what an invited server shows before it asks to join. Unsigned — holding
/// the invite is the permission.
async fn preview(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (invite_id, gate) = headers
        .get(H_INVITE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once('.'))
        .ok_or_else(invite_invalid)?;
    let store = hub.store();
    let (team, _) = load_team(store.conn(), &id)?.ok_or_else(invite_invalid)?;
    open_invite(store.conn(), &id, invite_id, gate)?;
    Ok(Json(serde_json::json!({
        "team_id": team.id, "name": team.name, "members": team.members.len(),
    }))
    .into_response())
}

// ── join requests ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct JoinReq {
    invite_id: String,
    gate: String,
    /// `peckboard_team_format::join_proof(secret, team_id, key)` — opaque
    /// to the hub, checked by the approving server.
    proof: String,
    /// The joining server's display name.
    label: String,
}

async fn join(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: JoinReq = parse(&s.body, "{invite_id, gate, proof, label}")?;
    let store = hub.store();
    let (team, _) = load_team(store.conn(), &id)?.ok_or_else(invite_invalid)?;
    open_invite(store.conn(), &id, &req.invite_id, &req.gate)?;
    if team.role_of(&s.key).is_some() {
        return Err(ApiError::conflict(
            "already_member",
            "This server is already in that team.",
        ));
    }
    if !is_hex(&req.proof, 64) {
        return Err(ApiError::bad_request("The join request is malformed."));
    }
    let label = clean_name(&req.label, "The server name")
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    // Asking again with the same invite replaces the earlier request.
    if let Some(existing) = store
        .conn()
        .query_row(
            "SELECT id FROM team_join_requests
             WHERE team_id = ?1 AND invite_id = ?2 AND public_key = ?3 AND state = 'pending'",
            params![id, req.invite_id, s.key],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        store.conn().execute(
            "UPDATE team_join_requests SET label = ?2, proof = ?3 WHERE id = ?1",
            params![existing, label, req.proof],
        )?;
        return Ok(Json(serde_json::json!({
            "request_id": existing, "team_id": team.id, "name": team.name,
        }))
        .into_response());
    }
    let pending: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM team_join_requests
         WHERE team_id = ?1 AND invite_id = ?2 AND state = 'pending'",
        params![id, req.invite_id],
        |r| r.get(0),
    )?;
    if pending >= MAX_REQUESTS_PER_INVITE {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many servers have asked to join with this invite. Ask for a new one.",
        ));
    }
    let request_id = random_id();
    store.conn().execute(
        "INSERT INTO team_join_requests (id, team_id, invite_id, public_key, label, proof, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![request_id, id, req.invite_id, s.key, label, req.proof, now_rfc3339()],
    )?;
    store.audit("team-join-request", Some(&s.key), &id);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "request_id": request_id, "team_id": team.id, "name": team.name,
        })),
    )
        .into_response())
}

async fn list_requests(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let key = verify_signed(&hub, &method, &uri, &headers, b"")?;
    let store = hub.store();
    team_for_manager(store.conn(), &id, &key)?;
    let mut stmt = store.conn().prepare(
        "SELECT r.id, r.invite_id, r.public_key, r.label, r.proof, r.created_at, i.created_by
         FROM team_join_requests r
         JOIN team_invites i ON i.team_id = r.team_id AND i.id = r.invite_id
         WHERE r.team_id = ?1 AND r.state = 'pending' AND i.used_by IS NULL AND i.expires_at > ?2
         ORDER BY r.created_at",
    )?;
    let requests: Vec<serde_json::Value> = stmt
        .query_map(params![id, now_rfc3339()], |r| {
            Ok(serde_json::json!({
                "request_id": r.get::<_, String>(0)?,
                "invite_id": r.get::<_, String>(1)?,
                "key": r.get::<_, String>(2)?,
                "label": r.get::<_, String>(3)?,
                "proof": r.get::<_, String>(4)?,
                "created_at": r.get::<_, String>(5)?,
                "invited_by": r.get::<_, String>(6)?,
            }))
        })?
        .collect::<Result<_, _>>()?;
    Ok(Json(serde_json::json!({ "requests": requests })).into_response())
}

/// `GET /api/teams/<id>/requests/<request>` (signed by the server that
/// filed it): how its join request stands.
async fn request_state(
    State(hub): State<Arc<Hub>>,
    Path((id, request)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let key = verify_signed(&hub, &method, &uri, &headers, b"")?;
    let store = hub.store();
    let state: Option<String> = store
        .conn()
        .query_row(
            "SELECT state FROM team_join_requests WHERE id = ?1 AND team_id = ?2 AND public_key = ?3",
            params![request, id, key],
            |r| r.get(0),
        )
        .optional()?;
    let state = state.ok_or_else(|| ApiError::not_found("Join request"))?;
    Ok(Json(serde_json::json!({ "request_id": request, "state": state })).into_response())
}

/// `DELETE /api/teams/<id>/requests/<request>` (signed by the server that
/// filed it): take its own open join request back.
async fn withdraw_request(
    State(hub): State<Arc<Hub>>,
    Path((id, request)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let key = verify_signed(&hub, &method, &uri, &headers, b"")?;
    let store = hub.store();
    let changed = store.conn().execute(
        "UPDATE team_join_requests SET state = 'rejected'
         WHERE id = ?1 AND team_id = ?2 AND public_key = ?3 AND state = 'pending'",
        params![request, id, key],
    )?;
    if changed == 0 {
        return Err(ApiError::not_found("Join request"));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn reject(
    State(hub): State<Arc<Hub>>,
    Path((id, request)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let store = hub.store();
    team_for_manager(store.conn(), &id, &s.key)?;
    let changed = store.conn().execute(
        "UPDATE team_join_requests SET state = 'rejected'
         WHERE id = ?1 AND team_id = ?2 AND state = 'pending'",
        params![request, id],
    )?;
    if changed == 0 {
        return Err(ApiError::not_found("Join request"));
    }
    store.audit("team-join-reject", Some(&s.key), &id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// For the member list a client shows: the team's members as the log has
/// them. Exposed for tests and the admin CLI.
pub fn members_of(conn: &Connection, team_id: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    Ok(load_team(conn, team_id)?
        .map(|(team, _)| member_views(&team))
        .unwrap_or_default())
}
