//! SQLite (WAL) metadata. `SKILL.md` bodies live in git ([`crate::git`]);
//! a version row points at its commit (`commit_id`).
//!
//! A version is *yanked* by its owner (hidden from the index, still
//! served — installed copies keep resolving) or *removed* by an admin
//! takedown (hidden and no longer served). Neither frees the name or the
//! version number. Both are flags here, never git rewrites.
//!
//! Data dirs from before git storage kept bodies as `blobs/<sha256>`;
//! [`migrate_to_git`] commits those into repos on startup.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Context;
use peckboard_skill_format::format::{self, SkillDoc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::git::{Author, RepoLock, Repos};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS publishers (
    public_key TEXT PRIMARY KEY,
    first_seen TEXT NOT NULL,
    banned INTEGER NOT NULL DEFAULT 0,
    publish_count_day INTEGER NOT NULL DEFAULT 0,
    day TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS skills (
    name TEXT PRIMARY KEY,
    owner_key TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS versions (
    name TEXT NOT NULL REFERENCES skills(name),
    version TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    tags TEXT NOT NULL DEFAULT '[]',
    "groups" TEXT NOT NULL DEFAULT '[]',
    published_at TEXT NOT NULL,
    yanked INTEGER NOT NULL DEFAULT 0,
    removed INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (name, version)
);
CREATE TABLE IF NOT EXISTS "groups" (
    slug TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    owner_key TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    action TEXT NOT NULL,
    public_key TEXT,
    detail TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS audit_log_at ON audit_log(at);
CREATE TABLE IF NOT EXISTS commit_authors (
    commit_id TEXT PRIMARY KEY,
    public_key TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS merge_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target TEXT NOT NULL REFERENCES skills(name),
    fork TEXT NOT NULL REFERENCES skills(name),
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'open',
    author_key TEXT NOT NULL,
    target_owner_key TEXT NOT NULL,
    base_commit TEXT NOT NULL DEFAULT '',
    head_commit TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    merged_commit TEXT,
    merged_version TEXT
);
CREATE INDEX IF NOT EXISTS merge_requests_target ON merge_requests(target, state);
CREATE INDEX IF NOT EXISTS merge_requests_fork ON merge_requests(fork, state);
CREATE INDEX IF NOT EXISTS merge_requests_author ON merge_requests(author_key, state);
CREATE INDEX IF NOT EXISTS merge_requests_owner ON merge_requests(target_owner_key, state);
CREATE TABLE IF NOT EXISTS mr_comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    mr_id INTEGER NOT NULL REFERENCES merge_requests(id),
    author_key TEXT NOT NULL,
    body TEXT NOT NULL,
    at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS mr_comments_mr ON mr_comments(mr_id);
"#;

/// Columns added after the first release (ALTER TABLE can't be
/// idempotent, so each is added only when missing).
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("publishers", "handle", "TEXT"),
    ("skills", "parent", "TEXT"),
    ("versions", "commit_id", "TEXT"),
];

/// Indexes on added columns (created once the columns exist).
const LATE_INDEXES: &str = r#"
CREATE UNIQUE INDEX IF NOT EXISTS publishers_handle ON publishers(handle);
CREATE UNIQUE INDEX IF NOT EXISTS skills_fork_per_key ON skills(parent, owner_key) WHERE parent IS NOT NULL;
"#;

// ── index.json shape (what PeckBoard's `sources::RepoIndex` reads) ──────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexSkill {
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    /// Newest first, yanked and removed versions excluded.
    pub versions: Vec<String>,
    pub latest: Option<String>,
    /// How many forks the skill has.
    #[serde(default)]
    pub forks: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexGroup {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RepoIndex {
    pub schema: u32,
    pub generated_at: Option<String>,
    pub skills: Vec<IndexSkill>,
    pub groups: Vec<IndexGroup>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Publisher {
    pub public_key: String,
    pub first_seen: String,
    pub banned: bool,
    pub publish_count_day: u32,
    pub day: String,
    pub handle: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillRow {
    pub name: String,
    pub owner_key: String,
    pub created_at: String,
    /// The original, for a fork.
    pub parent: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VersionRow {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub description: String,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    pub published_at: String,
    pub yanked: bool,
    pub removed: bool,
    /// The version's commit in the skill's repo (`None` only for a
    /// pre-git row not migrated yet).
    pub commit_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupRow {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub owner_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MrRow {
    pub id: i64,
    pub target: String,
    pub fork: String,
    pub title: String,
    pub description: String,
    pub state: String,
    pub author_key: String,
    pub target_owner_key: String,
    pub base_commit: String,
    pub head_commit: String,
    pub created_at: String,
    pub updated_at: String,
    pub merged_commit: Option<String>,
    pub merged_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommentRow {
    pub id: i64,
    pub mr_id: i64,
    pub author_key: String,
    pub body: String,
    pub at: String,
}

/// Which merge requests to list.
pub enum MrFilter<'a> {
    Target(&'a str),
    /// MRs into skills this key owns.
    Owner(&'a str),
    /// MRs this key opened.
    Author(&'a str),
}

pub struct Store {
    conn: Connection,
    data_dir: PathBuf,
}

fn json_list(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn version_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<VersionRow> {
    Ok(VersionRow {
        name: r.get(0)?,
        version: r.get(1)?,
        sha256: r.get(2)?,
        description: r.get(3)?,
        tags: json_list(&r.get::<_, String>(4)?),
        groups: json_list(&r.get::<_, String>(5)?),
        published_at: r.get(6)?,
        yanked: r.get(7)?,
        removed: r.get(8)?,
        commit_id: r.get(9)?,
    })
}

const VERSION_COLS: &str = r#"name, version, sha256, description, tags, "groups", published_at, yanked, removed, commit_id"#;

const MR_COLS: &str = "id, target, fork, title, description, state, author_key, target_owner_key, \
     base_commit, head_commit, created_at, updated_at, merged_commit, merged_version";

fn mr_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MrRow> {
    Ok(MrRow {
        id: r.get(0)?,
        target: r.get(1)?,
        fork: r.get(2)?,
        title: r.get(3)?,
        description: r.get(4)?,
        state: r.get(5)?,
        author_key: r.get(6)?,
        target_owner_key: r.get(7)?,
        base_commit: r.get(8)?,
        head_commit: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        merged_commit: r.get(12)?,
        merged_version: r.get(13)?,
    })
}

fn skill_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SkillRow> {
    Ok(SkillRow {
        name: r.get(0)?,
        owner_key: r.get(1)?,
        created_at: r.get(2)?,
        parent: r.get(3)?,
    })
}

/// `k-` + the first 8 hex digits of sha256(public key bytes): the handle a
/// key has until it picks one.
pub fn default_handle(key: &str) -> String {
    let bytes = crate::auth::decode_b64(key).unwrap_or_default();
    format!("k-{}", &crate::auth::sha256_hex(&bytes)[..8])
}

impl Store {
    /// Open (creating) `<data_dir>/hub.sqlite`.
    pub fn open(data_dir: &Path) -> anyhow::Result<Store> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create {}", data_dir.display()))?;
        let conn = Connection::open(data_dir.join("hub.sqlite")).context("open hub.sqlite")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA).context("create schema")?;
        for (table, col, decl) in ADDED_COLUMNS {
            let has: bool = conn.query_row(
                &format!("SELECT COUNT(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?1"),
                [col],
                |r| r.get(0),
            )?;
            if !has {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))
                    .with_context(|| format!("add {table}.{col}"))?;
            }
        }
        conn.execute_batch(LATE_INDEXES).context("create indexes")?;
        Ok(Store {
            conn,
            data_dir: data_dir.to_path_buf(),
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Changes whenever this or any other connection (the admin CLI) has
    /// written: the `index.json` cache key.
    pub fn change_stamp(&self) -> anyhow::Result<(i64, i64)> {
        let dv: i64 = self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        let own: i64 = self
            .conn
            .query_row("SELECT total_changes()", [], |r| r.get(0))?;
        Ok((dv, own))
    }

    // ── publishers ──────────────────────────────────────────────────────

    pub fn publisher(&self, key: &str) -> anyhow::Result<Option<Publisher>> {
        Ok(self
            .conn
            .query_row(
                "SELECT public_key, first_seen, banned, publish_count_day, day, handle
                 FROM publishers WHERE public_key = ?1",
                [key],
                |r| {
                    Ok(Publisher {
                        public_key: r.get(0)?,
                        first_seen: r.get(1)?,
                        banned: r.get(2)?,
                        publish_count_day: r.get(3)?,
                        day: r.get(4)?,
                        handle: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Record `key` as seen (no-op if it already is).
    pub fn ensure_publisher(&self, key: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO publishers (public_key, first_seen) VALUES (?1, ?2)",
            params![key, crate::now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn set_banned(&self, key: &str, banned: bool) -> anyhow::Result<()> {
        self.ensure_publisher(key)?;
        self.conn.execute(
            "UPDATE publishers SET banned = ?2 WHERE public_key = ?1",
            params![key, banned],
        )?;
        Ok(())
    }

    /// The key's chosen handle, else its default one.
    pub fn handle(&self, key: &str) -> anyhow::Result<String> {
        Ok(self
            .publisher(key)?
            .and_then(|p| p.handle)
            .unwrap_or_else(|| default_handle(key)))
    }

    /// Choose `key`'s handle. False when another key has it (chosen, or
    /// as its default).
    pub fn set_handle(&self, key: &str, handle: &str) -> anyhow::Result<bool> {
        self.ensure_publisher(key)?;
        let holder: Option<String> = self
            .conn
            .query_row(
                "SELECT public_key FROM publishers WHERE handle = ?1",
                [handle],
                |r| r.get(0),
            )
            .optional()?;
        if holder.is_some_and(|h| h != key) {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE publishers SET handle = ?2 WHERE public_key = ?1",
            params![key, handle],
        )?;
        Ok(true)
    }

    /// Successful publishes by `key` on `today`.
    pub fn publishes_today(&self, key: &str, today: &str) -> anyhow::Result<u32> {
        Ok(match self.publisher(key)? {
            Some(p) if p.day == today => p.publish_count_day,
            _ => 0,
        })
    }

    fn count_publish(&self, key: &str, today: &str) -> anyhow::Result<()> {
        let n = self.publishes_today(key, today)? + 1;
        self.conn.execute(
            "UPDATE publishers SET publish_count_day = ?2, day = ?3 WHERE public_key = ?1",
            params![key, n, today],
        )?;
        Ok(())
    }

    // ── skills / forks / versions ───────────────────────────────────────

    pub fn skill(&self, name: &str) -> anyhow::Result<Option<SkillRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name, owner_key, created_at, parent FROM skills WHERE name = ?1",
                [name],
                skill_from_row,
            )
            .optional()?)
    }

    pub fn skill_owner(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.skill(name)?.map(|s| s.owner_key))
    }

    pub fn forks_of(&self, parent: &str) -> anyhow::Result<Vec<SkillRow>> {
        let mut st = self.conn.prepare(
            "SELECT name, owner_key, created_at, parent FROM skills WHERE parent = ?1 ORDER BY name",
        )?;
        let rows = st.query_map([parent], skill_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// `key`'s fork of `parent`, if it has one.
    pub fn fork_by(&self, parent: &str, key: &str) -> anyhow::Result<Option<SkillRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name, owner_key, created_at, parent FROM skills
                 WHERE parent = ?1 AND owner_key = ?2",
                [parent, key],
                skill_from_row,
            )
            .optional()?)
    }

    pub fn insert_fork(&self, fork: &str, owner: &str, parent: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO skills (name, owner_key, created_at, parent) VALUES (?1, ?2, ?3, ?4)",
            params![fork, owner, crate::now_rfc3339(), parent],
        )?;
        Ok(())
    }

    pub fn version(&self, name: &str, version: &str) -> anyhow::Result<Option<VersionRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {VERSION_COLS} FROM versions WHERE name = ?1 AND version = ?2"),
                [name, version],
                version_from_row,
            )
            .optional()?)
    }

    pub fn versions_of(&self, name: &str) -> anyhow::Result<Vec<VersionRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {VERSION_COLS} FROM versions WHERE name = ?1"
        ))?;
        let rows = st.query_map([name], version_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Listed (neither yanked nor removed) versions of `name`, newest first.
    pub fn listed_versions(&self, name: &str) -> anyhow::Result<Vec<String>> {
        let mut v: Vec<String> = self
            .versions_of(name)?
            .into_iter()
            .filter(|v| !v.yanked && !v.removed)
            .map(|v| v.version)
            .collect();
        format::sort_versions_desc(&mut v);
        Ok(v)
    }

    pub fn all_versions(&self) -> anyhow::Result<Vec<VersionRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {VERSION_COLS} FROM versions ORDER BY name, version"
        ))?;
        let rows = st.query_map([], version_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Store a new version row owned by `owner` (claiming the name if it
    /// is free), pointing at `commit_id`; create any of its groups that
    /// don't exist yet (owned by `owner`; not for forks); and count it
    /// against `owner`'s daily limit when `count_for_day` is set. Use
    /// [`commit_version`], which makes the commit first.
    fn insert_version(
        &mut self,
        owner: &str,
        doc: &SkillDoc,
        skill_md: &str,
        commit_id: &str,
        count_for_day: Option<&str>,
    ) -> anyhow::Result<()> {
        let sha = crate::auth::sha256_hex(skill_md.as_bytes());
        let now = crate::now_rfc3339();
        let is_fork = format::split_fork_name(&doc.name).is_some();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO skills (name, owner_key, created_at) VALUES (?1, ?2, ?3)",
            params![doc.name, owner, now],
        )?;
        tx.execute(
            r#"INSERT INTO versions (name, version, sha256, description, tags, "groups", published_at, commit_id)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            params![
                doc.name,
                doc.version,
                sha,
                doc.description,
                serde_json::to_string(&doc.tags)?,
                serde_json::to_string(&doc.groups)?,
                now,
                commit_id
            ],
        )?;
        if !is_fork {
            for slug in &doc.groups {
                tx.execute(
                    r#"INSERT OR IGNORE INTO "groups" (slug, name, description, owner_key)
                       VALUES (?1, ?1, '', ?2)"#,
                    params![slug, owner],
                )?;
            }
        }
        tx.commit()?;
        if let Some(today) = count_for_day {
            self.count_publish(owner, today)?;
        }
        Ok(())
    }

    pub fn set_yanked(&self, name: &str, version: &str, yanked: bool) -> anyhow::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE versions SET yanked = ?3 WHERE name = ?1 AND version = ?2",
            params![name, version, yanked],
        )? > 0)
    }

    /// Admin takedown: one version, or every version of `name`. Returns
    /// how many versions were newly removed.
    pub fn takedown(&self, name: &str, version: Option<&str>) -> anyhow::Result<usize> {
        Ok(match version {
            Some(v) => self.conn.execute(
                "UPDATE versions SET removed = 1 WHERE name = ?1 AND version = ?2 AND removed = 0",
                params![name, v],
            )?,
            None => self.conn.execute(
                "UPDATE versions SET removed = 1 WHERE name = ?1 AND removed = 0",
                params![name],
            )?,
        })
    }

    /// Admin: hand `name` (an original skill, not a fork) to another key.
    /// Returns false when there is no such skill.
    pub fn transfer(&self, name: &str, to_key: &str) -> anyhow::Result<bool> {
        self.ensure_publisher(to_key)?;
        Ok(self.conn.execute(
            "UPDATE skills SET owner_key = ?2 WHERE name = ?1 AND parent IS NULL",
            params![name, to_key],
        )? > 0)
    }

    /// True when every version of `name` was taken down (and it has some).
    pub fn fully_removed(&self, name: &str) -> anyhow::Result<bool> {
        let (n, removed): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(removed), 0) FROM versions WHERE name = ?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(n > 0 && n == removed)
    }

    /// Replace a version's group list (import: membership listed only in
    /// `groups/<slug>.json`).
    pub fn set_version_groups(
        &self,
        name: &str,
        version: &str,
        groups: &[String],
    ) -> anyhow::Result<()> {
        self.conn.execute(
            r#"UPDATE versions SET "groups" = ?3 WHERE name = ?1 AND version = ?2"#,
            params![name, version, serde_json::to_string(groups)?],
        )?;
        Ok(())
    }

    // ── commit authorship ───────────────────────────────────────────────

    pub fn record_author(&self, commit: &str, key: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO commit_authors (commit_id, public_key) VALUES (?1, ?2)",
            params![commit, key],
        )?;
        Ok(())
    }

    pub fn commit_author(&self, commit: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT public_key FROM commit_authors WHERE commit_id = ?1",
                [commit],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Commits of taken-down versions (never shown in a diff).
    pub fn removed_commits(&self, name: &str) -> anyhow::Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT commit_id FROM versions WHERE removed = 1 AND commit_id IS NOT NULL
             AND (name = ?1 OR name IN (SELECT parent FROM skills WHERE name = ?1))",
        )?;
        let rows = st.query_map([name], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ── merge requests ──────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn insert_mr(
        &self,
        target: &str,
        fork: &str,
        title: &str,
        description: &str,
        author_key: &str,
        target_owner_key: &str,
        base_commit: &str,
        head_commit: &str,
    ) -> anyhow::Result<i64> {
        let now = crate::now_rfc3339();
        self.conn.execute(
            "INSERT INTO merge_requests (target, fork, title, description, author_key,
                 target_owner_key, base_commit, head_commit, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                target,
                fork,
                title,
                description,
                author_key,
                target_owner_key,
                base_commit,
                head_commit,
                now
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn mr(&self, id: i64) -> anyhow::Result<Option<MrRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MR_COLS} FROM merge_requests WHERE id = ?1"),
                [id],
                mr_from_row,
            )
            .optional()?)
    }

    pub fn open_mr_for_fork(&self, fork: &str) -> anyhow::Result<Option<MrRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MR_COLS} FROM merge_requests WHERE fork = ?1 AND state = 'open'"),
                [fork],
                mr_from_row,
            )
            .optional()?)
    }

    /// Newest first. `state` `None` = all states.
    pub fn mrs(&self, filter: MrFilter, state: Option<&str>) -> anyhow::Result<Vec<MrRow>> {
        let (col, val) = match filter {
            MrFilter::Target(t) => ("target", t),
            MrFilter::Owner(k) => ("target_owner_key", k),
            MrFilter::Author(k) => ("author_key", k),
        };
        let sql = format!(
            "SELECT {MR_COLS} FROM merge_requests WHERE {col} = ?1 AND (?2 IS NULL OR state = ?2)
             ORDER BY id DESC LIMIT 500"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![val, state], mr_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A fork moved: point its open MR at the new head.
    pub fn update_open_mr_head(&self, fork: &str, head: &str, base: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE merge_requests SET head_commit = ?2, base_commit = ?3, updated_at = ?4
             WHERE fork = ?1 AND state = 'open' AND (head_commit != ?2 OR base_commit != ?3)",
            params![fork, head, base, crate::now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn close_mr(&self, id: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE merge_requests SET state = 'closed', updated_at = ?2 WHERE id = ?1",
            params![id, crate::now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn mark_merged(
        &self,
        id: i64,
        base: &str,
        head: &str,
        merged_commit: &str,
        merged_version: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE merge_requests SET state = 'merged', base_commit = ?2, head_commit = ?3,
                 merged_commit = ?4, merged_version = ?5, updated_at = ?6 WHERE id = ?1",
            params![
                id,
                base,
                head,
                merged_commit,
                merged_version,
                crate::now_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn add_comment(
        &self,
        mr_id: i64,
        author_key: &str,
        body: &str,
    ) -> anyhow::Result<CommentRow> {
        let at = crate::now_rfc3339();
        self.conn.execute(
            "INSERT INTO mr_comments (mr_id, author_key, body, at) VALUES (?1, ?2, ?3, ?4)",
            params![mr_id, author_key, body, at],
        )?;
        self.conn.execute(
            "UPDATE merge_requests SET updated_at = ?2 WHERE id = ?1",
            params![mr_id, at],
        )?;
        Ok(CommentRow {
            id: self.conn.last_insert_rowid(),
            mr_id,
            author_key: author_key.to_string(),
            body: body.to_string(),
            at,
        })
    }

    pub fn comments(&self, mr_id: i64) -> anyhow::Result<Vec<CommentRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, mr_id, author_key, body, at FROM mr_comments WHERE mr_id = ?1 ORDER BY id",
        )?;
        let rows = st.query_map([mr_id], |r| {
            Ok(CommentRow {
                id: r.get(0)?,
                mr_id: r.get(1)?,
                author_key: r.get(2)?,
                body: r.get(3)?,
                at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ── groups ──────────────────────────────────────────────────────────

    pub fn group(&self, slug: &str) -> anyhow::Result<Option<GroupRow>> {
        Ok(self
            .conn
            .query_row(
                r#"SELECT slug, name, description, owner_key FROM "groups" WHERE slug = ?1"#,
                [slug],
                |r| {
                    Ok(GroupRow {
                        slug: r.get(0)?,
                        name: r.get(1)?,
                        description: r.get(2)?,
                        owner_key: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn groups(&self) -> anyhow::Result<Vec<GroupRow>> {
        let mut st = self
            .conn
            .prepare(r#"SELECT slug, name, description, owner_key FROM "groups" ORDER BY slug"#)?;
        let rows = st.query_map([], |r| {
            Ok(GroupRow {
                slug: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                owner_key: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Create the group, or update its name/description when `owner` owns
    /// it. Returns false when someone else owns it (left unchanged).
    pub fn upsert_group(&self, g: &GroupRow) -> anyhow::Result<bool> {
        match self.group(&g.slug)? {
            Some(cur) if cur.owner_key != g.owner_key => Ok(false),
            Some(_) => {
                self.conn.execute(
                    r#"UPDATE "groups" SET name = ?2, description = ?3 WHERE slug = ?1"#,
                    params![g.slug, g.name, g.description],
                )?;
                Ok(true)
            }
            None => {
                self.conn.execute(
                    r#"INSERT INTO "groups" (slug, name, description, owner_key)
                       VALUES (?1, ?2, ?3, ?4)"#,
                    params![g.slug, g.name, g.description, g.owner_key],
                )?;
                Ok(true)
            }
        }
    }

    // ── audit ───────────────────────────────────────────────────────────

    pub fn audit(&self, action: &str, key: Option<&str>, detail: &str) {
        if let Err(e) = self.conn.execute(
            "INSERT INTO audit_log (at, action, public_key, detail) VALUES (?1, ?2, ?3, ?4)",
            params![crate::now_rfc3339(), action, key, detail],
        ) {
            tracing::warn!("audit log write failed: {e}");
        }
    }

    // ── content (git) ───────────────────────────────────────────────────

    /// The `SKILL.md` text of a served version (`None` if unknown or taken
    /// down; yanked versions are still served).
    pub fn served_skill(
        &self,
        repos: &Repos,
        name: &str,
        version: &str,
    ) -> anyhow::Result<Option<String>> {
        match self.version(name, version)? {
            Some(VersionRow {
                removed: false,
                commit_id: Some(c),
                ..
            }) => repos.read_skill(name, &c),
            _ => Ok(None),
        }
    }

    /// Pre-git body of a version (`blobs/<sha256>`), for the migration.
    fn legacy_blob(&self, sha: &str) -> Option<String> {
        if sha.is_empty() || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        std::fs::read_to_string(self.data_dir.join("blobs").join(sha)).ok()
    }

    // ── catalog ─────────────────────────────────────────────────────────

    /// The generated catalog: every skill (forks excluded) with at least
    /// one listed (neither yanked nor removed) version, its description
    /// and tags read from the newest listed version in git; groups with
    /// the listed skills whose newest version names them.
    pub fn index(&self, repos: &Repos) -> anyhow::Result<RepoIndex> {
        let mut by_name: BTreeMap<String, Vec<VersionRow>> = BTreeMap::new();
        for v in self.all_versions()? {
            if !v.yanked && !v.removed && v.commit_id.is_some() && !v.name.contains('@') {
                by_name.entry(v.name.clone()).or_default().push(v);
            }
        }
        let mut forks: HashMap<String, u32> = HashMap::new();
        {
            let mut st = self.conn.prepare(
                "SELECT parent, COUNT(*) FROM skills WHERE parent IS NOT NULL GROUP BY parent",
            )?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)))? {
                let (p, n) = r?;
                forks.insert(p, n);
            }
        }
        let mut skills = Vec::new();
        let mut members: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, rows) in by_name {
            let mut labels: Vec<String> = rows.iter().map(|r| r.version.clone()).collect();
            format::sort_versions_desc(&mut labels);
            let latest = labels[0].clone();
            let head = rows
                .iter()
                .find(|r| r.version == latest)
                .expect("latest row");
            let doc = head
                .commit_id
                .as_deref()
                .and_then(|c| repos.read_skill(&name, c).ok().flatten())
                .and_then(|t| format::parse_skill(&t).ok());
            let (description, tags) = match doc {
                Some(d) => (d.description, d.tags),
                None => (head.description.clone(), head.tags.clone()),
            };
            for g in &head.groups {
                members.entry(g.clone()).or_default().push(name.clone());
            }
            skills.push(IndexSkill {
                forks: forks.get(&name).copied().unwrap_or(0),
                name,
                description,
                tags,
                groups: head.groups.clone(),
                versions: labels,
                latest: Some(latest),
            });
        }
        let groups = self
            .groups()?
            .into_iter()
            .map(|g| IndexGroup {
                skills: members.remove(&g.slug).unwrap_or_default(),
                slug: g.slug,
                name: g.name,
                description: g.description,
            })
            .collect();
        Ok(RepoIndex {
            schema: 1,
            generated_at: Some(crate::now_rfc3339()),
            skills,
            groups,
        })
    }

    /// Online backup of the database to `dest`.
    pub fn backup_to(&self, dest: &Path) -> anyhow::Result<()> {
        self.conn
            .backup(rusqlite::DatabaseName::Main, dest, None)
            .context("sqlite backup")
    }
}

/// Commit `text` (parsed as `doc`) to the locked repo as version
/// `doc.version` — a commit on `main`, tagged `v<version>`, authored by
/// `owner`'s handle — then record the version row. `merge_parent` makes it
/// a merge commit.
#[allow(clippy::too_many_arguments)]
pub fn commit_version(
    store: &mut Store,
    repos: &Repos,
    lock: &RepoLock,
    owner: &str,
    doc: &SkillDoc,
    text: &str,
    merge_parent: Option<git2::Oid>,
    message: &str,
    count_for_day: Option<&str>,
) -> anyhow::Result<String> {
    anyhow::ensure!(lock.name() == doc.name, "repo lock for the wrong skill");
    let handle = store.handle(owner)?;
    let oid = repos.commit(
        lock,
        text,
        merge_parent,
        &Author {
            handle: &handle,
            at: None,
        },
        message,
        Some(&doc.version),
    )?;
    let id = oid.to_string();
    store.record_author(&id, owner)?;
    store.insert_version(owner, doc, text, &id, count_for_day)?;
    Ok(id)
}

/// Build repos for versions stored before git (`blobs/` + rows without a
/// `commit_id`): one commit + tag each, in published order, authored by the
/// skill's owner at the original publish time. Idempotent; a version whose
/// tag already exists (an interrupted run) adopts that commit. Returns how
/// many versions were migrated.
pub fn migrate_to_git(store: &mut Store, repos: &Repos) -> anyhow::Result<usize> {
    let pending: Vec<(VersionRow, String)> = {
        let mut st = store.conn.prepare(&format!(
            "SELECT {}, s.owner_key FROM versions v JOIN skills s ON s.name = v.name
             WHERE v.commit_id IS NULL ORDER BY v.published_at, v.rowid",
            VERSION_COLS
                .split(", ")
                .map(|c| format!("v.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
        let rows = st.query_map([], |r| Ok((version_from_row(r)?, r.get::<_, String>(10)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut n = 0;
    for (v, owner) in pending {
        let lock = repos.lock(&v.name);
        let existing = repos
            .exists(&v.name)
            .then(|| repos.open_repo(&v.name).ok())
            .flatten()
            .and_then(|r| crate::git::tag_commit(&r, &v.version));
        let oid = match existing {
            Some(oid) => oid,
            None => {
                let Some(text) = store.legacy_blob(&v.sha256) else {
                    tracing::warn!(
                        "migration: {}@{} has no blob {}; left unserved",
                        v.name,
                        v.version,
                        v.sha256
                    );
                    continue;
                };
                let handle = store.handle(&owner)?;
                let at = chrono::DateTime::parse_from_rfc3339(&v.published_at)
                    .map(|t| t.timestamp())
                    .ok();
                repos.commit(
                    &lock,
                    &text,
                    None,
                    &Author {
                        handle: &handle,
                        at,
                    },
                    &format!("Publish {}", v.version),
                    Some(&v.version),
                )?
            }
        };
        let id = oid.to_string();
        store.record_author(&id, &owner)?;
        store.conn.execute(
            "UPDATE versions SET commit_id = ?3 WHERE name = ?1 AND version = ?2",
            params![v.name, v.version, id],
        )?;
        n += 1;
    }
    if n > 0 {
        store.audit("migrate", None, &format!("{n} versions moved into git"));
    }
    Ok(n)
}
