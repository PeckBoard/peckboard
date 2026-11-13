//! Skills: reusable, versioned agent instructions (`SKILL.md`), shared
//! through three scopes — `local` (DB only), `team` (a private git repo
//! source) and `public` (an `index.json` URL source, seeded as source
//! `public`; `PECKBOARD_SKILLS_INDEX_URL` overrides its URL).
//!
//! - [`format`]: `SKILL.md` / memory frontmatter, names, semver bumps.
//! - [`guard`]: the proprietary-content check on anything leaving the host.
//! - [`sources`]: public index fetch, team repo clone / sync / commit+push.
//! - [`session`]: per-session resolution (direct + via groups, at pinned
//!   versions) and how a spawn sees the result.
//!
//! This module is the one API the REST routes (`routes::skills`) and the MCP
//! tools (`mcp_server::handlers::skills`) share, so both doors apply the
//! same validation, ownership and guard rules. Versions are immutable: an
//! edit stores a new, bumped version; a remote skill's versions are cached
//! as they are fetched.

pub mod format;
pub mod guard;
pub mod session;
pub mod sources;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::db::models::{
    SkillGroupMemberRow, SkillGroupRow, SkillRow, SkillSourceRow, SkillSubscriptionRow,
    SkillVersionRow,
};
use format::SkillDoc;
use guard::Finding;

const MAX_BODY_CHARS: usize = 200_000;
const MAX_DESCRIPTION_CHARS: usize = 1_000;
const MAX_TAGS: usize = 32;

// ── Errors ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SkillError {
    NotFound(String),
    BadRequest(String),
    Forbidden(String),
    Conflict(String),
    /// The guard refused the text (HTTP 422 `proprietary_content`).
    Proprietary(Vec<Finding>),
    /// A source could not be reached / read.
    Upstream(String),
    Internal(anyhow::Error),
}

pub type SkillResult<T> = Result<T, SkillError>;

/// Marker errors raised inside closures that only return `anyhow`, mapped
/// back to their HTTP meaning by `From<anyhow::Error>`.
#[derive(Debug)]
pub(crate) struct ConflictErr(pub String);
#[derive(Debug)]
pub(crate) struct MissingErr(pub String);
impl std::fmt::Display for ConflictErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::fmt::Display for MissingErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConflictErr {}
impl std::error::Error for MissingErr {}

impl From<anyhow::Error> for SkillError {
    fn from(e: anyhow::Error) -> Self {
        if let Some(c) = e.downcast_ref::<ConflictErr>() {
            return SkillError::Conflict(c.0.clone());
        }
        if let Some(m) = e.downcast_ref::<MissingErr>() {
            return SkillError::NotFound(m.0.clone());
        }
        SkillError::Internal(e)
    }
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::NotFound(m)
            | SkillError::BadRequest(m)
            | SkillError::Forbidden(m)
            | SkillError::Conflict(m)
            | SkillError::Upstream(m) => f.write_str(m),
            SkillError::Proprietary(findings) => {
                let parts: Vec<String> = findings
                    .iter()
                    .map(|x| format!("line {}: {} ({})", x.line, x.kind, x.excerpt))
                    .collect();
                write!(
                    f,
                    "proprietary_content: refused — remove these before sharing: {}",
                    parts.join("; ")
                )
            }
            SkillError::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl SkillError {
    pub fn status(&self) -> u16 {
        match self {
            SkillError::NotFound(_) => 404,
            SkillError::BadRequest(_) => 400,
            SkillError::Forbidden(_) => 403,
            SkillError::Conflict(_) => 409,
            SkillError::Proprietary(_) => 422,
            SkillError::Upstream(_) => 502,
            SkillError::Internal(_) => 500,
        }
    }

    /// The JSON error body (`{error}`; the guard's
    /// `{error:"proprietary_content", findings}`).
    pub fn body(&self) -> serde_json::Value {
        match self {
            SkillError::Proprietary(findings) => serde_json::json!({
                "error": "proprietary_content",
                "findings": findings,
            }),
            other => serde_json::json!({ "error": other.to_string() }),
        }
    }
}

fn bad(msg: impl Into<String>) -> SkillError {
    SkillError::BadRequest(msg.into())
}

fn missing(msg: impl Into<String>) -> SkillError {
    SkillError::NotFound(msg.into())
}

// ── Who is acting ───────────────────────────────────────────────────────

/// The caller of a mutation: a logged-in user (REST) or a session's owner
/// (MCP). Shared state (sources, groups) is admin-only; a skill may be
/// changed by an admin or the user who created / subscribed it.
#[derive(Debug, Clone)]
pub struct Actor {
    pub user_id: Option<String>,
    pub is_admin: bool,
}

impl Actor {
    pub fn can_edit(&self, created_by: Option<&str>) -> bool {
        self.is_admin || (self.user_id.is_some() && self.user_id.as_deref() == created_by)
    }

    fn require_admin(&self, what: &str) -> SkillResult<()> {
        if self.is_admin {
            Ok(())
        } else {
            Err(SkillError::Forbidden(format!("{what} is admin only")))
        }
    }

    fn require_editor(&self, skill: &SkillRow) -> SkillResult<()> {
        if self.can_edit(skill.created_by.as_deref()) {
            Ok(())
        } else {
            Err(SkillError::Forbidden(format!(
                "only an admin or the user who added '{}' may change it",
                skill.name
            )))
        }
    }

    /// The owner of `session_id` (an unowned legacy session is not admin).
    pub async fn for_session(db: &Db, session_id: &str) -> anyhow::Result<Self> {
        let user_id = db.get_session(session_id).await?.and_then(|s| s.user_id);
        let is_admin = match &user_id {
            Some(uid) => db.get_user(uid).await?.is_some_and(|u| u.role == "admin"),
            None => false,
        };
        Ok(Actor { user_id, is_admin })
    }
}

// ── Wire shapes ─────────────────────────────────────────────────────────

/// The contract's skill object. `body` is omitted in list-style responses
/// that are "minus body"; `available_versions` / `installed` only appear on
/// catalog (search) items, where `id` is null for a not-installed skill.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SkillView {
    pub id: Option<String>,
    pub scope: String,
    pub source_id: Option<String>,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub versions: Vec<String>,
    pub subscribed: bool,
    pub pinned_version: Option<String>,
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_versions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GroupView {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub scope: String,
    pub source_id: Option<String>,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SourceView {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub url: String,
    pub branch: String,
    pub secret_ref: Option<String>,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
}

impl From<&SkillSourceRow> for SourceView {
    fn from(s: &SkillSourceRow) -> Self {
        SourceView {
            id: s.id.clone(),
            kind: s.kind.clone(),
            name: s.name.clone(),
            url: if s.kind == "public" {
                sources::effective_url(s)
            } else {
                s.url.clone()
            },
            branch: s.branch.clone(),
            secret_ref: s.secret_ref.clone(),
            last_sync_at: s.last_sync_at.clone(),
            last_error: s.last_error.clone(),
        }
    }
}

fn parse_tags(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

fn tags_json(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".into())
}

/// Everything a view needs besides the skill row, loaded once per request.
struct Catalog {
    groups: Vec<SkillGroupRow>,
    members: Vec<SkillGroupMemberRow>,
    subs: HashMap<String, SkillSubscriptionRow>,
    versions: HashMap<String, Vec<String>>,
}

impl Catalog {
    async fn load(db: &Db) -> anyhow::Result<Self> {
        let mut versions: HashMap<String, Vec<String>> = HashMap::new();
        for (sid, v) in db.list_all_skill_version_labels().await? {
            versions.entry(sid).or_default().push(v);
        }
        for v in versions.values_mut() {
            format::sort_versions_desc(v);
        }
        Ok(Catalog {
            groups: db.list_skill_groups().await?,
            members: db.list_skill_group_members().await?,
            subs: db
                .list_skill_subscriptions()
                .await?
                .into_iter()
                .map(|s| (s.skill_id.clone(), s))
                .collect(),
            versions,
        })
    }

    fn members_of(&self, group_id: &str) -> Vec<String> {
        self.members
            .iter()
            .filter(|m| m.group_id == group_id)
            .map(|m| m.skill_name.clone())
            .collect()
    }

    /// Slugs of the groups that list `skill` — its own source's groups plus
    /// local ones.
    fn groups_of(&self, skill: &SkillRow) -> Vec<String> {
        let mut out: Vec<String> = self
            .groups
            .iter()
            .filter(|g| g.source_id.is_none() || g.source_id == skill.source_id)
            .filter(|g| {
                self.members
                    .iter()
                    .any(|m| m.group_id == g.id && m.skill_name == skill.name)
            })
            .map(|g| g.slug.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn pinned(&self, skill_id: &str) -> Option<String> {
        self.subs
            .get(skill_id)
            .and_then(|s| s.pinned_version.clone())
    }

    fn view(&self, skill: &SkillRow, body: Option<String>) -> SkillView {
        SkillView {
            id: Some(skill.id.clone()),
            scope: skill.scope.clone(),
            source_id: skill.source_id.clone(),
            name: skill.name.clone(),
            description: skill.description.clone(),
            tags: parse_tags(&skill.tags),
            groups: self.groups_of(skill),
            version: skill.version.clone(),
            body,
            versions: self.versions.get(&skill.id).cloned().unwrap_or_default(),
            subscribed: self.subs.contains_key(&skill.id),
            pinned_version: self.pinned(&skill.id),
            updated_at: Some(skill.updated_at.clone()),
            available_versions: None,
            installed: None,
        }
    }

    fn group_view(&self, g: &SkillGroupRow) -> GroupView {
        GroupView {
            id: g.id.clone(),
            slug: g.slug.clone(),
            name: g.name.clone(),
            description: g.description.clone(),
            scope: g.scope.clone(),
            source_id: g.source_id.clone(),
            skills: self.members_of(&g.id),
        }
    }
}

/// The version a skill is used at: its subscription pin, else the newest.
pub(crate) fn effective_version(skill: &SkillRow, sub: Option<&SkillSubscriptionRow>) -> String {
    sub.and_then(|s| s.pinned_version.clone())
        .unwrap_or_else(|| skill.version.clone())
}

fn version_row(skill_id: &str, version: &str, body: &str) -> SkillVersionRow {
    SkillVersionRow {
        id: uuid::Uuid::new_v4().to_string(),
        skill_id: skill_id.to_string(),
        version: version.to_string(),
        body: body.to_string(),
        sha256: crate::plugin::registry::sha256_hex(body.as_bytes()),
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn validate_meta(description: &str, tags: &[String], body: Option<&str>) -> SkillResult<()> {
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(bad("description too long"));
    }
    if tags.len() > MAX_TAGS || tags.iter().any(|t| t.trim().is_empty() || t.len() > 64) {
        return Err(bad("tags must be 1..=64 chars each, at most 32"));
    }
    if let Some(b) = body {
        if b.trim().is_empty() {
            return Err(bad("body must not be empty"));
        }
        if b.chars().count() > MAX_BODY_CHARS {
            return Err(bad("body too long"));
        }
    }
    Ok(())
}

fn validate_slugs(groups: &[String]) -> SkillResult<()> {
    match groups.iter().find(|g| !format::valid_name(g)) {
        Some(g) => Err(bad(format!(
            "invalid group slug '{g}' — use ^[a-z0-9][a-z0-9-]{{0,63}}$"
        ))),
        None => Ok(()),
    }
}

fn matches_q(q: &str, name: &str, description: &str, tags: &[String]) -> bool {
    if q.is_empty() {
        return true;
    }
    let q = q.to_lowercase();
    name.to_lowercase().contains(&q)
        || description.to_lowercase().contains(&q)
        || tags.iter().any(|t| t.to_lowercase().contains(&q))
}

// ── Skills ──────────────────────────────────────────────────────────────

/// `GET /api/skills` — installed skills (local + subscribed), each with the
/// body of the version it is used at.
pub async fn list(
    db: &Db,
    q: Option<&str>,
    scope: Option<&str>,
    group: Option<&str>,
) -> SkillResult<Vec<SkillView>> {
    let cat = Catalog::load(db).await?;
    let q = q.unwrap_or("").trim();
    let mut out = Vec::new();
    for s in db.list_skills().await? {
        if scope.is_some_and(|sc| !sc.is_empty() && sc != s.scope) {
            continue;
        }
        let tags = parse_tags(&s.tags);
        if !matches_q(q, &s.name, &s.description, &tags) {
            continue;
        }
        if let Some(g) = group.filter(|g| !g.is_empty()) {
            let in_group = cat
                .groups
                .iter()
                .filter(|row| row.slug == g || row.id == g)
                .any(|row| cat.members_of(&row.id).contains(&s.name));
            if !in_group {
                continue;
            }
        }
        let ver = effective_version(&s, cat.subs.get(&s.id));
        let body = db.get_skill_version(&s.id, &ver).await?.map(|v| v.body);
        out.push(cat.view(&s, body));
    }
    Ok(out)
}

pub async fn get(db: &Db, id: &str) -> SkillResult<SkillView> {
    let s = db
        .get_skill(id)
        .await?
        .ok_or_else(|| missing(format!("skill not found: {id}")))?;
    let cat = Catalog::load(db).await?;
    let ver = effective_version(&s, cat.subs.get(&s.id));
    let body = db.get_skill_version(&s.id, &ver).await?.map(|v| v.body);
    Ok(cat.view(&s, body))
}

/// Resolve an MCP-style reference: an id, else a name (local first).
pub async fn find_ref(db: &Db, id_or_name: &str) -> SkillResult<SkillRow> {
    if let Some(s) = db.get_skill(id_or_name).await? {
        return Ok(s);
    }
    let mut named: Vec<SkillRow> = db
        .list_skills()
        .await?
        .into_iter()
        .filter(|s| s.name == id_or_name)
        .collect();
    named.sort_by_key(|s| s.scope != "local");
    named
        .into_iter()
        .next()
        .ok_or_else(|| missing(format!("skill not found: {id_or_name}")))
}

/// `GET /api/skills/{id}/versions/{version}`.
pub async fn get_version(db: &Db, id: &str, version: &str) -> SkillResult<serde_json::Value> {
    let s = db
        .get_skill(id)
        .await?
        .ok_or_else(|| missing(format!("skill not found: {id}")))?;
    let v = db
        .get_skill_version(id, version)
        .await?
        .ok_or_else(|| missing(format!("{}@{version} is not stored", s.name)))?;
    Ok(serde_json::json!({
        "id": s.id,
        "name": s.name,
        "version": v.version,
        "body": v.body,
        "sha256": v.sha256,
        "created_at": v.created_at,
    }))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateSkill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    pub body: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Put local skill `name` into exactly the local groups `slugs`, creating
/// missing ones (slug as display name).
async fn set_local_groups(db: &Db, name: &str, slugs: &[String]) -> SkillResult<()> {
    db.remove_skill_from_local_groups(name).await?;
    for slug in slugs {
        let gid = match db.find_skill_group(None, slug).await? {
            Some(g) => g.id,
            None => {
                let now = chrono::Utc::now().to_rfc3339();
                let g = SkillGroupRow {
                    id: uuid::Uuid::new_v4().to_string(),
                    slug: slug.clone(),
                    name: slug.clone(),
                    description: String::new(),
                    scope: "local".into(),
                    source_id: None,
                    created_at: now.clone(),
                    updated_at: now,
                };
                let id = g.id.clone();
                db.save_skill_group(g, Vec::new()).await?;
                id
            }
        };
        db.add_skill_group_member(&gid, name).await?;
    }
    Ok(())
}

/// `POST /api/skills` — a new local skill (default version 1.0.0).
pub async fn create(db: &Db, actor: &Actor, input: CreateSkill) -> SkillResult<SkillView> {
    let name = input.name.trim().to_string();
    if !format::valid_name(&name) {
        return Err(bad(format!(
            "invalid skill name '{name}' — use ^[a-z0-9][a-z0-9-]{{0,63}}$"
        )));
    }
    let version = input
        .version
        .clone()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "1.0.0".into());
    let version = semver::Version::parse(version.trim())
        .map_err(|_| bad(format!("version '{version}' is not semver")))?
        .to_string();
    let description = input.description.trim().to_string();
    validate_meta(&description, &input.tags, Some(&input.body))?;
    validate_slugs(&input.groups)?;
    if db.find_skill(None, &name).await?.is_some() {
        return Err(SkillError::Conflict(format!(
            "a local skill named '{name}' already exists"
        )));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let row = SkillRow {
        id: uuid::Uuid::new_v4().to_string(),
        scope: "local".into(),
        source_id: None,
        name: name.clone(),
        description,
        tags: tags_json(&input.tags),
        version: version.clone(),
        created_by: actor.user_id.clone(),
        created_at: now.clone(),
        updated_at: now,
    };
    let id = row.id.clone();
    db.insert_skill_with_version(row, version_row(&id, &version, &input.body))
        .await?;
    set_local_groups(db, &name, &input.groups).await?;
    get(db, &id).await
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateSkill {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub groups: Option<Vec<String>>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub bump: Option<String>,
}

/// `PUT /api/skills/{id}` — store a new immutable version (bump defaults to
/// `patch`). A groups-only change re-tags without a new version.
pub async fn update(
    db: &Db,
    actor: &Actor,
    id: &str,
    input: UpdateSkill,
) -> SkillResult<SkillView> {
    let s = db
        .get_skill(id)
        .await?
        .ok_or_else(|| missing(format!("skill not found: {id}")))?;
    if s.scope != "local" {
        return Err(bad(format!(
            "'{}' comes from a {} source and is read-only here — change it at the source",
            s.name, s.scope
        )));
    }
    actor.require_editor(&s)?;
    if let Some(groups) = &input.groups {
        validate_slugs(groups)?;
    }
    let content_change = input.description.is_some()
        || input.tags.is_some()
        || input.body.is_some()
        || input.bump.is_some();
    if content_change {
        let head = db
            .get_skill_version(&s.id, &s.version)
            .await?
            .ok_or_else(|| SkillError::Internal(anyhow::anyhow!("head version missing")))?;
        let description = input
            .description
            .map(|d| d.trim().to_string())
            .unwrap_or_else(|| s.description.clone());
        let tags = input.tags.unwrap_or_else(|| parse_tags(&s.tags));
        let body = input.body.unwrap_or(head.body);
        validate_meta(&description, &tags, Some(&body))?;
        let next = format::bump_version(&s.version, input.bump.as_deref().unwrap_or("patch"))
            .map_err(bad)?;
        db.add_skill_version(
            version_row(&s.id, &next, &body),
            true,
            description,
            tags_json(&tags),
        )
        .await
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                SkillError::Conflict(format!("{}@{next} already exists", s.name))
            } else {
                e.into()
            }
        })?;
    }
    if let Some(groups) = &input.groups {
        set_local_groups(db, &s.name, groups).await?;
    }
    get(db, id).await
}

/// `DELETE /api/skills/{id}` — removes the skill, its versions and every
/// session assignment of it.
pub async fn delete(db: &Db, actor: &Actor, id: &str) -> SkillResult<()> {
    let s = db
        .get_skill(id)
        .await?
        .ok_or_else(|| missing(format!("skill not found: {id}")))?;
    actor.require_editor(&s)?;
    db.delete_skill(id).await?;
    Ok(())
}

// ── Remote: catalog, subscribe, sync ────────────────────────────────────

async fn source_or_404(db: &Db, id: &str) -> SkillResult<SkillSourceRow> {
    db.get_skill_source(id)
        .await?
        .ok_or_else(|| missing(format!("skill source not found: {id}")))
}

/// A source's catalog. Team browse reads the existing clone (cloning it on
/// first use); `sync_source` is what pulls.
async fn remote_index(
    db: &Db,
    data_dir: &Path,
    source: &SkillSourceRow,
) -> SkillResult<sources::RepoIndex> {
    match source.kind.as_str() {
        "public" => sources::fetch_public_index(source)
            .await
            .map_err(|e| SkillError::Upstream(format!("{}: {e}", source.name))),
        _ => {
            let repo = sources::repo_dir(data_dir, &source.id);
            if repo.join(".git").exists() {
                Ok(sources::scan_repo(&repo))
            } else {
                sources::sync_team(db, data_dir, source)
                    .await
                    .map_err(|e| SkillError::Upstream(format!("{}: {e}", source.name)))
            }
        }
    }
}

async fn fetch_remote(
    db: &Db,
    data_dir: &Path,
    source: &SkillSourceRow,
    name: &str,
    version: &str,
) -> SkillResult<SkillDoc> {
    let r = match source.kind.as_str() {
        "public" => sources::fetch_public_skill(source, name, version).await,
        _ => {
            let repo = sources::repo_dir(data_dir, &source.id);
            if !repo.join(".git").exists() {
                sources::sync_team(db, data_dir, source)
                    .await
                    .map_err(|e| SkillError::Upstream(e.to_string()))?;
            }
            sources::read_skill(&repo, name, version)
        }
    };
    r.map_err(|e| SkillError::Upstream(format!("{name}@{version}: {e}")))
}

/// Store a fetched version unless it is already cached.
async fn cache_version(db: &Db, skill_id: &str, doc: &SkillDoc) -> SkillResult<()> {
    if db
        .get_skill_version(skill_id, &doc.version)
        .await?
        .is_none()
    {
        db.add_skill_version(
            version_row(skill_id, &doc.version, &doc.body),
            false,
            String::new(),
            String::new(),
        )
        .await?;
    }
    Ok(())
}

/// Mirror a source's groups into the DB (id kept per slug).
async fn upsert_remote_groups(
    db: &Db,
    source: &SkillSourceRow,
    index: &sources::RepoIndex,
) -> SkillResult<()> {
    for g in &index.groups {
        if !format::valid_name(&g.slug) {
            continue;
        }
        let existing = db.find_skill_group(Some(&source.id), &g.slug).await?;
        let now = chrono::Utc::now().to_rfc3339();
        let row = SkillGroupRow {
            id: existing
                .as_ref()
                .map(|e| e.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            slug: g.slug.clone(),
            name: if g.name.is_empty() {
                g.slug.clone()
            } else {
                g.name.clone()
            },
            description: g.description.clone(),
            scope: source.kind.clone(),
            source_id: Some(source.id.clone()),
            created_at: existing
                .map(|e| e.created_at)
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        db.save_skill_group(row, g.skills.clone()).await?;
    }
    Ok(())
}

/// Move an installed remote skill's head to `latest` (fetching it when not
/// cached) and make sure its pinned version is cached.
async fn refresh_installed(
    db: &Db,
    data_dir: &Path,
    source: &SkillSourceRow,
    skill: &SkillRow,
    entry: &sources::IndexSkill,
) -> SkillResult<()> {
    if let Some(latest) = entry.latest_version()
        && latest != skill.version
    {
        let doc = fetch_remote(db, data_dir, source, &skill.name, &latest).await?;
        if db.get_skill_version(&skill.id, &latest).await?.is_some() {
            db.update_skill_head(&skill.id, &latest, &doc.description, &tags_json(&doc.tags))
                .await?;
        } else {
            db.add_skill_version(
                version_row(&skill.id, &latest, &doc.body),
                true,
                doc.description.clone(),
                tags_json(&doc.tags),
            )
            .await?;
        }
    }
    if let Some(pin) = db
        .get_skill_subscription(&skill.id)
        .await?
        .and_then(|s| s.pinned_version)
        && db.get_skill_version(&skill.id, &pin).await?.is_none()
    {
        let doc = fetch_remote(db, data_dir, source, &skill.name, &pin).await?;
        cache_version(db, &skill.id, &doc).await?;
    }
    Ok(())
}

/// `POST /api/skills/subscribe` — install a remote skill, following latest
/// (`version` None) or pinned.
pub async fn subscribe(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    source_id: &str,
    name: &str,
    version: Option<String>,
) -> SkillResult<SkillView> {
    let source = source_or_404(db, source_id).await?;
    let index = remote_index(db, data_dir, &source).await?;
    let entry = index
        .skills
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| missing(format!("'{name}' is not in {}", source.name)))?;
    let latest = entry
        .latest_version()
        .ok_or_else(|| SkillError::Upstream(format!("'{name}' lists no versions")))?;
    let version = version.filter(|v| !v.trim().is_empty());
    if let Some(v) = &version
        && v != &latest
        && !entry.versions.contains(v)
    {
        return Err(missing(format!(
            "{name}@{v} is not published in {}",
            source.name
        )));
    }
    let skill_id = match db.find_skill(Some(&source.id), name).await? {
        Some(existing) => {
            refresh_installed(db, data_dir, &source, &existing, entry).await?;
            existing.id
        }
        None => {
            let doc = fetch_remote(db, data_dir, &source, name, &latest).await?;
            let now = chrono::Utc::now().to_rfc3339();
            let row = SkillRow {
                id: uuid::Uuid::new_v4().to_string(),
                scope: source.kind.clone(),
                source_id: Some(source.id.clone()),
                name: name.to_string(),
                description: doc.description.clone(),
                tags: tags_json(&doc.tags),
                version: latest.clone(),
                created_by: actor.user_id.clone(),
                created_at: now.clone(),
                updated_at: now,
            };
            let id = row.id.clone();
            db.insert_skill_with_version(row, version_row(&id, &latest, &doc.body))
                .await?;
            id
        }
    };
    if let Some(v) = &version
        && db.get_skill_version(&skill_id, v).await?.is_none()
    {
        let doc = fetch_remote(db, data_dir, &source, name, v).await?;
        cache_version(db, &skill_id, &doc).await?;
    }
    db.upsert_skill_subscription(&skill_id, version).await?;
    upsert_remote_groups(db, &source, &index).await?;
    get(db, &skill_id).await
}

/// `PUT /api/skills/{id}/subscription` — re-pin (`None` = latest).
pub async fn set_subscription(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    id: &str,
    version: Option<String>,
) -> SkillResult<SkillView> {
    let s = db
        .get_skill(id)
        .await?
        .ok_or_else(|| missing(format!("skill not found: {id}")))?;
    let Some(source_id) = s.source_id.clone() else {
        return Err(bad(
            "local skills are not subscriptions — edit them instead",
        ));
    };
    actor.require_editor(&s)?;
    let version = version.filter(|v| !v.trim().is_empty());
    if let Some(v) = &version
        && db.get_skill_version(&s.id, v).await?.is_none()
    {
        let source = source_or_404(db, &source_id).await?;
        let doc = fetch_remote(db, data_dir, &source, &s.name, v).await?;
        cache_version(db, &s.id, &doc).await?;
    }
    db.upsert_skill_subscription(&s.id, version).await?;
    get(db, id).await
}

/// `DELETE /api/skills/{id}/subscription` — uninstall a remote skill.
pub async fn unsubscribe(db: &Db, actor: &Actor, id: &str) -> SkillResult<()> {
    let s = db
        .get_skill(id)
        .await?
        .ok_or_else(|| missing(format!("skill not found: {id}")))?;
    if s.source_id.is_none() {
        return Err(bad(
            "local skills are not subscriptions — delete them instead",
        ));
    }
    actor.require_editor(&s)?;
    db.delete_skill(id).await?;
    Ok(())
}

/// `GET /api/skills/search` — the browsable catalog: local skills plus every
/// source's index (or just `source_id`'s). A source that can't be read is
/// reported in `errors`, not fatal.
pub async fn search(
    db: &Db,
    data_dir: &Path,
    q: Option<&str>,
    source_id: Option<&str>,
) -> SkillResult<serde_json::Value> {
    let q = q.unwrap_or("").trim();
    let cat = Catalog::load(db).await?;
    let installed = db.list_skills().await?;
    let mut items: Vec<SkillView> = Vec::new();
    let mut groups: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<serde_json::Value> = Vec::new();
    let source_filter = source_id.filter(|s| !s.is_empty());
    if source_filter.is_none() || source_filter == Some("local") {
        for s in installed.iter().filter(|s| s.scope == "local") {
            if matches_q(q, &s.name, &s.description, &parse_tags(&s.tags)) {
                let mut v = cat.view(s, None);
                v.available_versions = Some(v.versions.clone());
                v.installed = Some(true);
                items.push(v);
            }
        }
    }
    for source in db.list_skill_sources().await? {
        if source_filter.is_some_and(|f| f != source.id) {
            continue;
        }
        let index = match remote_index(db, data_dir, &source).await {
            Ok(i) => i,
            Err(e) => {
                errors.push(serde_json::json!({ "source_id": source.id, "error": e.to_string() }));
                continue;
            }
        };
        for entry in &index.skills {
            if !format::valid_name(&entry.name)
                || !matches_q(q, &entry.name, &entry.description, &entry.tags)
            {
                continue;
            }
            let local = installed
                .iter()
                .find(|s| s.source_id.as_deref() == Some(&source.id) && s.name == entry.name);
            let item = match local {
                Some(s) => {
                    let mut v = cat.view(s, None);
                    v.available_versions = Some(entry.versions.clone());
                    v.installed = Some(true);
                    v
                }
                None => SkillView {
                    id: None,
                    scope: source.kind.clone(),
                    source_id: Some(source.id.clone()),
                    name: entry.name.clone(),
                    description: entry.description.clone(),
                    tags: entry.tags.clone(),
                    groups: entry.groups.clone(),
                    version: entry.latest_version().unwrap_or_default(),
                    body: None,
                    versions: Vec::new(),
                    subscribed: false,
                    pinned_version: None,
                    updated_at: None,
                    available_versions: Some(entry.versions.clone()),
                    installed: Some(false),
                },
            };
            items.push(item);
        }
        for g in &index.groups {
            groups.push(serde_json::json!({
                "slug": g.slug, "name": g.name, "description": g.description,
                "skills": g.skills, "source_id": source.id,
            }));
        }
    }
    Ok(serde_json::json!({ "items": items, "groups": groups, "errors": errors }))
}

// ── Publish ─────────────────────────────────────────────────────────────

/// `POST /api/skills/{id}/publish` — share the skill's newest version to a
/// team source (commit + push) or prepare it for the public repo (written to
/// `<data_dir>/skills/public-outbox/` with PR instructions). The guard runs
/// first either way.
pub async fn publish(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    id: &str,
    target_source_id: &str,
) -> SkillResult<serde_json::Value> {
    let s = db
        .get_skill(id)
        .await?
        .ok_or_else(|| missing(format!("skill not found: {id}")))?;
    actor.require_editor(&s)?;
    let target = source_or_404(db, target_source_id).await?;
    let head = db
        .get_skill_version(&s.id, &s.version)
        .await?
        .ok_or_else(|| SkillError::Internal(anyhow::anyhow!("head version missing")))?;
    let cat = Catalog::load(db).await?;
    let doc = SkillDoc {
        name: s.name.clone(),
        description: s.description.clone(),
        version: s.version.clone(),
        tags: parse_tags(&s.tags),
        groups: cat.groups_of(&s),
        body: head.body.clone(),
    };
    // Body lines are reported as the user wrote them; frontmatter fields
    // (name, description, tags, groups) are line 0.
    let meta = format!(
        "{}\n{}\n{}\n{}",
        doc.name,
        doc.description,
        doc.tags.join(" "),
        doc.groups.join(" ")
    );
    let findings = guard::check_document(db, &meta, &doc.body).await;
    if !findings.is_empty() {
        return Err(SkillError::Proprietary(findings));
    }
    let repo_path = format!("skills/{}/{}/SKILL.md", doc.name, doc.version);
    match target.kind.as_str() {
        "team" => {
            // The skill's local groups travel with it (merged into the repo's
            // group files).
            let local_groups: Vec<GroupView> = cat
                .groups
                .iter()
                .filter(|g| g.source_id.is_none() && doc.groups.contains(&g.slug))
                .map(|g| cat.group_view(g))
                .collect();
            let doc2 = doc.clone();
            let committed = sources::team_write(db, data_dir, &target, move |repo| {
                sources::write_skill(repo, &doc2)
                    .map_err(|e| anyhow::Error::new(ConflictErr(e.to_string())))?;
                for g in &local_groups {
                    let path = repo.join("groups").join(format!("{}.json", g.slug));
                    let mut ig: sources::IndexGroup = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|t| serde_json::from_str(&t).ok())
                        .unwrap_or_else(|| sources::IndexGroup {
                            slug: g.slug.clone(),
                            name: g.name.clone(),
                            description: g.description.clone(),
                            skills: Vec::new(),
                        });
                    if !ig.skills.contains(&doc2.name) {
                        ig.skills.push(doc2.name.clone());
                        ig.skills.sort();
                    }
                    sources::write_group(repo, &ig)?;
                }
                Ok(Some(format!(
                    "Publish skill {} {}",
                    doc2.name, doc2.version
                )))
            })
            .await
            .map_err(|e| match SkillError::from(e) {
                SkillError::Internal(e) => SkillError::Upstream(e.to_string()),
                other => other,
            })?;
            let _ = db
                .set_skill_source_sync(&target.id, Some(chrono::Utc::now().to_rfc3339()), None)
                .await;
            Ok(serde_json::json!({
                "pr_url": null,
                "path": repo_path,
                "source_id": target.id,
                "committed": committed.is_some(),
            }))
        }
        _ => {
            let outbox = data_dir.join("skills").join("public-outbox");
            let local = sources::write_skill(&outbox, &doc)
                .map_err(|e| SkillError::Conflict(e.to_string()))?;
            sources::write_index(&outbox)?;
            Ok(serde_json::json!({
                "pr_url": null,
                "path": repo_path,
                "local_path": local.to_string_lossy(),
                "instructions": format!(
                    "Passed the proprietary-content check. To publish, open a pull request \
                     against github.com/PeckBoard/skills adding `{repo_path}` (the file is \
                     ready at the local_path on the Peckboard server; the repo's CI \
                     regenerates index.json)."
                ),
            }))
        }
    }
}

// ── Groups ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GroupInput {
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub skills: Vec<String>,
}

pub async fn list_groups(db: &Db) -> SkillResult<Vec<GroupView>> {
    let cat = Catalog::load(db).await?;
    Ok(cat.groups.iter().map(|g| cat.group_view(g)).collect())
}

async fn group_view_by_id(db: &Db, id: &str) -> SkillResult<GroupView> {
    let cat = Catalog::load(db).await?;
    cat.groups
        .iter()
        .find(|g| g.id == id)
        .map(|g| cat.group_view(g))
        .ok_or_else(|| missing(format!("skill group not found: {id}")))
}

fn validate_group(input: &GroupInput) -> SkillResult<()> {
    if !format::valid_name(input.slug.trim()) {
        return Err(bad(format!(
            "invalid group slug '{}' — use ^[a-z0-9][a-z0-9-]{{0,63}}$",
            input.slug
        )));
    }
    if input.name.chars().count() > 200 || input.description.chars().count() > MAX_DESCRIPTION_CHARS
    {
        return Err(bad("group name or description too long"));
    }
    match input.skills.iter().find(|s| !format::valid_name(s)) {
        Some(s) => Err(bad(format!("invalid skill name '{s}' in group"))),
        None => Ok(()),
    }
}

pub async fn create_group(db: &Db, actor: &Actor, input: GroupInput) -> SkillResult<GroupView> {
    actor.require_admin("creating a skill group")?;
    validate_group(&input)?;
    let slug = input.slug.trim().to_string();
    if db.find_skill_group(None, &slug).await?.is_some() {
        return Err(SkillError::Conflict(format!(
            "a local group '{slug}' already exists"
        )));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let row = SkillGroupRow {
        id: uuid::Uuid::new_v4().to_string(),
        name: if input.name.trim().is_empty() {
            slug.clone()
        } else {
            input.name.trim().to_string()
        },
        slug,
        description: input.description.trim().to_string(),
        scope: "local".into(),
        source_id: None,
        created_at: now.clone(),
        updated_at: now,
    };
    let id = row.id.clone();
    db.save_skill_group(row, input.skills).await?;
    group_view_by_id(db, &id).await
}

pub async fn update_group(
    db: &Db,
    actor: &Actor,
    id: &str,
    input: GroupInput,
) -> SkillResult<GroupView> {
    actor.require_admin("editing a skill group")?;
    let g = db
        .get_skill_group(id)
        .await?
        .ok_or_else(|| missing(format!("skill group not found: {id}")))?;
    if g.source_id.is_some() {
        return Err(bad(
            "this group is managed by its source and is read-only here",
        ));
    }
    validate_group(&input)?;
    let slug = input.slug.trim().to_string();
    if let Some(other) = db.find_skill_group(None, &slug).await?
        && other.id != g.id
    {
        return Err(SkillError::Conflict(format!(
            "a local group '{slug}' already exists"
        )));
    }
    let row = SkillGroupRow {
        name: if input.name.trim().is_empty() {
            slug.clone()
        } else {
            input.name.trim().to_string()
        },
        slug,
        description: input.description.trim().to_string(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        ..g
    };
    db.save_skill_group(row, input.skills).await?;
    group_view_by_id(db, id).await
}

pub async fn delete_group(db: &Db, actor: &Actor, id: &str) -> SkillResult<()> {
    actor.require_admin("deleting a skill group")?;
    let g = db
        .get_skill_group(id)
        .await?
        .ok_or_else(|| missing(format!("skill group not found: {id}")))?;
    if g.source_id.is_some() {
        return Err(bad(
            "this group is managed by its source and is read-only here",
        ));
    }
    db.delete_skill_group(id).await?;
    Ok(())
}

// ── Sources ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceInput {
    pub kind: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub secret_ref: Option<String>,
}

pub async fn list_sources(db: &Db) -> SkillResult<Vec<SourceView>> {
    Ok(db
        .list_skill_sources()
        .await?
        .iter()
        .map(SourceView::from)
        .collect())
}

pub async fn create_source(db: &Db, actor: &Actor, input: SourceInput) -> SkillResult<SourceView> {
    actor.require_admin("adding a skill source")?;
    let name = input.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(bad("name must be 1..=200 characters"));
    }
    let url = input.url.trim().to_string();
    match input.kind.as_str() {
        "public" => {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(bad("a public source url must be an http(s) index.json URL"));
            }
        }
        "team" => sources::validate_git_url(&url).map_err(bad)?,
        other => return Err(bad(format!("invalid kind '{other}' — use public or team"))),
    }
    let branch = input
        .branch
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "main".into());
    if branch.starts_with('-')
        || !branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        return Err(bad(format!("invalid branch '{branch}'")));
    }
    let secret_ref = input
        .secret_ref
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(r) = &secret_ref
        && !(r.starts_with("ssh_key:") || r.starts_with("env:"))
    {
        return Err(bad("secret_ref must be ssh_key:<id> or env:<NAME>"));
    }
    if db.get_skill_source_by_name(&name).await?.is_some() {
        return Err(SkillError::Conflict(format!(
            "a skill source named '{name}' already exists"
        )));
    }
    let row = SkillSourceRow {
        id: uuid::Uuid::new_v4().to_string(),
        kind: input.kind,
        name,
        url,
        branch,
        secret_ref,
        last_sync_at: None,
        last_error: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    db.insert_skill_source(row.clone()).await?;
    Ok(SourceView::from(&row))
}

/// `DELETE /api/skill-sources/{id}` — its subscribed skills and groups go
/// with it; a team clone is removed from disk.
pub async fn delete_source(db: &Db, data_dir: &Path, actor: &Actor, id: &str) -> SkillResult<()> {
    actor.require_admin("removing a skill source")?;
    let source = source_or_404(db, id).await?;
    db.delete_skill_source(id).await?;
    if source.kind == "team" {
        let _ = std::fs::remove_dir_all(sources::repo_dir(data_dir, id));
    }
    Ok(())
}

/// `POST /api/skill-sources/{id}/sync` — pull the catalog, mirror its groups,
/// and move every subscribed skill from it to its latest version (pinned
/// ones stay pinned). The outcome is recorded on the source row.
pub async fn sync_source(
    db: &Db,
    data_dir: &Path,
    actor: &Actor,
    id: &str,
) -> SkillResult<SourceView> {
    actor.require_admin("syncing a skill source")?;
    let source = source_or_404(db, id).await?;
    let index = match source.kind.as_str() {
        "public" => sources::fetch_public_index(&source).await,
        _ => sources::sync_team(db, data_dir, &source).await,
    };
    let index = match index {
        Ok(i) => i,
        Err(e) => {
            let msg = e.to_string();
            db.set_skill_source_sync(id, None, Some(msg.clone()))
                .await?;
            return Err(SkillError::Upstream(format!("{}: {msg}", source.name)));
        }
    };
    let mut errors: Vec<String> = Vec::new();
    if let Err(e) = upsert_remote_groups(db, &source, &index).await {
        errors.push(e.to_string());
    }
    let installed: Vec<SkillRow> = db
        .list_skills()
        .await?
        .into_iter()
        .filter(|s| s.source_id.as_deref() == Some(id))
        .collect();
    for s in installed {
        if let Some(entry) = index.skills.iter().find(|e| e.name == s.name)
            && let Err(e) = refresh_installed(db, data_dir, &source, &s, entry).await
        {
            errors.push(e.to_string());
        }
    }
    let err = (!errors.is_empty()).then(|| errors.join("; "));
    db.set_skill_source_sync(id, Some(chrono::Utc::now().to_rfc3339()), err)
        .await?;
    Ok(SourceView::from(&source_or_404(db, id).await?))
}

// ── Team memory ─────────────────────────────────────────────────────────

async fn team_source(db: &Db, id: &str) -> SkillResult<SkillSourceRow> {
    let s = source_or_404(db, id).await?;
    if s.kind != "team" {
        return Err(bad("team memory lives in team sources only"));
    }
    Ok(s)
}

async fn team_repo(
    db: &Db,
    data_dir: &Path,
    source: &SkillSourceRow,
) -> SkillResult<std::path::PathBuf> {
    let repo = sources::repo_dir(data_dir, &source.id);
    if !repo.join(".git").exists() {
        sources::sync_team(db, data_dir, source)
            .await
            .map_err(|e| SkillError::Upstream(e.to_string()))?;
    }
    Ok(repo)
}

pub async fn list_memory(
    db: &Db,
    data_dir: &Path,
    source_id: &str,
) -> SkillResult<Vec<format::MemoryDoc>> {
    let source = team_source(db, source_id).await?;
    let repo = team_repo(db, data_dir, &source).await?;
    Ok(sources::list_memory(&repo))
}

pub async fn get_memory(
    db: &Db,
    data_dir: &Path,
    source_id: &str,
    slug: &str,
) -> SkillResult<format::MemoryDoc> {
    list_memory(db, data_dir, source_id)
        .await?
        .into_iter()
        .find(|m| m.slug == slug)
        .ok_or_else(|| missing(format!("team memory '{slug}' not found")))
}

/// Add (`create = true`, refusing an existing slug) or update a team-memory
/// note. Guarded; committed and pushed.
pub async fn write_memory(
    db: &Db,
    data_dir: &Path,
    source_id: &str,
    slug: &str,
    title: Option<String>,
    body: String,
    create: bool,
) -> SkillResult<format::MemoryDoc> {
    let source = team_source(db, source_id).await?;
    if !format::valid_name(slug) {
        return Err(bad(format!(
            "invalid memory slug '{slug}' — use ^[a-z0-9][a-z0-9-]{{0,63}}$"
        )));
    }
    if body.trim().is_empty() || body.chars().count() > MAX_BODY_CHARS {
        return Err(bad("body must be non-empty and at most 200000 characters"));
    }
    let title = title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if title.as_ref().is_some_and(|t| t.chars().count() > 200) {
        return Err(bad("title too long"));
    }
    // Body lines are reported as the user wrote them; the title is line 0.
    let meta = format!("{slug}\n{}", title.clone().unwrap_or_default());
    let findings = guard::check_document(db, &meta, &body).await;
    if !findings.is_empty() {
        return Err(SkillError::Proprietary(findings));
    }
    let slug_s = slug.to_string();
    let written = std::sync::Arc::new(std::sync::Mutex::new(None));
    let out = written.clone();
    sources::team_write(db, data_dir, &source, move |repo| {
        let path = sources::memory_path(repo, &slug_s)?;
        let existing = std::fs::read_to_string(&path)
            .ok()
            .map(|t| format::parse_memory(&slug_s, &t));
        if create && existing.is_some() {
            return Err(anyhow::Error::new(ConflictErr(format!(
                "team memory '{slug_s}' already exists — update it instead"
            ))));
        }
        if !create && existing.is_none() {
            return Err(anyhow::Error::new(MissingErr(format!(
                "team memory '{slug_s}' not found"
            ))));
        }
        let doc = format::MemoryDoc {
            slug: slug_s.clone(),
            title: title
                .clone()
                .or_else(|| existing.map(|e| e.title))
                .unwrap_or_else(|| slug_s.clone()),
            updated: chrono::Utc::now().to_rfc3339(),
            body: body.clone(),
        };
        std::fs::create_dir_all(path.parent().expect("memory dir"))?;
        std::fs::write(&path, format::serialize_memory(&doc))?;
        *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(doc);
        Ok(Some(format!(
            "{} team memory {slug_s}",
            if create { "Add" } else { "Update" }
        )))
    })
    .await
    .map_err(team_err)?;
    let doc = written
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .ok_or_else(|| SkillError::Internal(anyhow::anyhow!("memory not written")))?;
    Ok(doc)
}

pub async fn remove_memory(
    db: &Db,
    data_dir: &Path,
    source_id: &str,
    slug: &str,
) -> SkillResult<()> {
    let source = team_source(db, source_id).await?;
    let slug_s = slug.to_string();
    sources::team_write(db, data_dir, &source, move |repo| {
        let path = sources::memory_path(repo, &slug_s)?;
        if !path.exists() {
            return Err(anyhow::Error::new(MissingErr(format!(
                "team memory '{slug_s}' not found"
            ))));
        }
        std::fs::remove_file(&path)?;
        Ok(Some(format!("Remove team memory {slug_s}")))
    })
    .await
    .map_err(team_err)?;
    Ok(())
}

/// A team git failure is the remote's problem (502), not ours — unless it
/// is one of the marker errors.
fn team_err(e: anyhow::Error) -> SkillError {
    match SkillError::from(e) {
        SkillError::Internal(e) => SkillError::Upstream(e.to_string()),
        other => other,
    }
}

// ── Session assignments ─────────────────────────────────────────────────

/// `GET /api/sessions/{id}/skills`.
pub async fn session_skills(db: &Db, session_id: &str) -> SkillResult<serde_json::Value> {
    if db.get_session(session_id).await?.is_none() {
        return Err(missing(format!("session not found: {session_id}")));
    }
    let rows = db.list_session_skills(session_id).await?;
    let cat = Catalog::load(db).await?;
    let resolved: Vec<serde_json::Value> = session::resolve(db, session_id)
        .await?
        .into_iter()
        .map(|r| {
            let mut v = serde_json::to_value(cat.view(&r.skill, None)).unwrap_or_default();
            v["active_version"] = serde_json::Value::String(r.version);
            v["via_group"] = r
                .via_group
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null);
            v
        })
        .collect();
    Ok(serde_json::json!({
        "skills": rows.iter().filter_map(|r| r.skill_id.clone()).collect::<Vec<_>>(),
        "groups": rows.iter().filter_map(|r| r.group_id.clone()).collect::<Vec<_>>(),
        "resolved": resolved,
    }))
}

/// `PUT /api/sessions/{id}/skills` — replace the assignment. Skills and
/// groups may be given by id or (skills) name / (groups) slug.
pub async fn set_session_skills(
    db: &Db,
    session_id: &str,
    skills: Vec<String>,
    groups: Vec<String>,
) -> SkillResult<serde_json::Value> {
    if db.get_session(session_id).await?.is_none() {
        return Err(missing(format!("session not found: {session_id}")));
    }
    let mut skill_ids = Vec::new();
    for s in &skills {
        skill_ids.push(find_ref(db, s).await?.id);
    }
    let all_groups = db.list_skill_groups().await?;
    let mut group_ids = Vec::new();
    for g in &groups {
        let found = all_groups
            .iter()
            .find(|row| &row.id == g)
            .or_else(|| {
                // By slug: local first.
                let mut by_slug: Vec<&SkillGroupRow> =
                    all_groups.iter().filter(|row| &row.slug == g).collect();
                by_slug.sort_by_key(|row| row.source_id.is_some());
                by_slug.into_iter().next()
            })
            .ok_or_else(|| missing(format!("skill group not found: {g}")))?;
        group_ids.push(found.id.clone());
    }
    db.set_session_skills(session_id, skill_ids, group_ids)
        .await?;
    session_skills(db, session_id).await
}

/// The index entry for a skill name across installed skills (for the MCP
/// `skill_list` tool's compact output).
pub fn compact(view: &SkillView) -> serde_json::Value {
    serde_json::json!({
        "id": view.id, "name": view.name, "scope": view.scope,
        "source_id": view.source_id, "description": view.description,
        "version": view.version, "pinned_version": view.pinned_version,
        "groups": view.groups, "tags": view.tags,
    })
}

/// Group slugs → member names (exposed for tests and the session resolver).
pub(crate) fn group_members(
    groups: &[SkillGroupRow],
    members: &[SkillGroupMemberRow],
) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for g in groups {
        out.insert(
            g.id.clone(),
            members
                .iter()
                .filter(|m| m.group_id == g.id)
                .map(|m| m.skill_name.clone())
                .collect(),
        );
    }
    out
}
