//! Collaboration endpoints: handles, skill info / history / diff, forks and
//! merge requests (see `API.md`). Every write is signed with `X-Peck-*`
//! headers ([`crate::api::verify_signed`]).
//!
//! A fork `<name>@<handle>` is a bare repo cloned from the original's
//! `main`, so the two share history. After each write to a fork its `main`
//! is fetched into the original's repo as `refs/forks/<fork>`
//! ([`refresh_fork`]); ahead/behind, mergeability and merges are computed
//! there. Merges are 3-way `merge_trees` of `SKILL.md` with the frontmatter
//! `name` / `version` neutralized (they always differ between a skill and a
//! fork); the writer stamps the right ones on the result.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use git2::{Oid, Repository};
use peckboard_skill_format::format;
use peckboard_skill_format::guard::{self, NoSecrets};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::api::{
    ApiError, ApiResult, bad_key, content_findings, rate_limited, read_body, verify_signed,
};
use crate::git::{self, Author, MAIN, Merge, fork_ref, main_head};
use crate::store::{self, MrFilter, MrRow, SkillRow, Store, commit_version};
use crate::{Hub, auth};

const MAX_SIGNED_BODY: usize = 64 * 1024;
const MAX_TITLE: usize = 200;
const MAX_TEXT: usize = 20_000;
const UPSTREAM: &str = "refs/upstream/main";

pub fn routes(r: Router<Arc<Hub>>) -> Router<Arc<Hub>> {
    r.route("/api/handle", post(set_handle))
        .route("/api/publishers/{key}", get(publisher))
        .route("/api/skills/{name}", get(skill_info))
        .route("/api/skills/{name}/history", get(history))
        .route("/api/skills/{name}/forks", get(forks))
        .route("/api/skills/{name}/diff", get(diff))
        .route("/api/fork", post(fork))
        .route("/api/forks/{fork}/sync", post(sync))
        .route("/api/merge-requests", post(open_mr).get(list_mrs))
        .route("/api/merge-requests/{id}", get(get_mr))
        .route("/api/merge-requests/{id}/comments", post(comment))
        .route("/api/merge-requests/{id}/merge", post(merge))
        .route("/api/merge-requests/{id}/close", post(close))
}

// ── helpers ─────────────────────────────────────────────────────────────

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

fn parse<T: DeserializeOwned>(body: &[u8], shape: &str) -> ApiResult<T> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("Expected JSON {shape}: {e}")))
}

/// A skill or fork that exists and isn't entirely taken down.
pub(crate) fn visible(store: &Store, name: &str) -> ApiResult<SkillRow> {
    let gone = || ApiError::not_found("Skill");
    if !format::valid_skill_name(name) {
        return Err(gone());
    }
    let row = store.skill(name)?.ok_or_else(gone)?;
    if store.fully_removed(name)? {
        return Err(gone());
    }
    if let Some(p) = &row.parent
        && store.fully_removed(p)?
    {
        return Err(gone());
    }
    Ok(row)
}

fn not_mergeable(msg: impl Into<String>) -> ApiError {
    ApiError::conflict("not_mergeable", msg)
}

/// Fetch `fork`'s `main` into `parent`'s `refs/forks/<fork>` and point
/// its open merge request at it. Failures are logged (the next write to
/// the fork retries).
pub(crate) fn refresh_fork(hub: &Hub, fork: &str, parent: &str) {
    if let Err(e) = try_refresh_fork(hub, fork, parent) {
        warn!("refresh {fork} into {parent}: {e:#}");
    }
}

fn try_refresh_fork(hub: &Hub, fork: &str, parent: &str) -> anyhow::Result<()> {
    let head = {
        let lock = hub.repos.lock(parent);
        hub.repos.fetch(&lock, fork, MAIN, &fork_ref(fork))?
    };
    let Some(head) = head else {
        return Ok(());
    };
    let repo = hub.repos.open_repo(parent)?;
    let base = main_head(&repo)
        .and_then(|m| repo.merge_base(m, head).ok())
        .map(|o| o.to_string())
        .unwrap_or_default();
    hub.store()
        .update_open_mr_head(fork, &head.to_string(), &base)?;
    Ok(())
}

// ── handles ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct HandleReq {
    handle: String,
}

async fn set_handle(
    State(hub): State<Arc<Hub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: HandleReq = parse(&s.body, "{handle}")?;
    let handle = req.handle.trim();
    if !format::valid_handle(handle) {
        return Err(ApiError::bad_request(
            "A handle is 2 to 32 lowercase letters, digits and dashes.",
        ));
    }
    if handle.starts_with("k-") && handle != store::default_handle(&s.key) {
        return Err(ApiError::bad_request(
            "Handles starting with 'k-' are reserved for keys that haven't picked one.",
        ));
    }
    let store = hub.store();
    if !store.set_handle(&s.key, handle)? {
        return Err(ApiError::conflict(
            "handle_taken",
            format!("The handle '{handle}' is taken. Pick another one."),
        ));
    }
    store.audit("handle", Some(&s.key), handle);
    Ok(Json(serde_json::json!({ "handle": handle })).into_response())
}

async fn publisher(State(hub): State<Arc<Hub>>, Path(key): Path<String>) -> ApiResult<Response> {
    let (_, key) = auth::parse_public_key(&key.replace(' ', "+")).ok_or_else(bad_key)?;
    let handle = hub.store().handle(&key)?;
    Ok(Json(serde_json::json!({ "public_key": key, "handle": handle })).into_response())
}

// ── reads ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct SkillInfo {
    name: String,
    owner_handle: String,
    owner_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    latest: Option<String>,
    versions: Vec<String>,
    description: String,
    tags: Vec<String>,
    groups: Vec<String>,
}

async fn skill_info(State(hub): State<Arc<Hub>>, Path(name): Path<String>) -> ApiResult<Response> {
    let store = hub.store();
    let row = visible(&store, &name)?;
    let versions = store.listed_versions(&name)?;
    let latest = versions.first().cloned();
    let (text, groups) = match &latest {
        Some(v) => (
            store.served_skill(&hub.repos, &name, v)?,
            store.version(&name, v)?.map(|r| r.groups),
        ),
        None => {
            let text = hub
                .repos
                .open_repo(&name)
                .ok()
                .and_then(|r| main_head(&r).and_then(|h| git::skill_at(&r, h).ok().flatten()));
            (text, None)
        }
    };
    let doc = text.and_then(|t| format::parse_skill(&t).ok());
    let info = SkillInfo {
        owner_handle: store.handle(&row.owner_key)?,
        owner_key: row.owner_key,
        parent: row.parent,
        latest,
        versions,
        description: doc
            .as_ref()
            .map(|d| d.description.clone())
            .unwrap_or_default(),
        tags: doc.as_ref().map(|d| d.tags.clone()).unwrap_or_default(),
        groups: groups
            .or_else(|| doc.as_ref().map(|d| d.groups.clone()))
            .unwrap_or_default(),
        name: row.name,
    };
    Ok(Json(info).into_response())
}

#[derive(Serialize)]
struct HistoryEntry {
    commit: String,
    parents: Vec<String>,
    author_key: Option<String>,
    author_handle: String,
    message: String,
    at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

async fn history(State(hub): State<Arc<Hub>>, Path(name): Path<String>) -> ApiResult<Response> {
    let store = hub.store();
    visible(&store, &name)?;
    let commits = hub.repos.history(&name)?;
    let tags = match hub.repos.open_repo(&name) {
        Ok(r) => git::tags(&r)?,
        Err(_) => Default::default(),
    };
    let mut out = Vec::with_capacity(commits.len());
    for c in commits {
        let author_key = store.commit_author(&c.id)?;
        let author_handle = match &author_key {
            Some(k) => store.handle(k)?,
            None => c.author.clone(),
        };
        out.push(HistoryEntry {
            tag: tags.get(&c.id).cloned(),
            commit: c.id,
            parents: c.parents,
            author_key,
            author_handle,
            message: c.message,
            at: crate::rfc3339_at(c.at),
        });
    }
    Ok(Json(out).into_response())
}

#[derive(Serialize)]
struct ForkEntry {
    fork: String,
    owner_handle: String,
    owner_key: String,
    latest: Option<String>,
    ahead: usize,
    behind: usize,
}

async fn forks(State(hub): State<Arc<Hub>>, Path(name): Path<String>) -> ApiResult<Response> {
    let store = hub.store();
    let row = visible(&store, &name)?;
    let mut out = Vec::new();
    if row.parent.is_some() {
        return Ok(Json(out).into_response());
    }
    let repo = hub.repos.open_repo(&name).ok();
    let main = repo.as_ref().and_then(main_head);
    for f in store.forks_of(&name)? {
        let (ahead, behind) = match (&repo, main) {
            (Some(r), Some(m)) => r
                .refname_to_id(&fork_ref(&f.name))
                .ok()
                .and_then(|h| r.graph_ahead_behind(h, m).ok())
                .unwrap_or((0, 0)),
            _ => (0, 0),
        };
        out.push(ForkEntry {
            latest: store.listed_versions(&f.name)?.into_iter().next(),
            owner_handle: store.handle(&f.owner_key)?,
            owner_key: f.owner_key,
            fork: f.name,
            ahead,
            behind,
        });
    }
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
struct DiffQuery {
    from: Option<String>,
    to: Option<String>,
}

async fn diff(
    State(hub): State<Arc<Hub>>,
    Path(name): Path<String>,
    Query(q): Query<DiffQuery>,
) -> ApiResult<Response> {
    let (Some(from), Some(to)) = (q.from, q.to) else {
        return Err(ApiError::bad_request(
            "Pass from and to: a commit id, a version or main.",
        ));
    };
    let store = hub.store();
    visible(&store, &name)?;
    let repo = hub
        .repos
        .open_repo(&name)
        .map_err(|_| ApiError::not_found("Skill"))?;
    let removed = store.removed_commits(&name)?;
    let resolve = |rev: &str| -> ApiResult<Oid> {
        let oid = if rev.contains('.') {
            store
                .version(&name, rev)?
                .filter(|v| !v.removed)
                .and_then(|v| v.commit_id)
                .and_then(|c| Oid::from_str(&c).ok())
        } else {
            git::resolve(&repo, rev)
        };
        oid.filter(|o| !removed.contains(&o.to_string()))
            .ok_or_else(|| ApiError::not_found(&format!("Revision '{rev}'")))
    };
    let (a, b) = (resolve(&from)?, resolve(&to)?);
    let text = git::diff_commits(&repo, a, b)?;
    Ok(Json(serde_json::json!({
        "from": a.to_string(),
        "to": b.to_string(),
        "diff": text,
    }))
    .into_response())
}

// ── forks ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ForkReq {
    name: String,
}

async fn fork(
    State(hub): State<Arc<Hub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: ForkReq = parse(&s.body, "{name}")?;
    if req.name.contains('@') {
        return Err(ApiError::bad_request(
            "A fork can't be forked. Fork the original skill.",
        ));
    }
    let already = || {
        ApiError::conflict(
            "already_forked",
            format!("You already have a fork of '{}'.", req.name),
        )
    };
    let handle = {
        let store = hub.store();
        visible(&store, &req.name)?;
        if store.fork_by(&req.name, &s.key)?.is_some() {
            return Err(already());
        }
        store.handle(&s.key)?
    };
    let fork = format!("{}@{handle}", req.name);
    {
        let lock = hub.repos.lock(&fork);
        let store = hub.store();
        if store.fork_by(&req.name, &s.key)?.is_some() {
            return Err(already());
        }
        if store.skill(&fork)?.is_some() {
            return Err(ApiError::conflict(
                "name_taken",
                format!("The name '{fork}' is taken. Pick another handle and try again."),
            ));
        }
        if hub.repos.fetch(&lock, &req.name, MAIN, MAIN)?.is_none() {
            return Err(ApiError::not_found("Skill"));
        }
        store.insert_fork(&fork, &s.key, &req.name)?;
        store.audit("fork", Some(&s.key), &fork);
    }
    refresh_fork(&hub, &fork, &req.name);
    hub.invalidate_index();
    let url = format!("{}/api/skills/{fork}", hub.cfg.public_url);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "fork": fork, "url": url })),
    )
        .into_response())
}

async fn sync(
    State(hub): State<Arc<Hub>>,
    Path(fork): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let parent = {
        let store = hub.store();
        let row = visible(&store, &fork)?;
        let Some(parent) = row.parent else {
            return Err(ApiError::bad_request(format!("'{fork}' is not a fork.")));
        };
        if row.owner_key != s.key {
            return Err(ApiError::not_owner("Only the fork's owner can sync it."));
        }
        parent
    };
    let status = {
        let lock = hub.repos.lock(&fork);
        let Some(up) = hub.repos.fetch(&lock, &parent, MAIN, UPSTREAM)? else {
            return Ok(Json(serde_json::json!({ "status": "up_to_date" })).into_response());
        };
        let repo = hub.repos.open_scratch(&fork)?;
        match main_head(&repo) {
            Some(h) if git::contains(&repo, h, up) => "up_to_date",
            None => {
                hub.repos.set_main(&lock, up)?;
                "fast_forward"
            }
            Some(h) if git::contains(&repo, up, h) => {
                hub.repos.set_main(&lock, up)?;
                "fast_forward"
            }
            Some(h) => match git::merge_skill(&repo, h, up)? {
                Merge::Conflict => "conflict",
                Merge::Clean(text) => {
                    let ours = git::skill_at(&repo, h)?.unwrap_or_default();
                    let text = git::stamp(&text, &fork, &git::version_of(&ours));
                    let store = hub.store();
                    let handle = store.handle(&s.key)?;
                    let oid = hub.repos.commit(
                        &lock,
                        &text,
                        Some(up),
                        &Author {
                            handle: &handle,
                            at: None,
                        },
                        &format!("Sync with {parent}"),
                        None,
                    )?;
                    store.record_author(&oid.to_string(), &s.key)?;
                    "merged"
                }
            },
        }
    };
    if matches!(status, "fast_forward" | "merged") {
        hub.store().audit("sync", Some(&s.key), &fork);
        refresh_fork(&hub, &fork, &parent);
    }
    Ok(Json(serde_json::json!({ "status": status })).into_response())
}

// ── merge requests ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct CommentView {
    id: i64,
    author_handle: String,
    author_key: String,
    body: String,
    at: String,
}

#[derive(Serialize)]
struct MrView {
    id: i64,
    target: String,
    fork: String,
    title: String,
    description: String,
    state: String,
    author_key: String,
    author_handle: String,
    target_owner_key: String,
    base_commit: String,
    head_commit: String,
    mergeable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comments: Option<Vec<CommentView>>,
    created_at: String,
    updated_at: String,
    merged_commit: Option<String>,
    merged_version: Option<String>,
}

/// An open MR as it stands now in the target's repo.
struct Live {
    main: Oid,
    head: Oid,
    base: Option<Oid>,
    /// Clean merge result (neutral frontmatter); `None` = conflict or
    /// nothing to merge.
    merged: Option<String>,
}

fn live(repo: &Repository, fork: &str) -> anyhow::Result<Option<Live>> {
    let (Some(main), Ok(head)) = (main_head(repo), repo.refname_to_id(&fork_ref(fork))) else {
        return Ok(None);
    };
    let merged = if git::contains(repo, main, head) {
        None
    } else {
        match git::merge_skill(repo, main, head)? {
            Merge::Clean(t) => Some(t),
            Merge::Conflict => None,
        }
    };
    Ok(Some(Live {
        main,
        head,
        base: repo.merge_base(main, head).ok(),
        merged,
    }))
}

fn comment_view(store: &Store, c: store::CommentRow) -> anyhow::Result<CommentView> {
    Ok(CommentView {
        id: c.id,
        author_handle: store.handle(&c.author_key)?,
        author_key: c.author_key,
        body: c.body,
        at: c.at,
    })
}

fn mr_view(hub: &Hub, mut row: MrRow, detail: bool) -> ApiResult<MrView> {
    let (author_handle, comments) = {
        let store = hub.store();
        let comments = if detail {
            Some(
                store
                    .comments(row.id)?
                    .into_iter()
                    .map(|c| comment_view(&store, c))
                    .collect::<anyhow::Result<Vec<_>>>()?,
            )
        } else {
            None
        };
        (store.handle(&row.author_key)?, comments)
    };
    let mut mergeable = false;
    let mut diff = None;
    if let Ok(repo) = hub.repos.open_scratch(&row.target) {
        if row.state == "open" {
            if let Some(l) = live(&repo, &row.fork)? {
                row.head_commit = l.head.to_string();
                row.base_commit = l.base.map(|b| b.to_string()).unwrap_or_default();
                mergeable = l.merged.is_some();
                if detail {
                    diff = Some(match (&l.merged, l.base) {
                        (Some(t), _) => {
                            let main_text = git::skill_at(&repo, l.main)?.unwrap_or_default();
                            let after = git::stamp(t, &row.target, &git::version_of(&main_text));
                            git::diff_texts(&main_text, &after)?
                        }
                        (None, Some(b)) => git::diff_commits(&repo, b, l.head)?,
                        (None, None) => String::new(),
                    });
                }
            }
        } else if detail {
            let (from, to) = match (&row.merged_commit, row.state.as_str()) {
                (Some(m), "merged") => {
                    let m = Oid::from_str(m).ok();
                    let first =
                        m.and_then(|m| repo.find_commit(m).ok().and_then(|c| c.parent_id(0).ok()));
                    (first, m)
                }
                _ => (
                    Oid::from_str(&row.base_commit).ok(),
                    Oid::from_str(&row.head_commit).ok(),
                ),
            };
            diff = Some(match (from, to) {
                (Some(a), Some(b)) => git::diff_commits(&repo, a, b).unwrap_or_default(),
                _ => String::new(),
            });
        }
    }
    Ok(MrView {
        id: row.id,
        target: row.target,
        fork: row.fork,
        title: row.title,
        description: row.description,
        state: row.state,
        author_key: row.author_key,
        author_handle,
        target_owner_key: row.target_owner_key,
        base_commit: row.base_commit,
        head_commit: row.head_commit,
        mergeable,
        diff,
        comments,
        created_at: row.created_at,
        updated_at: row.updated_at,
        merged_commit: row.merged_commit,
        merged_version: row.merged_version,
    })
}

fn mr_row(hub: &Hub, id: i64) -> ApiResult<MrRow> {
    hub.store()
        .mr(id)?
        .ok_or_else(|| ApiError::not_found("Merge request"))
}

#[derive(Deserialize)]
struct OpenMrReq {
    fork: String,
    title: String,
    #[serde(default)]
    description: String,
}

async fn open_mr(
    State(hub): State<Arc<Hub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: OpenMrReq = parse(&s.body, "{fork, title, description}")?;
    let title = req.title.trim();
    if title.is_empty() || title.chars().count() > MAX_TITLE {
        return Err(ApiError::bad_request(format!(
            "A merge request needs a title of 1 to {MAX_TITLE} characters."
        )));
    }
    if req.description.chars().count() > MAX_TEXT {
        return Err(ApiError::bad_request(format!(
            "The description is over {MAX_TEXT} characters."
        )));
    }
    let (parent, target_owner) = {
        let store = hub.store();
        let row = visible(&store, &req.fork)?;
        let Some(parent) = row.parent else {
            return Err(ApiError::bad_request(format!(
                "'{}' is not a fork.",
                req.fork
            )));
        };
        if row.owner_key != s.key {
            return Err(ApiError::not_owner(
                "Only the fork's owner can open a merge request from it.",
            ));
        }
        let owner = store.skill_owner(&parent)?.unwrap_or_default();
        (parent, owner)
    };
    refresh_fork(&hub, &req.fork, &parent);
    let (base, head) = {
        let repo = hub.repos.open_repo(&parent)?;
        let head = repo.refname_to_id(&fork_ref(&req.fork)).ok();
        let base = match (main_head(&repo), head) {
            (Some(m), Some(h)) => repo.merge_base(m, h).ok(),
            _ => None,
        };
        (
            base.map(|o| o.to_string()).unwrap_or_default(),
            head.map(|o| o.to_string()).unwrap_or_default(),
        )
    };
    let id = {
        let _lock = hub.repos.lock(&req.fork);
        let store = hub.store();
        if let Some(open) = store.open_mr_for_fork(&req.fork)? {
            return Err(ApiError::conflict(
                "already_open",
                format!(
                    "This fork already has an open merge request (#{}). Close it first.",
                    open.id
                ),
            ));
        }
        let id = store.insert_mr(
            &parent,
            &req.fork,
            title,
            &req.description,
            &s.key,
            &target_owner,
            &base,
            &head,
        )?;
        store.audit(
            "mr_open",
            Some(&s.key),
            &format!("#{id} {} → {parent}", req.fork),
        );
        id
    };
    let view = mr_view(&hub, mr_row(&hub, id)?, true)?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

#[derive(Deserialize)]
struct MrQuery {
    target: Option<String>,
    key: Option<String>,
    role: Option<String>,
    state: Option<String>,
}

async fn list_mrs(State(hub): State<Arc<Hub>>, Query(q): Query<MrQuery>) -> ApiResult<Response> {
    let state = match q.state.as_deref().unwrap_or("open") {
        "all" => None,
        s @ ("open" | "merged" | "closed") => Some(s),
        _ => {
            return Err(ApiError::bad_request(
                "state is open, merged, closed or all.",
            ));
        }
    };
    let key = match &q.key {
        Some(k) => Some(
            auth::parse_public_key(&k.replace(' ', "+"))
                .ok_or_else(bad_key)?
                .1,
        ),
        None => None,
    };
    let filter = match (&q.target, &key, q.role.as_deref()) {
        (Some(t), _, _) => MrFilter::Target(t),
        (None, Some(k), Some("owner")) => MrFilter::Owner(k),
        (None, Some(k), Some("author") | None) => MrFilter::Author(k),
        (None, Some(_), Some(_)) => {
            return Err(ApiError::bad_request("role is owner or author."));
        }
        (None, None, _) => {
            return Err(ApiError::bad_request(
                "Pass target=<name>, or key=<public_key>&role=owner|author.",
            ));
        }
    };
    let rows = hub.store().mrs(filter, state)?;
    let out = rows
        .into_iter()
        .map(|r| mr_view(&hub, r, false))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(out).into_response())
}

async fn get_mr(State(hub): State<Arc<Hub>>, Path(id): Path<i64>) -> ApiResult<Response> {
    let view = mr_view(&hub, mr_row(&hub, id)?, true)?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
struct CommentReq {
    body: String,
}

async fn comment(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: CommentReq = parse(&s.body, "{body}")?;
    let text = req.body.trim();
    if text.is_empty() || text.chars().count() > MAX_TEXT {
        return Err(ApiError::bad_request(format!(
            "A comment is 1 to {MAX_TEXT} characters."
        )));
    }
    let row = mr_row(&hub, id)?;
    if s.key != row.target_owner_key && s.key != row.author_key {
        return Err(ApiError::not_owner(
            "Only the skill's owner and the merge request's author can comment.",
        ));
    }
    let store = hub.store();
    let c = store.add_comment(id, &s.key, text)?;
    let view = comment_view(&store, c)?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

#[derive(Deserialize)]
struct MergeReq {
    #[serde(default)]
    publish: bool,
    #[serde(default)]
    bump: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

async fn merge(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let req: MergeReq = if s.body.is_empty() {
        MergeReq {
            publish: false,
            bump: None,
            version: None,
        }
    } else {
        parse(&s.body, "{publish, bump?, version?}")?
    };
    if let Some(b) = &req.bump
        && !matches!(b.as_str(), "patch" | "minor" | "major")
    {
        return Err(ApiError::bad_request("bump is patch, minor or major."));
    }
    let row = mr_row(&hub, id)?;
    if s.key != row.target_owner_key {
        return Err(ApiError::not_owner("Only the skill's owner can merge."));
    }
    let target = row.target.clone();
    let today = crate::today_utc();
    {
        let lock = hub.repos.lock(&target);
        let head = hub
            .repos
            .fetch(&lock, &row.fork, MAIN, &fork_ref(&row.fork))?;
        let repo = hub.repos.open_scratch(&target)?;
        let (Some(head), Some(main)) = (head, main_head(&repo)) else {
            return Err(not_mergeable("There is nothing to merge."));
        };
        if git::contains(&repo, main, head) {
            return Err(not_mergeable(
                "The fork has nothing that isn't already in the skill.",
            ));
        }
        let Merge::Clean(merged) = git::merge_skill(&repo, main, head)? else {
            return Err(not_mergeable(
                "The fork no longer merges cleanly. Its owner needs to sync it and resolve the conflict.",
            ));
        };
        let main_version = git::version_of(&git::skill_at(&repo, main)?.unwrap_or_default());
        let base = repo
            .merge_base(main, head)
            .map(|o| o.to_string())
            .unwrap_or_default();
        let message = format!("Merge !{id} from {}: {}", row.fork, row.title);

        let mut store = hub.store();
        match store.mr(id)? {
            Some(r) if r.state == "open" => {}
            Some(r) => {
                return Err(not_mergeable(format!(
                    "This merge request is already {}.",
                    r.state
                )));
            }
            None => return Err(ApiError::not_found("Merge request")),
        }
        let (oid, version) = if req.publish {
            if store.publishes_today(&s.key, &today)? >= hub.cfg.publishes_per_day {
                return Err(rate_limited(hub.cfg.publishes_per_day));
            }
            let version = match req.version.as_deref().map(str::trim) {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => {
                    let mut all: Vec<String> = store
                        .versions_of(&target)?
                        .into_iter()
                        .map(|v| v.version)
                        .collect();
                    all.push(main_version.clone());
                    format::sort_versions_desc(&mut all);
                    format::bump_version(&all[0], req.bump.as_deref().unwrap_or("patch"))
                        .map_err(ApiError::bad_request)?
                }
            };
            if store.version(&target, &version)?.is_some() {
                return Err(ApiError::conflict(
                    "version_exists",
                    format!("Version {version} of '{target}' is already published."),
                ));
            }
            let text = git::stamp(&merged, &target, &version);
            let doc = format::parse_skill(&text)
                .map_err(|e| ApiError::bad_request(format!("Merged SKILL.md: {e}.")))?;
            if let Some(g) = doc.groups.iter().find(|g| !format::valid_name(g)) {
                return Err(ApiError::bad_request(format!(
                    "Merged SKILL.md: invalid group slug '{g}'."
                )));
            }
            let findings = guard::check_skill(&doc, &NoSecrets);
            if !findings.is_empty() {
                return Err(content_findings(findings));
            }
            let oid = commit_version(
                &mut store,
                &hub.repos,
                &lock,
                &s.key,
                &doc,
                &text,
                Some(head),
                &message,
                Some(&today),
            )?;
            (oid, Some(version))
        } else {
            let text = git::stamp(&merged, &target, &main_version);
            let handle = store.handle(&s.key)?;
            let oid = hub
                .repos
                .commit(
                    &lock,
                    &text,
                    Some(head),
                    &Author {
                        handle: &handle,
                        at: None,
                    },
                    &message,
                    None,
                )?
                .to_string();
            store.record_author(&oid, &s.key)?;
            (oid, None)
        };
        store.mark_merged(id, &base, &head.to_string(), &oid, version.as_deref())?;
        store.audit(
            "mr_merge",
            Some(&s.key),
            &format!(
                "#{id} {} → {target} {}",
                row.fork,
                version.as_deref().unwrap_or("")
            ),
        );
    }
    hub.invalidate_index();
    let view = mr_view(&hub, mr_row(&hub, id)?, true)?;
    Ok(Json(view).into_response())
}

async fn close(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<i64>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let s = signed(&hub, &method, &uri, &headers, body).await?;
    let row = mr_row(&hub, id)?;
    if s.key != row.target_owner_key && s.key != row.author_key {
        return Err(ApiError::not_owner(
            "Only the skill's owner and the merge request's author can close it.",
        ));
    }
    {
        let store = hub.store();
        if row.state != "open" {
            return Err(ApiError::bad_request(format!(
                "This merge request is already {}.",
                row.state
            )));
        }
        store.close_mr(id)?;
        store.audit("mr_close", Some(&s.key), &format!("#{id}"));
    }
    let view = mr_view(&hub, mr_row(&hub, id)?, true)?;
    Ok(Json(view).into_response())
}
