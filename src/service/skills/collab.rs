//! Working together on public skills through the hub (`skills.peckboard.com`,
//! contract in `peckboard-skills-hub/API.md`): this install's handle, forks,
//! merge requests, history and diffs, plus the incoming-merge-request
//! poller.
//!
//! A fork lives on the hub as `<name>@<handle>`. "Fork and edit" also keeps
//! a **local** copy under that same name so the ordinary skill editor edits
//! it; publishing that local skill publishes to the fork (the hub routes a
//! `SKILL.md` whose `name` is a fork name to the fork). Syncing a fork, or
//! merging into a skill this install owns, pulls the hub's result back into
//! the local copy so the next publish doesn't undo it.
//!
//! Who may act: every hub write is signed by the install key, so the local
//! rule is the one publishing uses — an admin, or the user who added the
//! local skill involved (the fork copy for fork-side actions, the original
//! for owner-side ones).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::format::{self, FmValue, SkillDoc};
use super::hub::{self, HUB_LABEL, HubClient};
use super::{
    Actor, SkillError, SkillResult, SkillView, bad, parse_tags, sources, tags_json, version_row,
};
use crate::db::Db;
use crate::db::models::{SkillRow, SkillSourceRow};
use crate::service::push::{PushMessage, PushService};
use crate::service::remote_access::IDENTITY_FILE;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Key-value namespace (`plugin_settings`) for the hub client's own state.
pub const SETTINGS_ID: &str = "core.skills-hub";
/// Ids of the incoming open merge requests already announced.
const SEEN_KEY: &str = "seen_incoming_merge_requests";
/// WS event: `{open, new}` incoming merge-request counts (global).
pub const MR_EVENT: &str = "skills-merge-requests";
/// How often the poller asks the hub for incoming merge requests.
const POLL_EVERY: Duration = Duration::from_secs(5 * 60);

// ── Names ───────────────────────────────────────────────────────────────

/// `[a-z0-9-]{2,32}` — a hub handle.
pub fn valid_handle(h: &str) -> bool {
    (2..=32).contains(&h.len())
        && h.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// `name@handle` → `(name, handle)`; `None` for an original skill's name.
pub fn fork_parts(name: &str) -> Option<(&str, &str)> {
    let (base, handle) = name.split_once('@')?;
    (format::valid_name(base) && valid_handle(handle)).then_some((base, handle))
}

/// The handle a key has before choosing one: `k-` + the first 8 hex of
/// `sha256(public key)`.
pub fn default_handle(public_key: &[u8; 32]) -> String {
    format!("k-{}", &hex::encode(Sha256::digest(public_key))[..8])
}

// ── Plumbing ────────────────────────────────────────────────────────────

/// The public source the hub is reached through: the seeded `public`
/// source, else the first public one.
pub async fn public_source(db: &Db) -> SkillResult<SkillSourceRow> {
    if let Some(s) = db.get_skill_source(sources::PUBLIC_SOURCE_ID).await?
        && s.kind == "public"
    {
        return Ok(s);
    }
    db.list_skill_sources()
        .await?
        .into_iter()
        .find(|s| s.kind == "public")
        .ok_or_else(|| {
            bad(format!(
                "No public skill source is set up. Add {HUB_LABEL} under Sources first."
            ))
        })
}

struct Hub {
    client: HubClient,
    source: SkillSourceRow,
}

async fn hub_for(db: &Db) -> SkillResult<Hub> {
    let source = public_source(db).await?;
    Ok(Hub {
        client: HubClient::new(&sources::hub_base_url(&source))?,
        source,
    })
}

fn key_of(identity: &crate::service::remote_access::tunnel::BoxIdentity) -> String {
    STANDARD.encode(identity.public_key())
}

/// A JSON list answer (`[...]`, or `{<key>: [...]}`).
fn list_of(v: Value, key: &str) -> Vec<Value> {
    match v {
        Value::Array(a) => a,
        Value::Object(mut o) => match o.remove(key) {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn versions_of(detail: &Value) -> Vec<String> {
    detail
        .get("versions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Admin, or the user who added the local skill `row` (named `name`).
fn require_local_owner(
    actor: &Actor,
    row: Option<&SkillRow>,
    name: &str,
    what: &str,
) -> SkillResult<()> {
    if actor.is_admin {
        return Ok(());
    }
    match row {
        Some(s) if actor.can_edit(s.created_by.as_deref()) => Ok(()),
        _ => Err(SkillError::Forbidden(format!(
            "Only an admin or the user who added '{name}' may {what}."
        ))),
    }
}

/// The local (scope `local`) skill called `name`, if any.
async fn local_skill(db: &Db, name: &str) -> SkillResult<Option<SkillRow>> {
    Ok(db
        .find_skill(None, name)
        .await?
        .filter(|s| s.scope == "local"))
}

/// Parse a hub `SKILL.md`, whose `name` may be a fork name.
fn parse_hub_skill(text: &str) -> SkillResult<SkillDoc> {
    let (fm, body) = format::split_frontmatter(text);
    let get = |k: &str| fm.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let version = get("version");
    if semver::Version::parse(&version).is_err() {
        return Err(SkillError::Upstream(format!(
            "{HUB_LABEL} sent a SKILL.md without a valid version"
        )));
    }
    Ok(SkillDoc {
        name: get("name"),
        description: get("description"),
        version,
        tags: fm.get("tags").map(FmValue::as_list).unwrap_or_default(),
        groups: fm.get("groups").map(FmValue::as_list).unwrap_or_default(),
        body: body.trim_start_matches(['\n', '\r']).to_string(),
    })
}

/// Store `doc` as the local skill's new head version: `doc.version` when
/// it is newer than the head, else the next free patch version.
async fn store_new_head(db: &Db, s: &SkillRow, doc: &SkillDoc) -> SkillResult<String> {
    let newer = matches!(
        (semver::Version::parse(&doc.version), semver::Version::parse(&s.version)),
        (Ok(d), Ok(h)) if d > h
    );
    let mut next = if newer {
        doc.version.clone()
    } else {
        format::bump_version(&s.version, "patch").map_err(bad)?
    };
    for _ in 0..100 {
        if db.get_skill_version(&s.id, &next).await?.is_none() {
            break;
        }
        next = format::bump_version(&next, "patch").map_err(bad)?;
    }
    let description = if doc.description.is_empty() {
        s.description.clone()
    } else {
        doc.description.clone()
    };
    let tags = if doc.tags.is_empty() {
        parse_tags(&s.tags)
    } else {
        doc.tags.clone()
    };
    db.add_skill_version(
        version_row(&s.id, &next, &doc.body),
        true,
        description,
        tags_json(&tags),
    )
    .await?;
    Ok(next)
}

/// Pull `hub_name`'s `main` into the local skill `s` (after a sync or an
/// unpublished merge, `main` is ahead of every release). False when `main`
/// is the same text the local head was published as.
async fn refresh_from_main(db: &Db, hub: &Hub, s: &SkillRow, hub_name: &str) -> SkillResult<bool> {
    let detail = hub.client.skill(hub_name).await?;
    let main = hub.client.skill_md(hub_name, "main").await?;
    if versions_of(&detail).contains(&s.version)
        && hub.client.skill_md(hub_name, &s.version).await? == main
    {
        return Ok(false);
    }
    store_new_head(db, s, &parse_hub_skill(&main)?).await?;
    Ok(true)
}

// ── Handle ──────────────────────────────────────────────────────────────

/// `{public_key, handle}` of this install on the hub.
pub async fn my_handle(db: &Db, data_dir: &Path) -> SkillResult<Value> {
    let hub = hub_for(db).await?;
    let identity = hub::identity(db, data_dir).await?;
    let key = key_of(&identity);
    let handle = match hub.client.publisher(&key).await {
        Ok(v) => Some(str_field(&v, "handle")).filter(|h| !h.is_empty()),
        // A key the hub has never seen still has its default handle.
        Err(e) if e.status() == 404 => None,
        Err(e) => return Err(e),
    }
    .unwrap_or_else(|| default_handle(&identity.public_key()));
    Ok(json!({ "public_key": key, "handle": handle }))
}

/// Choose this install's handle (admin only: it names every fork).
pub async fn set_handle(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    handle: &str,
) -> SkillResult<Value> {
    actor.require_admin("Changing this PeckBoard's handle")?;
    let handle = handle.trim();
    if !valid_handle(handle) {
        return Err(bad(
            "A handle is 2 to 32 lowercase letters, digits or dashes.",
        ));
    }
    let hub = hub_for(db).await?;
    let identity = hub::identity(db, data_dir).await?;
    let out = hub.client.set_handle(&identity, handle).await?;
    let handle = Some(str_field(&out, "handle"))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| handle.to_string());
    Ok(json!({ "public_key": key_of(&identity), "handle": handle }))
}

// ── Reads ───────────────────────────────────────────────────────────────

/// A hub skill or fork, plus `mine` (this install owns it) and `my_fork`
/// (the fork this install already has of it, if any).
pub async fn skill_detail(db: &Db, data_dir: &Path, name: &str) -> SkillResult<Value> {
    let hub = hub_for(db).await?;
    let mut detail = hub.client.skill(name).await?;
    let identity = hub::identity(db, data_dir).await?;
    let key = key_of(&identity);
    let mine = str_field(&detail, "owner_key") == key;
    let my_fork = if fork_parts(name).is_none() && !mine {
        hub.client
            .forks(name)
            .await
            .map(|f| list_of(f, "forks"))
            .unwrap_or_default()
            .into_iter()
            .find(|f| str_field(f, "owner_key") == key)
            .map(|f| str_field(&f, "fork"))
    } else {
        None
    };
    if let Some(o) = detail.as_object_mut() {
        o.insert("mine".into(), json!(mine));
        o.insert("my_fork".into(), json!(my_fork));
    }
    Ok(detail)
}

/// `[{commit, parents, author_key, author_handle, message, at, tag?}]`.
pub async fn history(db: &Db, name: &str) -> SkillResult<Value> {
    let hub = hub_for(db).await?;
    Ok(Value::Array(list_of(
        hub.client.history(name).await?,
        "history",
    )))
}

/// `[{fork, owner_handle, owner_key, latest, ahead, behind}]`.
pub async fn forks(db: &Db, name: &str) -> SkillResult<Value> {
    let hub = hub_for(db).await?;
    Ok(Value::Array(list_of(
        hub.client.forks(name).await?,
        "forks",
    )))
}

/// `{from, to, diff}` — `rev` is a commit, a version or `main`.
pub async fn diff(db: &Db, name: &str, from: &str, to: &str) -> SkillResult<Value> {
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() || to.is_empty() {
        return Err(bad("Pick two versions to compare."));
    }
    hub_for(db).await?.client.diff(name, from, to).await
}

// ── Forks ───────────────────────────────────────────────────────────────

/// Fork `name` on the hub (or reuse this install's fork) and install the
/// local copy the editor works on. `{fork, skill}`.
pub async fn fork(db: &Db, data_dir: &Path, actor: &Actor, name: &str) -> SkillResult<Value> {
    let name = name.trim();
    if !format::valid_name(name) {
        return Err(bad(format!(
            "'{name}' can't be forked: only an original skill can (a fork can't be forked)."
        )));
    }
    let hub = hub_for(db).await?;
    let identity = hub::identity(db, data_dir).await?;
    let fork_name = match hub.client.fork(&identity, name).await {
        Ok(v) => Some(str_field(&v, "fork"))
            .filter(|f| !f.is_empty())
            .ok_or_else(|| {
                SkillError::Upstream(format!("{HUB_LABEL} didn't say what the fork is called"))
            })?,
        // Forked before (maybe from another user here): open that one.
        Err(SkillError::Refused { code, .. }) if code == "already_forked" => {
            let me = my_handle(db, data_dir).await?;
            format!("{name}@{}", str_field(&me, "handle"))
        }
        Err(e) => return Err(e),
    };
    hub::path_name(&fork_name)?;
    let skill = match local_skill(db, &fork_name).await? {
        Some(existing) => {
            require_local_owner(actor, Some(&existing), &fork_name, "edit it")?;
            super::get(db, &existing.id).await?
        }
        None => install_fork_copy(db, &hub, actor, &fork_name, name).await?,
    };
    Ok(json!({ "fork": fork_name, "skill": skill }))
}

/// The local, editable copy of a fork, at the fork's latest version (the
/// original's, while the fork has none of its own).
async fn install_fork_copy(
    db: &Db,
    hub: &Hub,
    actor: &Actor,
    fork_name: &str,
    parent: &str,
) -> SkillResult<SkillView> {
    let detail = hub.client.skill(fork_name).await?;
    let (from, version) = match detail.get("latest").and_then(Value::as_str) {
        Some(v) if !v.is_empty() => (fork_name, v.to_string()),
        _ => {
            let p = hub.client.skill(parent).await?;
            (parent, str_field(&p, "latest"))
        }
    };
    let doc = parse_hub_skill(&hub.client.skill_md(from, &version).await?)?;
    let description = if doc.description.is_empty() {
        str_field(&detail, "description")
    } else {
        doc.description.clone()
    };
    let now = chrono::Utc::now().to_rfc3339();
    let row = SkillRow {
        id: uuid::Uuid::new_v4().to_string(),
        scope: "local".into(),
        source_id: None,
        name: fork_name.to_string(),
        description,
        tags: tags_json(&doc.tags),
        version: doc.version.clone(),
        created_by: actor.user_id.clone(),
        created_at: now.clone(),
        updated_at: now,
    };
    let id = row.id.clone();
    db.insert_skill_with_version(row, version_row(&id, &doc.version, &doc.body))
        .await?;
    super::get(db, &id).await
}

/// True when the local copy has edits the fork doesn't have yet: its head
/// isn't a fork version and it isn't the untouched copy.
async fn has_unpublished_edits(db: &Db, s: &SkillRow, fork_detail: &Value) -> SkillResult<bool> {
    if versions_of(fork_detail).contains(&s.version) {
        return Ok(false);
    }
    Ok(db.list_skill_versions(&s.id).await?.len() > 1)
}

/// Bring the original's new commits into this install's fork, then into
/// the local copy. `{status, local_updated, skill?}`.
pub async fn sync_fork(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    fork_name: &str,
) -> SkillResult<Value> {
    if fork_parts(fork_name).is_none() {
        return Err(bad(format!("'{fork_name}' is not a fork.")));
    }
    let local = local_skill(db, fork_name).await?;
    require_local_owner(actor, local.as_ref(), fork_name, "sync it")?;
    let hub = hub_for(db).await?;
    if let Some(s) = &local
        && has_unpublished_edits(db, s, &hub.client.skill(fork_name).await?).await?
    {
        return Err(bad(
            "This fork has edits that aren't published yet. Publish them to the fork first, \
             then sync it from the original.",
        ));
    }
    let identity = hub::identity(db, data_dir).await?;
    let out = hub.client.sync_fork(&identity, fork_name).await?;
    let status = str_field(&out, "status");
    let mut local_updated = false;
    if matches!(status.as_str(), "fast_forward" | "merged")
        && let Some(s) = &local
    {
        local_updated = refresh_from_main(db, &hub, s, fork_name).await?;
    }
    let skill = match &local {
        Some(s) => Some(super::get(db, &s.id).await?),
        None => None,
    };
    Ok(json!({ "status": status, "local_updated": local_updated, "skill": skill }))
}

// ── Merge requests ──────────────────────────────────────────────────────

fn mr_id(v: &Value) -> Option<u64> {
    v.get("id").and_then(Value::as_u64)
}

/// Ask the original's author to merge this install's fork. Local edits
/// not on the fork yet are published to it first. `{merge_request,
/// published_version}`.
pub async fn create_merge_request(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    fork_name: &str,
    title: &str,
    description: &str,
) -> SkillResult<Value> {
    if fork_parts(fork_name).is_none() {
        return Err(bad(
            "Only a fork can ask the original's author to merge. Fork the skill first.",
        ));
    }
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(bad(
            "Give the merge request a title (up to 200 characters).",
        ));
    }
    if description.chars().count() > 20_000 {
        return Err(bad(
            "The description is too long (up to 20,000 characters).",
        ));
    }
    let local = local_skill(db, fork_name).await?;
    require_local_owner(actor, local.as_ref(), fork_name, "ask to merge it")?;
    let hub = hub_for(db).await?;
    let mut published = Value::Null;
    if let Some(s) = &local
        && has_unpublished_edits(db, s, &hub.client.skill(fork_name).await?).await?
    {
        let r = super::publish(db, data_dir, actor, &s.id, &hub.source.id).await?;
        published = r.get("version").cloned().unwrap_or(Value::Null);
    }
    let identity = hub::identity(db, data_dir).await?;
    let mr = hub
        .client
        .create_merge_request(&identity, fork_name, title, description.trim())
        .await?;
    Ok(json!({ "merge_request": mr, "published_version": published }))
}

/// This install's merge requests: `incoming` (into skills it owns) or
/// `outgoing` (opened from its forks); `state` open (default) | merged |
/// closed | all.
pub async fn list_merge_requests(
    db: &Db,
    data_dir: &Path,
    role: &str,
    state: &str,
) -> SkillResult<Value> {
    let role = match role.trim() {
        "" | "incoming" | "owner" => "owner",
        "outgoing" | "author" => "author",
        other => {
            return Err(bad(format!("role '{other}' — use incoming or outgoing")));
        }
    };
    let state = match state.trim() {
        "" | "open" => "open",
        s @ ("merged" | "closed" | "all") => s,
        other => {
            return Err(bad(format!(
                "state '{other}' — use open, merged, closed or all"
            )));
        }
    };
    let hub = hub_for(db).await?;
    let identity = hub::identity(db, data_dir).await?;
    let key = key_of(&identity);
    let list = hub
        .client
        .list_merge_requests(&[("key", &key), ("role", role), ("state", state)])
        .await?;
    Ok(Value::Array(list_of(list, "merge_requests")))
}

/// One merge request with its diff and comments.
pub async fn get_merge_request(db: &Db, id: u64) -> SkillResult<Value> {
    hub_for(db).await?.client.merge_request(id).await
}

/// Admin, or the user who added the local fork copy or the local original
/// this merge request involves.
async fn require_mr_party(
    db: &Db,
    actor: &Actor,
    mr: &Value,
    owner_only: bool,
    what: &str,
) -> SkillResult<()> {
    if actor.is_admin {
        return Ok(());
    }
    let target = str_field(mr, "target");
    let mut names = vec![target.clone()];
    if !owner_only {
        names.push(str_field(mr, "fork"));
    }
    for n in &names {
        if let Some(s) = local_skill(db, n).await?
            && actor.can_edit(s.created_by.as_deref())
        {
            return Ok(());
        }
    }
    Err(SkillError::Forbidden(format!(
        "Only an admin or the user who added '{target}' may {what}."
    )))
}

pub async fn comment_merge_request(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    id: u64,
    body: &str,
) -> SkillResult<Value> {
    let body = body.trim();
    if body.is_empty() || body.chars().count() > 20_000 {
        return Err(bad("Write a comment (up to 20,000 characters)."));
    }
    let hub = hub_for(db).await?;
    let mr = hub.client.merge_request(id).await?;
    require_mr_party(db, actor, &mr, false, "comment on it").await?;
    let identity = hub::identity(db, data_dir).await?;
    hub.client.comment_merge_request(&identity, id, body).await
}

/// Merge a fork into a skill this install owns; with `publish`, as a new
/// version (`bump` patch|minor|major, or an exact `version`). The local
/// original follows. `{merge_request, local_updated}`.
pub async fn merge_merge_request(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    id: u64,
    publish: bool,
    bump: Option<&str>,
    version: Option<&str>,
) -> SkillResult<Value> {
    let bump = bump.map(str::trim).filter(|b| !b.is_empty());
    let version = version.map(str::trim).filter(|v| !v.is_empty());
    if let Some(b) = bump
        && !matches!(b, "patch" | "minor" | "major")
    {
        return Err(bad(format!("bump '{b}' — use patch, minor or major")));
    }
    if let Some(v) = version
        && semver::Version::parse(v).is_err()
    {
        return Err(bad(format!("'{v}' is not a version like 1.2.0")));
    }
    let hub = hub_for(db).await?;
    let mr = hub.client.merge_request(id).await?;
    require_mr_party(db, actor, &mr, true, "merge into it").await?;
    let identity = hub::identity(db, data_dir).await?;
    let merged = hub
        .client
        .merge_merge_request(
            &identity,
            id,
            publish,
            bump.or((publish && version.is_none()).then_some("patch")),
            version,
        )
        .await?;
    let target = str_field(&mr, "target");
    let mut local_updated = false;
    if let Some(s) = local_skill(db, &target).await? {
        let merged_version = str_field(&merged, "merged_version");
        let r: SkillResult<bool> = async {
            if !(publish && !merged_version.is_empty()) {
                return refresh_from_main(db, &hub, &s, &target).await;
            }
            if db
                .get_skill_version(&s.id, &merged_version)
                .await?
                .is_some()
            {
                return Ok(false);
            }
            let mut doc = parse_hub_skill(&hub.client.skill_md(&target, &merged_version).await?)?;
            doc.version = merged_version.clone();
            store_new_head(db, &s, &doc).await.map(|_| true)
        }
        .await;
        // The merge happened; a local copy that couldn't follow is logged,
        // not reported as a failed merge.
        local_updated = r.unwrap_or_else(|e| {
            tracing::warn!(skill = %target, "skills hub: local copy not updated after merge: {e}");
            false
        });
    }
    Ok(json!({ "merge_request": merged, "local_updated": local_updated }))
}

pub async fn close_merge_request(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    id: u64,
) -> SkillResult<Value> {
    let hub = hub_for(db).await?;
    let mr = hub.client.merge_request(id).await?;
    require_mr_party(db, actor, &mr, false, "close it").await?;
    let identity = hub::identity(db, data_dir).await?;
    hub.client.close_merge_request(&identity, id).await
}

// ── Incoming merge-request poller ───────────────────────────────────────

/// Ask the hub for the open merge requests into skills this install owns;
/// announce the ones not seen before (web push) and broadcast the counts
/// (`skills-merge-requests`) so the UI badge follows. `{open, new}`.
pub async fn check_incoming(
    db: &Db,
    data_dir: &Path,
    broadcaster: Option<&Broadcaster>,
    push: Option<&PushService>,
) -> SkillResult<Value> {
    // Never mint an identity key just to poll.
    if !data_dir.join(IDENTITY_FILE).exists() {
        return Ok(json!({ "open": 0, "new": [] }));
    }
    let open = list_merge_requests(db, data_dir, "incoming", "open").await?;
    let items = open.as_array().cloned().unwrap_or_default();
    let ids: Vec<u64> = items.iter().filter_map(mr_id).collect();
    let seen: Vec<u64> = db
        .list_plugin_settings(SETTINGS_ID)
        .await?
        .get(SEEN_KEY)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let new: Vec<Value> = items
        .iter()
        .filter(|m| mr_id(m).is_some_and(|i| !seen.contains(&i)))
        .cloned()
        .collect();
    if ids != seen {
        db.set_plugin_setting(SETTINGS_ID, SEEN_KEY, &json!(ids))
            .await?;
    }
    if let Some(b) = broadcaster {
        b.broadcast(WsEvent {
            event_type: MR_EVENT.into(),
            session_id: String::new(),
            data: json!({ "open": ids.len(), "new": new.len() }),
        });
    }
    if !new.is_empty()
        && let Some(push) = push
        && !crate::service::push::delivery_suppressed()
    {
        let body = match new.as_slice() {
            [m] => format!(
                "{} asks to merge {} into {}: {}",
                str_field(m, "author_handle"),
                str_field(m, "fork"),
                str_field(m, "target"),
                str_field(m, "title")
            ),
            many => format!("{} new merge requests for your skills", many.len()),
        };
        let msg = PushMessage {
            title: "Merge request".into(),
            body,
            url: "/settings/skills?tab=merge-requests".into(),
            tag: "peckboard-skills-mr".into(),
        };
        push.send_to_all(db, &crate::service::push::http_client(), &msg, 24 * 60 * 60)
            .await;
    }
    Ok(json!({ "open": ids.len(), "new": new }))
}

/// True when this install has an original local skill — the only kind
/// anyone can open a merge request into.
async fn has_local_original(db: &Db) -> bool {
    db.list_skills()
        .await
        .map(|all| {
            all.iter()
                .any(|s| s.scope == "local" && !s.name.contains('@'))
        })
        .unwrap_or(false)
}

/// Poll [`check_incoming`] every 5 minutes (boot skips this under
/// `PECKBOARD_NO_RESUME`; `POST /api/skills/hub/merge-requests/check` runs
/// it on demand).
pub fn spawn_poller(db: Db, data_dir: PathBuf, broadcaster: Arc<Broadcaster>, push: PushService) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut tick = tokio::time::interval(POLL_EVERY);
        loop {
            tick.tick().await;
            if !has_local_original(&db).await {
                continue;
            }
            if let Err(e) = check_incoming(&db, &data_dir, Some(&broadcaster), Some(&push)).await {
                tracing::debug!("skills hub: merge-request poll failed: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_names_split_and_handles_validate() {
        assert_eq!(fork_parts("pr-review@alice"), Some(("pr-review", "alice")));
        assert_eq!(fork_parts("pr-review"), None);
        assert!(!valid_handle("A"));
        assert!(default_handle(&[0u8; 32]).starts_with("k-"));
    }

    use std::sync::Mutex;

    use axum::body::Bytes;
    use axum::extract::{Path as AxPath, State};
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use peckboard_relay::identity::verify_skills_request;

    use crate::service::skills::{self, CreateSkill, SourceInput};

    #[derive(Default)]
    struct Stub {
        nonces: Vec<String>,
        /// `(METHOD path, body)` of every signed request that verified.
        signed: Vec<(String, Value)>,
    }
    type Shared = Arc<Mutex<Stub>>;

    fn skill_md(name: &str, version: &str, line: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: d\nversion: {version}\ntags: []\ngroups: []\n---\n# {name}\n\n{line}\n"
        )
    }

    /// Checks the `X-Peck-*` headers like the hub: single-use nonce,
    /// signature over method, path, nonce and the body's digest.
    fn verify(hub: &Shared, method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> bool {
        let h = |k: &str| {
            headers
                .get(k)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
        };
        let mut st = hub.lock().unwrap();
        let Some(i) = st.nonces.iter().position(|n| n == h("x-peck-nonce")) else {
            return false;
        };
        st.nonces.remove(i);
        let key: [u8; 32] = STANDARD
            .decode(h("x-peck-key"))
            .unwrap()
            .try_into()
            .unwrap();
        let sig: [u8; 64] = STANDARD
            .decode(h("x-peck-signature"))
            .unwrap()
            .try_into()
            .unwrap();
        let digest = hex::encode(Sha256::digest(body));
        let ok = verify_skills_request(
            &key,
            method.as_str(),
            uri.path(),
            h("x-peck-nonce"),
            &digest,
            &sig,
        );
        if ok {
            let v = serde_json::from_slice(body).unwrap_or(Value::Null);
            st.signed.push((format!("{method} {}", uri.path()), v));
        }
        ok
    }

    async fn signed_route(
        State(hub): State<Shared>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<Value>) {
        if !verify(&hub, &method, &uri, &headers, &body) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "bad signature", "code": "bad_signature"})),
            );
        }
        match uri.path() {
            "/api/fork" => (
                StatusCode::CREATED,
                Json(json!({"fork": "orig@me", "url": "https://hub.test/s/orig@me"})),
            ),
            "/api/merge-requests/7/merge" => (
                StatusCode::OK,
                Json(
                    json!({"id": 7, "target": "mine", "fork": "mine@bob", "state": "merged",
                    "merged_commit": "abc", "merged_version": "1.1.0"}),
                ),
            ),
            _ => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "nope", "code": "not_found"})),
            ),
        }
    }

    async fn challenge(State(hub): State<Shared>) -> Json<Value> {
        let nonce = STANDARD.encode(rand::random::<[u8; 16]>());
        hub.lock().unwrap().nonces.push(nonce.clone());
        Json(json!({"nonce": nonce, "expires_in": 60, "pow_difficulty": 0}))
    }

    async fn skill_detail_route(AxPath(name): AxPath<String>) -> Json<Value> {
        Json(match name.as_str() {
            "orig" => {
                json!({"name": "orig", "owner_key": "other", "latest": "1.0.0", "versions": ["1.0.0"]})
            }
            "orig@me" => {
                json!({"name": "orig@me", "parent": "orig", "latest": "1.0.0", "versions": ["1.0.0"]})
            }
            _ => json!({"name": name, "latest": "1.1.0", "versions": ["1.1.0", "1.0.0"]}),
        })
    }

    async fn skill_file(AxPath((name, version)): AxPath<(String, String)>) -> String {
        let line = if name == "mine" {
            "Merged from bob."
        } else {
            "Original text."
        };
        skill_md(&name, &version, line)
    }

    #[tokio::test]
    async fn fork_merge_and_incoming_poll_use_signed_requests() {
        let hub: Shared = Arc::default();
        let app = Router::new()
            .route("/api/challenge", get(challenge))
            .route("/api/fork", post(signed_route))
            .route("/api/merge-requests/{id}/merge", post(signed_route))
            .route(
                "/api/merge-requests/{id}",
                get(|| async {
                    Json(json!({"id": 7, "target": "mine", "fork": "mine@bob", "state": "open"}))
                }),
            )
            .route(
                "/api/merge-requests",
                get(|| async {
                    Json(json!([{"id": 7, "target": "mine", "fork": "mine@bob",
                        "title": "Better wording", "author_handle": "bob", "state": "open"}]))
                }),
            )
            .route("/api/skills/{name}", get(skill_detail_route))
            .route("/skills/{name}/{version}/SKILL.md", get(skill_file))
            .with_state(hub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Db::in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let admin = Actor {
            user_id: Some("u1".into()),
            is_admin: true,
        };
        // Point the hub at the stub instead of the seeded catalog.
        let _ = db.delete_skill_source(sources::PUBLIC_SOURCE_ID).await;
        skills::create_source(
            &db,
            &admin,
            SourceInput {
                kind: "public".into(),
                name: "stub hub".into(),
                url: format!("http://{addr}/index.json"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Fork and edit: a signed POST /api/fork, then a local copy of the
        // fork's latest version under the fork's name.
        let out = fork(&db, dir.path(), &admin, "orig").await.unwrap();
        assert_eq!(out["fork"], "orig@me");
        let copy = db.find_skill(None, "orig@me").await.unwrap().unwrap();
        assert_eq!(copy.version, "1.0.0");
        let body = db
            .get_skill_version(&copy.id, "1.0.0")
            .await
            .unwrap()
            .unwrap()
            .body;
        assert!(body.contains("Original text."), "{body}");

        // Merge (publishing a minor version) into a skill this install owns:
        // the local original follows to the merged version.
        let mine = skills::create(
            &db,
            &admin,
            CreateSkill {
                name: "mine".into(),
                description: "d".into(),
                tags: vec![],
                groups: vec![],
                body: "# mine\n\nOld text.\n".into(),
                version: None,
            },
        )
        .await
        .unwrap();
        let merged = merge_merge_request(&db, dir.path(), &admin, 7, true, Some("minor"), None)
            .await
            .unwrap();
        assert_eq!(merged["merge_request"]["state"], "merged");
        assert_eq!(merged["local_updated"], true);
        let head = db.get_skill(&mine.id.unwrap()).await.unwrap().unwrap();
        assert_eq!(head.version, "1.1.0");
        let signed = hub.lock().unwrap().signed.clone();
        assert_eq!(signed[0].0, "POST /api/fork");
        assert_eq!(signed[0].1, json!({"name": "orig"}));
        assert_eq!(signed[1].0, "POST /api/merge-requests/7/merge");
        assert_eq!(signed[1].1, json!({"publish": true, "bump": "minor"}));

        // Incoming poll: new once, then seen.
        let first = check_incoming(&db, dir.path(), None, None).await.unwrap();
        assert_eq!(first["open"], 1);
        assert_eq!(first["new"].as_array().unwrap().len(), 1);
        let again = check_incoming(&db, dir.path(), None, None).await.unwrap();
        assert_eq!(again["new"].as_array().unwrap().len(), 0);

        // A request the hub can't verify maps to plain words.
        let err = hub::request_error(
            401,
            r#"{"error":"x","code":"bad_signature"}"#,
            "fork the skill",
        );
        assert!(err.to_string().contains("couldn't verify"), "{err}");
        let err = hub::request_error(409, r#"{"error":"x","code":"not_mergeable"}"#, "merge");
        assert!(
            err.to_string().contains("needs an update from the fork"),
            "{err}"
        );
        let err = hub::request_error(409, r#"{"error":"x","code":"already_open"}"#, "open it");
        assert_eq!(
            err.to_string(),
            "This fork already has an open merge request."
        );
    }
}
