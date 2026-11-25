//! Teams of PeckBoard servers, with no accounts (`team.peckboard.com`).
//!
//! A member is a server, named by its permanent identity key (the one that
//! signs skills-hub requests). A team is a signed, append-only membership
//! log (`peckboard-team-format`). The team service stores the log and
//! relays join requests, but this server never takes its word for who is in
//! a team: every fetched log is replayed and every signature checked here,
//! and the log must extend the newest state this server has already
//! verified (`teams.log_len` / `log_head`), so a removal cannot be hidden
//! by serving older history.
//!
//! Joining: an owner or admin creates an invite link (the secret in it
//! never reaches the team service). The invited server files a join request
//! with a proof only the inviting server can check, both people compare the
//! six-digit verification code, and the inviting server appends the signed
//! `add` record.
//!
//! Every function here is admin-only at the route layer (`routes::teams`).

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use peckboard_team_format::{
    Action, Invite, LogError, Record, Role, Team, clean_name, gate, join_proof, log_message,
    replay, verification_code,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::db::Db;
use crate::db::models::{TeamInviteRow, TeamRow};
use crate::service::remote_access::tunnel::BoxIdentity;
use crate::service::skills::hub as skills_hub;

/// The hosted team service.
pub const DEFAULT_HUB_URL: &str = "https://team.peckboard.com";
/// What the UI calls it.
const HUB_LABEL: &str = "the team service";
const TIMEOUT: Duration = Duration::from_secs(30);
/// The most proof of work this server will do for one request.
const MAX_POW_BITS: u32 = 28;
/// How long an invite stays valid unless the caller says otherwise, and
/// the longest the team service accepts.
const DEFAULT_INVITE_DAYS: i64 = 3;
const MAX_INVITE_DAYS: i64 = 7;

/// Base URL of the team service. `PECKBOARD_TEAM_HUB_URL` overrides it
/// (tests, a self-hosted hub). Invite links for any other service are
/// refused: pasting a link must not make this server talk to an arbitrary
/// address.
pub fn hub_url() -> String {
    std::env::var("PECKBOARD_TEAM_HUB_URL")
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .unwrap_or_else(|| DEFAULT_HUB_URL.to_string())
}

/// A failed team operation, in plain words for the Team settings page.
#[derive(Debug, Clone)]
pub struct TeamError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

pub type TeamResult<T> = Result<T, TeamError>;

impl TeamError {
    fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_string(),
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "bad_request", message)
    }

    fn not_found(what: &str) -> Self {
        Self::new(404, "not_found", format!("{what} not found."))
    }

    fn internal(e: impl std::fmt::Display) -> Self {
        tracing::warn!("teams: {e:#}");
        Self::new(
            500,
            "internal",
            "Something went wrong on this server. Check its log.",
        )
    }

    pub fn body(&self) -> Value {
        json!({ "error": self.message, "code": self.code })
    }
}

impl From<anyhow::Error> for TeamError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(format!("{e:#}"))
    }
}

impl From<LogError> for TeamError {
    fn from(e: LogError) -> Self {
        let (status, code) = match &e {
            LogError::NotAllowed(_) => (403, "not_allowed"),
            LogError::OutOfOrder => (409, "log_conflict"),
            _ => (422, "bad_record"),
        };
        Self::new(status, code, e.to_string())
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A server's key in the one text form team records use: standard padded
/// base64 of the 32 bytes.
fn key_text(identity: &BoxIdentity) -> String {
    STANDARD.encode(identity.public_key())
}

fn key_bytes(key: &str) -> Option<[u8; 32]> {
    let bytes: [u8; 32] = STANDARD.decode(key).ok()?.try_into().ok()?;
    (STANDARD.encode(bytes) == key).then_some(bytes)
}

/// Short, readable form of a server key for the member list.
fn fingerprint(key: &str) -> String {
    key_bytes(key)
        .map(|b| peckboard_relay::identity::fingerprint(&b))
        .unwrap_or_default()
}

/// This server's Ed25519 check for a log record: canonical key, canonical
/// signature, strict verification — the same rule the team service applies.
fn verify(key: &str, msg: &str, signature: &str) -> bool {
    let Some(key) = key_bytes(key) else {
        return false;
    };
    let Ok(sig) = STANDARD.decode(signature) else {
        return false;
    };
    let Ok(sig): Result<[u8; 64], _> = sig.try_into() else {
        return false;
    };
    STANDARD.encode(sig) == signature
        && peckboard_relay::identity::verify_team_record(&key, msg, &sig)
}

async fn identity(db: &Db, data_dir: &Path) -> TeamResult<BoxIdentity> {
    skills_hub::identity(db, data_dir).await.map_err(|e| {
        TeamError::new(
            500,
            "identity_missing",
            format!(
                "This server's identity key could not be loaded, so it cannot act in a team. {e}"
            ),
        )
    })
}

/// A name for this server in member lists when the user gives none.
pub fn default_label() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_default();
    clean_name(&host, "name").unwrap_or_else(|_| "PeckBoard".to_string())
}

// ── The team service client ─────────────────────────────────────────────

struct Hub {
    base: String,
    client: reqwest::Client,
}

fn unreachable(e: reqwest::Error) -> TeamError {
    TeamError::new(
        502,
        "unreachable",
        format!("Couldn't reach {HUB_LABEL} ({}): {e}", hub_url()),
    )
}

/// A team-service error response, in plain words. The service already
/// writes `error` for a person; this adds the cases where its text would
/// not help.
fn hub_error(status: u16, text: &str) -> TeamError {
    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let code = body["code"].as_str().unwrap_or_default().to_string();
    let detail = body["error"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {status}"));
    match code.as_str() {
        "nonce_invalid" | "pow_required" => TeamError::new(
            502,
            &code,
            "The request took too long and its one-time code expired. Try again.",
        ),
        "bad_signature" => TeamError::new(
            502,
            &code,
            format!(
                "{HUB_LABEL} couldn't verify this server's signature. Try again; if it keeps \
                 happening, this server's identity key may be damaged."
            ),
        ),
        "banned" => TeamError::new(
            403,
            &code,
            format!("{HUB_LABEL} no longer accepts requests from this server."),
        ),
        "" => TeamError::new(
            502,
            "upstream",
            format!("{HUB_LABEL} answered with an error ({detail})."),
        ),
        _ => TeamError::new(if status >= 500 { 502 } else { status }, &code, detail),
    }
}

impl Hub {
    fn new() -> TeamResult<Self> {
        let client = crate::http_client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(TeamError::internal)?;
        Ok(Self {
            base: hub_url(),
            client,
        })
    }

    async fn read(resp: reqwest::Response) -> TeamResult<Value> {
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(unreachable)?;
        if !(200..300).contains(&status) {
            return Err(hub_error(status, &text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| {
            TeamError::new(
                502,
                "upstream",
                format!("{HUB_LABEL} sent an unreadable answer: {e}"),
            )
        })
    }

    /// One signed request: challenge, proof of work when asked, `X-Peck-*`
    /// headers over the method, the exact path and the body's digest.
    async fn signed(
        &self,
        identity: &BoxIdentity,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> TeamResult<Value> {
        let public_key = key_text(identity);
        let resp = self
            .client
            .get(format!(
                "{}/api/challenge?public_key={}",
                self.base,
                urlencoding::encode(&public_key)
            ))
            .send()
            .await
            .map_err(unreachable)?;
        let challenge = Self::read(resp).await?;
        let nonce = challenge["nonce"]
            .as_str()
            .ok_or_else(|| {
                TeamError::new(
                    502,
                    "upstream",
                    format!("{HUB_LABEL} sent an unreadable challenge."),
                )
            })?
            .to_string();
        let bits = challenge["pow_difficulty"].as_u64().unwrap_or(0) as u32;
        if bits > MAX_POW_BITS {
            return Err(TeamError::new(
                502,
                "upstream",
                format!(
                    "{HUB_LABEL} asked for more proof of work than this server will do. Try again later."
                ),
            ));
        }
        let pow = if bits > 0 {
            let n = nonce.clone();
            Some(
                tokio::task::spawn_blocking(move || skills_hub::solve_pow(&n, bits))
                    .await
                    .map_err(TeamError::internal)?,
            )
        } else {
            None
        };
        let raw = match body {
            Some(b) => serde_json::to_vec(b).map_err(TeamError::internal)?,
            None => Vec::new(),
        };
        let headers = skills_hub::signed_headers(
            identity,
            method.as_str(),
            path,
            &nonce,
            pow.as_deref(),
            &raw,
        );
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if body.is_some() {
            req = req.header("Content-Type", "application/json").body(raw);
        }
        Self::read(req.send().await.map_err(unreachable)?).await
    }

    async fn log(&self, identity: &BoxIdentity, team_id: &str) -> TeamResult<Vec<Record>> {
        let v = self
            .signed(
                identity,
                reqwest::Method::GET,
                &format!("/api/teams/{team_id}/log"),
                None,
            )
            .await?;
        serde_json::from_value(v["records"].clone()).map_err(|e| {
            TeamError::new(
                502,
                "upstream",
                format!("{HUB_LABEL} sent an unreadable team history: {e}"),
            )
        })
    }
}

// ── Local state ─────────────────────────────────────────────────────────

/// A team id as it appears in a URL path and in storage: 32 hex chars.
fn check_team_id(id: &str) -> TeamResult<()> {
    if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(TeamError::not_found("Team"))
    }
}

async fn team_row(db: &Db, id: &str) -> TeamResult<TeamRow> {
    check_team_id(id)?;
    db.get_team(id)
        .await?
        .ok_or_else(|| TeamError::not_found("Team"))
}

/// The team as this server's verified copy of its log says it is.
async fn local_team(db: &Db, id: &str) -> TeamResult<Option<Team>> {
    let stored = db.team_log_records(id).await?;
    if stored.is_empty() {
        return Ok(None);
    }
    let records: Vec<Record> = stored
        .iter()
        .map(|s| serde_json::from_str(s))
        .collect::<Result<_, _>>()
        .map_err(TeamError::internal)?;
    Ok(Some(replay(&records, &verify)?))
}

fn history_mismatch() -> TeamError {
    TeamError::new(
        502,
        "history_mismatch",
        format!(
            "{HUB_LABEL} returned a team history that does not continue the one this server \
             already verified. Nothing was changed. If this keeps happening, treat the team \
             service as untrustworthy for this team."
        ),
    )
}

/// Check a fetched log and fold it into `row`: it must replay, found
/// exactly this team, and extend the state this server pinned earlier.
/// Returns the row to save and the team.
fn accept_log(mut row: TeamRow, records: &[Record], my_key: &str) -> TeamResult<(TeamRow, Team)> {
    let team = replay(records, &verify).map_err(|e| {
        TeamError::new(
            502,
            "bad_history",
            format!("{HUB_LABEL} returned a team history that does not check out: {e}"),
        )
    })?;
    if team.id != row.id {
        return Err(history_mismatch());
    }
    if row.log_len > 0 {
        let pinned = records
            .get(row.log_len as usize - 1)
            .ok_or_else(history_mismatch)?;
        if pinned.hash() != row.log_head {
            return Err(history_mismatch());
        }
    }
    let role = team.role_of(my_key);
    row.name = team.name.clone();
    row.role = role.map(|r| r.as_str().to_string());
    row.log_len = team.len as i64;
    row.log_head = team.head.clone();
    row.last_error = None;
    row.state = match role {
        Some(_) => "member".to_string(),
        None => {
            // Left on its own, or taken out by someone else?
            let left = records.iter().rev().any(|r| {
                r.signer == my_key
                    && matches!(r.action(), Ok(Action::Remove { key }) if key == my_key)
            });
            if left { "left" } else { "removed" }.to_string()
        }
    };
    if row.state != "pending" {
        row.request_id = None;
        row.invite_id = None;
        row.invite_secret = None;
    }
    row.updated_at = now();
    Ok((row, team))
}

async fn save(db: &Db, row: TeamRow, records: Option<&[Record]>) -> TeamResult<()> {
    let records = match records {
        Some(rs) => Some(
            rs.iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()
                .map_err(TeamError::internal)?,
        ),
        None => None,
    };
    Ok(db.save_team(row, records).await?)
}

/// Record a failed refresh on the row so the page can show it.
async fn note_error(db: &Db, mut row: TeamRow, e: &TeamError) {
    row.last_error = Some(e.message.clone());
    row.updated_at = now();
    if let Err(e) = db.save_team(row, None).await {
        tracing::warn!("teams: could not record a refresh error: {e:#}");
    }
}

// ── Views ───────────────────────────────────────────────────────────────

#[derive(Serialize, Debug, Clone)]
pub struct MemberView {
    pub key: String,
    pub fingerprint: String,
    pub label: String,
    pub role: String,
    pub added_at: String,
    pub is_self: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct TeamView {
    pub id: String,
    pub name: String,
    /// member | pending | left | removed
    pub state: String,
    pub role: Option<String>,
    pub members: Vec<MemberView>,
    /// While a join request is pending: the code to read out to the person
    /// who sent the invite.
    pub pending_code: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

async fn view(db: &Db, row: &TeamRow, my_key: &str) -> TeamResult<TeamView> {
    let team = local_team(db, &row.id).await?;
    let members = team
        .map(|t| {
            t.members
                .iter()
                .map(|(key, m)| MemberView {
                    key: key.clone(),
                    fingerprint: fingerprint(key),
                    label: m.label.clone(),
                    role: m.role.as_str().to_string(),
                    added_at: m.added_at.clone(),
                    is_self: key == my_key,
                })
                .collect()
        })
        .unwrap_or_default();
    let pending_code = match (row.state.as_str(), &row.invite_secret) {
        ("pending", Some(secret)) => Some(verification_code(secret, &row.id, my_key)),
        _ => None,
    };
    Ok(TeamView {
        id: row.id.clone(),
        name: row.name.clone(),
        state: row.state.clone(),
        role: row.role.clone(),
        members,
        pending_code,
        last_error: row.last_error.clone(),
        updated_at: row.updated_at.clone(),
    })
}

/// Everything the Team settings page shows, from local state only.
pub async fn overview(db: &Db, data_dir: &Path) -> TeamResult<Value> {
    let identity = identity(db, data_dir).await?;
    let my_key = key_text(&identity);
    let mut teams = Vec::new();
    for row in db.list_teams().await? {
        teams.push(view(db, &row, &my_key).await?);
    }
    Ok(json!({
        "server_key": my_key,
        "server_fingerprint": fingerprint(&my_key),
        "default_label": default_label(),
        "hub_url": hub_url(),
        "teams": teams,
    }))
}

async fn team_view(db: &Db, data_dir: &Path, id: &str) -> TeamResult<TeamView> {
    let identity = identity(db, data_dir).await?;
    let row = team_row(db, id).await?;
    view(db, &row, &key_text(&identity)).await
}

// ── Create / refresh ────────────────────────────────────────────────────

fn sign_record(
    identity: &BoxIdentity,
    tip: Option<(u64, &str)>,
    action: &Action,
) -> TeamResult<Record> {
    let body = serde_json::to_string(action).map_err(TeamError::internal)?;
    let (seq, prev) = tip.map_or((0, String::new()), |(len, head)| (len, head.to_string()));
    let at = now();
    let signature = identity
        .sign_team_record(&log_message(seq, &prev, &at, &body))
        .ok_or_else(|| TeamError::internal("team record without the team context"))?;
    Ok(Record {
        seq,
        prev,
        at,
        body,
        signer: key_text(identity),
        signature: STANDARD.encode(signature),
    })
}

/// Found a team; this server becomes its first owner.
pub async fn create(db: &Db, data_dir: &Path, name: &str, label: &str) -> TeamResult<TeamView> {
    let identity = identity(db, data_dir).await?;
    let my_key = key_text(&identity);
    let action = Action::Create {
        name: clean_name(name, "The team name")?,
        founder: my_key.clone(),
        label: clean_name(label, "The server name")?,
        salt: hex::encode(rand::random::<[u8; 16]>()),
    };
    let record = sign_record(&identity, None, &action)?;
    let team = Team::apply(None, &record, &verify)?;
    Hub::new()?
        .signed(
            &identity,
            reqwest::Method::POST,
            "/api/teams",
            Some(&json!({ "record": record })),
        )
        .await?;
    let ts = now();
    let row = TeamRow {
        id: team.id.clone(),
        name: team.name.clone(),
        state: "member".into(),
        role: Some(Role::Owner.as_str().into()),
        log_len: team.len as i64,
        log_head: team.head.clone(),
        request_id: None,
        invite_id: None,
        invite_secret: None,
        last_error: None,
        created_at: ts.clone(),
        updated_at: ts,
    };
    save(db, row.clone(), Some(std::slice::from_ref(&record))).await?;
    view(db, &row, &my_key).await
}

/// Bring one team up to date with the team service. A pending join is
/// checked for an answer; a member's log is fetched, verified and stored.
async fn refresh_row(db: &Db, identity: &BoxIdentity, row: TeamRow) -> TeamResult<TeamRow> {
    let my_key = key_text(identity);
    let hub = Hub::new()?;
    match row.state.as_str() {
        "pending" => {
            let Some(request_id) = row.request_id.clone() else {
                return Ok(row);
            };
            let path = format!("/api/teams/{}/requests/{request_id}", row.id);
            let state = match hub
                .signed(identity, reqwest::Method::GET, &path, None)
                .await
            {
                Ok(v) => v["state"].as_str().unwrap_or("pending").to_string(),
                Err(e) if e.code == "not_found" => "rejected".to_string(),
                Err(e) => return Err(e),
            };
            match state.as_str() {
                "approved" => {
                    let records = hub.log(identity, &row.id).await?;
                    let (next, _) = accept_log(row, &records, &my_key)?;
                    save(db, next.clone(), Some(&records)).await?;
                    Ok(next)
                }
                "rejected" => {
                    let mut next = row;
                    next.state = "removed".into();
                    next.request_id = None;
                    next.invite_id = None;
                    next.invite_secret = None;
                    next.last_error = Some(
                        "The request to join was declined, or the invite was withdrawn.".into(),
                    );
                    next.updated_at = now();
                    save(db, next.clone(), None).await?;
                    Ok(next)
                }
                _ => Ok(row),
            }
        }
        "member" => match hub.log(identity, &row.id).await {
            Ok(records) => {
                let (next, _) = accept_log(row, &records, &my_key)?;
                save(db, next.clone(), Some(&records)).await?;
                Ok(next)
            }
            // The service no longer lists this server: it was removed while
            // it was not looking. Its last verified copy of the log stays.
            Err(e) if e.code == "not_member" => {
                let mut next = row;
                next.state = "removed".into();
                next.role = None;
                next.last_error = None;
                next.updated_at = now();
                save(db, next.clone(), None).await?;
                Ok(next)
            }
            Err(e) => Err(e),
        },
        _ => Ok(row),
    }
}

/// Refresh one team and return it. A failure is recorded on the team (and
/// returned), never lost.
pub async fn refresh(db: &Db, data_dir: &Path, id: &str) -> TeamResult<TeamView> {
    let identity = identity(db, data_dir).await?;
    let row = team_row(db, id).await?;
    match refresh_row(db, &identity, row.clone()).await {
        Ok(next) => view(db, &next, &key_text(&identity)).await,
        Err(e) => {
            note_error(db, row, &e).await;
            Err(e)
        }
    }
}

/// Refresh every team this server is in or waiting to join. Returns how
/// many changed. One team failing does not stop the others.
pub async fn refresh_all(db: &Db, data_dir: &Path) -> TeamResult<usize> {
    let identity = identity(db, data_dir).await?;
    let mut changed = 0;
    for row in db.list_teams().await? {
        if !matches!(row.state.as_str(), "member" | "pending") {
            continue;
        }
        match refresh_row(db, &identity, row.clone()).await {
            Ok(next) => {
                if next.state != row.state || next.log_head != row.log_head {
                    changed += 1;
                }
            }
            Err(e) => {
                if row.last_error.as_deref() != Some(e.message.as_str()) {
                    changed += 1;
                }
                note_error(db, row, &e).await;
            }
        }
    }
    Ok(changed)
}

// ── Changing a team ─────────────────────────────────────────────────────

/// Append one signed change. Refreshes first so the record follows the
/// real head; if someone else's change lands in between, tries once more.
async fn append(
    db: &Db,
    data_dir: &Path,
    id: &str,
    action: Action,
    request_id: Option<&str>,
) -> TeamResult<TeamView> {
    let identity = identity(db, data_dir).await?;
    let hub = Hub::new()?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let row = refresh_row(db, &identity, team_row(db, id).await?).await?;
        if row.state != "member" {
            return Err(TeamError::new(
                403,
                "not_member",
                "This server is not a member of that team.",
            ));
        }
        let team = local_team(db, id)
            .await?
            .ok_or_else(|| TeamError::not_found("Team"))?;
        let record = sign_record(&identity, Some((team.len, &team.head)), &action)?;
        // Check the rules here first: a clear refusal without a round trip.
        Team::apply(Some(&team), &record, &verify)?;
        let sent = hub
            .signed(
                &identity,
                reqwest::Method::POST,
                &format!("/api/teams/{id}/log"),
                Some(&json!({ "record": record, "request_id": request_id })),
            )
            .await;
        match sent {
            Ok(_) => break,
            Err(e) if e.code == "log_conflict" && attempt < 2 => continue,
            Err(e) => return Err(e),
        }
    }
    let row = refresh_row(db, &identity, team_row(db, id).await?).await?;
    view(db, &row, &key_text(&identity)).await
}

fn parse_role(role: &str) -> TeamResult<Role> {
    serde_json::from_value(json!(role))
        .map_err(|_| TeamError::bad_request("The role must be owner, admin or member."))
}

pub async fn rename(db: &Db, data_dir: &Path, id: &str, name: &str) -> TeamResult<TeamView> {
    let name = clean_name(name, "The team name")?;
    append(db, data_dir, id, Action::Rename { name }, None).await
}

pub async fn set_role(
    db: &Db,
    data_dir: &Path,
    id: &str,
    key: &str,
    role: &str,
) -> TeamResult<TeamView> {
    let action = Action::SetRole {
        key: key.to_string(),
        role: parse_role(role)?,
    };
    append(db, data_dir, id, action, None).await
}

pub async fn remove_member(db: &Db, data_dir: &Path, id: &str, key: &str) -> TeamResult<TeamView> {
    append(
        db,
        data_dir,
        id,
        Action::Remove {
            key: key.to_string(),
        },
        None,
    )
    .await
}

/// This server leaves the team.
pub async fn leave(db: &Db, data_dir: &Path, id: &str) -> TeamResult<TeamView> {
    let me = key_text(&identity(db, data_dir).await?);
    append(db, data_dir, id, Action::Remove { key: me }, None).await
}

/// Drop a team this server is no longer in (or a join it is waiting on)
/// from its list. A pending join request is also taken back at the team
/// service, so it stops showing up for the people who could approve it
/// (best effort: an unreachable service does not block the removal, and the
/// request then lapses with its invite). Does not touch the team itself.
pub async fn forget(db: &Db, data_dir: &Path, id: &str) -> TeamResult<()> {
    let row = team_row(db, id).await?;
    if row.state == "member" {
        return Err(TeamError::bad_request(
            "This server is still in that team. Leave the team first.",
        ));
    }
    if row.state == "pending"
        && let Some(request_id) = &row.request_id
        && let Ok(identity) = identity(db, data_dir).await
        && let Ok(hub) = Hub::new()
    {
        let path = format!("/api/teams/{id}/requests/{request_id}");
        if let Err(e) = hub
            .signed(&identity, reqwest::Method::DELETE, &path, None)
            .await
        {
            tracing::debug!("teams: could not withdraw the join request: {}", e.message);
        }
    }
    db.delete_team(id).await?;
    Ok(())
}

// ── Invites ─────────────────────────────────────────────────────────────

async fn require_manager(db: &Db, id: &str) -> TeamResult<TeamRow> {
    let row = team_row(db, id).await?;
    if row.state != "member" || !matches!(row.role.as_deref(), Some("owner" | "admin")) {
        return Err(TeamError::new(
            403,
            "not_allowed",
            "Only an owner or admin of the team can do this.",
        ));
    }
    Ok(row)
}

fn invite_view(id: &str, row: &TeamInviteRow) -> Value {
    let link = Invite {
        hub: hub_url(),
        team_id: id.to_string(),
        invite_id: row.invite_id.clone(),
        secret: row.secret.clone(),
    }
    .link();
    json!({
        "invite_id": row.invite_id,
        "link": link,
        "expires_at": row.expires_at,
        "created_at": row.created_at,
    })
}

/// Create an invite link for one server. The secret in the link stays on
/// this server and in the link; the team service gets only a hash of it.
pub async fn create_invite(
    db: &Db,
    data_dir: &Path,
    id: &str,
    days: Option<i64>,
) -> TeamResult<Value> {
    require_manager(db, id).await?;
    let identity = identity(db, data_dir).await?;
    let days = days.unwrap_or(DEFAULT_INVITE_DAYS);
    if !(1..=MAX_INVITE_DAYS).contains(&days) {
        return Err(TeamError::bad_request(format!(
            "An invite can stay valid for 1 to {MAX_INVITE_DAYS} days."
        )));
    }
    let row = TeamInviteRow {
        invite_id: format!("inv_{}", hex::encode(rand::random::<[u8; 8]>())),
        team_id: id.to_string(),
        secret: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
        expires_at: (chrono::Utc::now() + chrono::Duration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        created_at: now(),
    };
    Hub::new()?
        .signed(
            &identity,
            reqwest::Method::POST,
            &format!("/api/teams/{id}/invites"),
            Some(&json!({
                "invite_id": row.invite_id,
                "gate": gate(&row.secret),
                "expires_at": row.expires_at,
            })),
        )
        .await?;
    db.add_team_invite(row.clone()).await?;
    Ok(invite_view(id, &row))
}

/// The open invites this server created for a team.
pub async fn list_invites(db: &Db, id: &str) -> TeamResult<Value> {
    require_manager(db, id).await?;
    let invites: Vec<Value> = db
        .list_team_invites(id, &now())
        .await?
        .iter()
        .map(|row| invite_view(id, row))
        .collect();
    Ok(json!({ "invites": invites }))
}

pub async fn revoke_invite(db: &Db, data_dir: &Path, id: &str, invite_id: &str) -> TeamResult<()> {
    require_manager(db, id).await?;
    let known = db
        .list_team_invites(id, &now())
        .await?
        .iter()
        .any(|i| i.invite_id == invite_id);
    if !known {
        return Err(TeamError::not_found("Invite"));
    }
    let identity = identity(db, data_dir).await?;
    let gone = Hub::new()?
        .signed(
            &identity,
            reqwest::Method::DELETE,
            &format!("/api/teams/{id}/invites/{invite_id}"),
            None,
        )
        .await;
    match gone {
        Ok(_) => {}
        // Already used or expired over there: still drop the local copy.
        Err(e) if e.code == "not_found" => {}
        Err(e) => return Err(e),
    }
    db.delete_team_invite(invite_id).await?;
    Ok(())
}

// ── Joining ─────────────────────────────────────────────────────────────

fn parse_link(link: &str) -> TeamResult<Invite> {
    let invite = Invite::parse(link).ok_or_else(|| {
        TeamError::bad_request(
            "That is not a team invite link. Paste the whole link you were sent.",
        )
    })?;
    if invite.hub != hub_url() {
        return Err(TeamError::bad_request(format!(
            "That invite is for a different team service ({}). This server uses {}.",
            invite.hub,
            hub_url()
        )));
    }
    Ok(invite)
}

/// What an invite link leads to, before asking to join.
pub async fn preview_invite(link: &str) -> TeamResult<Value> {
    let invite = parse_link(link)?;
    let hub = Hub::new()?;
    let resp = hub
        .client
        .get(format!("{}/api/teams/{}/preview", hub.base, invite.team_id))
        .header(
            "X-Peck-Invite",
            format!("{}.{}", invite.invite_id, gate(&invite.secret)),
        )
        .send()
        .await
        .map_err(unreachable)?;
    let v = Hub::read(resp).await?;
    Ok(json!({
        "team_id": invite.team_id,
        "name": v["name"],
        "members": v["members"],
    }))
}

/// Ask to join the team an invite link is for. Returns the team with the
/// verification code to read out to the person who sent the link.
pub async fn request_join(
    db: &Db,
    data_dir: &Path,
    link: &str,
    label: &str,
) -> TeamResult<TeamView> {
    let invite = parse_link(link)?;
    let label = clean_name(label, "The server name")?;
    let identity = identity(db, data_dir).await?;
    let my_key = key_text(&identity);
    if let Some(existing) = db.get_team(&invite.team_id).await?
        && existing.state == "member"
    {
        return Err(TeamError::new(
            409,
            "already_member",
            "This server is already in that team.",
        ));
    }
    let v = Hub::new()?
        .signed(
            &identity,
            reqwest::Method::POST,
            &format!("/api/teams/{}/join", invite.team_id),
            Some(&json!({
                "invite_id": invite.invite_id,
                "gate": gate(&invite.secret),
                "proof": join_proof(&invite.secret, &invite.team_id, &my_key),
                "label": label,
            })),
        )
        .await?;
    let ts = now();
    let row = TeamRow {
        id: invite.team_id.clone(),
        // Shown until the log is verified; the service's word, not yet checked.
        name: v["name"]
            .as_str()
            .unwrap_or("Team")
            .chars()
            .take(64)
            .collect(),
        state: "pending".into(),
        role: None,
        log_len: 0,
        log_head: String::new(),
        request_id: v["request_id"].as_str().map(str::to_string),
        invite_id: Some(invite.invite_id),
        invite_secret: Some(invite.secret),
        last_error: None,
        created_at: ts.clone(),
        updated_at: ts,
    };
    // A fresh request replaces any old row for this team (left, removed, or
    // an earlier request) together with its stored log.
    save(db, row.clone(), Some(&[])).await?;
    view(db, &row, &my_key).await
}

// ── Join requests (the inviting side) ───────────────────────────────────

/// Open join requests for a team. A request made with an invite this
/// server created comes with its verification code; one made with another
/// server's invite can only be checked and approved there.
pub async fn list_requests(db: &Db, data_dir: &Path, id: &str) -> TeamResult<Value> {
    require_manager(db, id).await?;
    let identity = identity(db, data_dir).await?;
    let v = Hub::new()?
        .signed(
            &identity,
            reqwest::Method::GET,
            &format!("/api/teams/{id}/requests"),
            None,
        )
        .await?;
    let invites = db.list_team_invites(id, &now()).await?;
    let requests: Vec<Value> = v["requests"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let key = r["key"].as_str().unwrap_or_default();
            let secret = invites
                .iter()
                .find(|i| Some(i.invite_id.as_str()) == r["invite_id"].as_str())
                .map(|i| i.secret.as_str());
            let genuine = secret
                .is_some_and(|s| Some(join_proof(s, id, key).as_str()) == r["proof"].as_str());
            json!({
                "request_id": r["request_id"],
                "invite_id": r["invite_id"],
                "label": r["label"],
                "key": key,
                "fingerprint": fingerprint(key),
                "created_at": r["created_at"],
                // Whether this server holds the invite, and so can vouch.
                "can_approve": secret.is_some(),
                // The request really came from someone holding the link.
                "genuine": genuine,
                "code": secret.filter(|_| genuine).map(|s| verification_code(s, id, key)),
            })
        })
        .collect();
    Ok(json!({ "requests": requests }))
}

/// Approve a join request: append the signed `add` record. Refused unless
/// this server created the invite and the request's proof checks out — the
/// person approving must still compare the verification code first.
pub async fn approve_request(
    db: &Db,
    data_dir: &Path,
    id: &str,
    request_id: &str,
) -> TeamResult<TeamView> {
    let listed = list_requests(db, data_dir, id).await?;
    let request = listed["requests"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["request_id"] == request_id))
        .cloned()
        .ok_or_else(|| TeamError::not_found("Join request"))?;
    if request["can_approve"] != true {
        return Err(TeamError::new(
            403,
            "not_allowed",
            "This request used an invite created on another server. Approve it there.",
        ));
    }
    if request["genuine"] != true {
        return Err(TeamError::new(
            403,
            "not_allowed",
            "This request did not come from someone holding the invite link. It was not approved.",
        ));
    }
    let action = Action::Add {
        key: request["key"].as_str().unwrap_or_default().to_string(),
        role: Role::Member,
        label: request["label"].as_str().unwrap_or_default().to_string(),
    };
    let view = append(db, data_dir, id, action, Some(request_id)).await?;
    // The invite is used up.
    if let Some(invite_id) = request["invite_id"].as_str() {
        db.delete_team_invite(invite_id).await?;
    }
    Ok(view)
}

pub async fn reject_request(
    db: &Db,
    data_dir: &Path,
    id: &str,
    request_id: &str,
) -> TeamResult<()> {
    require_manager(db, id).await?;
    if !request_id.bytes().all(|b| b.is_ascii_alphanumeric()) || request_id.is_empty() {
        return Err(TeamError::not_found("Join request"));
    }
    let identity = identity(db, data_dir).await?;
    Hub::new()?
        .signed(
            &identity,
            reqwest::Method::POST,
            &format!("/api/teams/{id}/requests/{request_id}/reject"),
            None,
        )
        .await?;
    Ok(())
}

/// One team as the page shows it (no network).
pub async fn get(db: &Db, data_dir: &Path, id: &str) -> TeamResult<TeamView> {
    team_view(db, data_dir, id).await
}

// ── Background refresh ──────────────────────────────────────────────────

const POLL_EVERY: Duration = Duration::from_secs(5 * 60);

/// How many join requests are waiting across the teams this server manages.
async fn waiting_requests(db: &Db, data_dir: &Path) -> usize {
    let mut waiting = 0;
    for row in db.list_teams().await.unwrap_or_default() {
        if row.state == "member"
            && matches!(row.role.as_deref(), Some("owner" | "admin"))
            && let Ok(v) = list_requests(db, data_dir, &row.id).await
        {
            waiting += v["requests"].as_array().map_or(0, Vec::len);
        }
    }
    waiting
}

/// Refresh every team every 5 minutes and tell open pages when something
/// changed or a join request is waiting (`team-update`: counts only). Does
/// nothing — not even load the identity key — while this server is in no
/// team. Boot skips this under `PECKBOARD_NO_RESUME`.
pub fn spawn_poller(
    db: Db,
    data_dir: std::path::PathBuf,
    broadcaster: std::sync::Arc<crate::ws::broadcaster::Broadcaster>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(90)).await;
        let mut tick = tokio::time::interval(POLL_EVERY);
        let mut last_waiting = 0;
        loop {
            tick.tick().await;
            let active = db.list_teams().await.is_ok_and(|rows| {
                rows.iter()
                    .any(|r| matches!(r.state.as_str(), "member" | "pending"))
            });
            if !active {
                continue;
            }
            let changed = match refresh_all(&db, &data_dir).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!("teams: refresh failed: {}", e.message);
                    continue;
                }
            };
            let waiting = waiting_requests(&db, &data_dir).await;
            if changed > 0 || waiting != last_waiting {
                last_waiting = waiting;
                broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "team-update".into(),
                    session_id: String::new(),
                    data: json!({ "waiting_requests": waiting }),
                });
            }
        }
    });
}
