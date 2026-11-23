//! Skill storage in git. One bare repo per skill (`<root>/<name>.git`) and
//! per fork (`<root>/forks/<name>@<handle>.git`), each tracking a single
//! `SKILL.md` on `main`. Every publish is a commit on `main` tagged
//! `v<version>`. Git is never served over the network; repos only ever talk
//! to each other through local fetches.
//!
//! A skill's repo also carries `refs/forks/<fork>` — each fork's `main`,
//! fetched in after every write to the fork — so ahead/behind counts,
//! mergeability and merges are all computed inside the original's repo.
//!
//! Writes to a repo need its [`RepoLock`]. Never hold two at once (see
//! [`Repos::lock`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use anyhow::Context;
use git2::{
    AutotagOption, DiffFormat, DiffOptions, ErrorCode, FetchOptions, Oid, Repository,
    RepositoryInitOptions, Signature, Sort, Time,
};
use peckboard_skill_format::format;

pub const SKILL_FILE: &str = "SKILL.md";
pub const MAIN: &str = "refs/heads/main";
/// Commit author e-mails are `<handle>@` this.
pub const AUTHOR_DOMAIN: &str = "keys.skills.peckboard.com";
/// Longest history returned.
pub const MAX_HISTORY: usize = 1000;

/// `refs/forks/<fork>` in the original's repo: the fork's `main`.
pub fn fork_ref(fork: &str) -> String {
    format!("refs/forks/{fork}")
}

/// Who a commit is attributed to (the key itself is kept in SQLite).
pub struct Author<'a> {
    pub handle: &'a str,
    /// Unix seconds; `None` = now.
    pub at: Option<i64>,
}

impl Author<'_> {
    fn signature(&self) -> anyhow::Result<Signature<'static>> {
        let at = self.at.unwrap_or_else(|| chrono::Utc::now().timestamp());
        Ok(Signature::new(
            self.handle,
            &format!("{}@{AUTHOR_DOMAIN}", self.handle),
            &Time::new(at, 0),
        )?)
    }
}

pub struct Repos {
    root: PathBuf,
    busy: Mutex<HashSet<String>>,
    freed: Condvar,
    /// Writers hold it shared; a backup holds it exclusively so the tarball
    /// never catches a ref without its objects.
    gate: RwLock<()>,
}

/// Proof token: the bearer holds the write lock of repo `name` (and a share
/// of the backup gate). Every git write takes one, so writes to a repo are
/// serialized. Constructed only by [`Repos::lock`]; see `api::publish` for
/// an example.
pub struct RepoLock<'a> {
    repos: &'a Repos,
    name: String,
    _gate: RwLockReadGuard<'a, ()>,
}

impl RepoLock<'_> {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for RepoLock<'_> {
    fn drop(&mut self) {
        let mut busy = self.repos.busy.lock().unwrap_or_else(|p| p.into_inner());
        busy.remove(&self.name);
        self.repos.freed.notify_all();
    }
}

/// Result of a frontmatter-neutral 3-way merge of `SKILL.md`.
#[derive(Debug, PartialEq)]
pub enum Merge {
    /// Merged text, with placeholder `name`/`version` the caller replaces.
    Clean(String),
    Conflict,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub message: String,
    pub at: i64,
}

fn not_found(e: &git2::Error) -> bool {
    e.code() == ErrorCode::NotFound
}

impl Repos {
    pub fn open(root: PathBuf) -> anyhow::Result<Repos> {
        std::fs::create_dir_all(root.join("forks"))
            .with_context(|| format!("create {}", root.display()))?;
        Ok(Repos {
            root,
            busy: Mutex::new(HashSet::new()),
            freed: Condvar::new(),
            gate: RwLock::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bare repo of a skill or fork (names are validated by callers).
    pub fn path(&self, name: &str) -> PathBuf {
        if name.contains('@') {
            self.root.join("forks").join(format!("{name}.git"))
        } else {
            self.root.join(format!("{name}.git"))
        }
    }

    /// Take the write lock of repo `name`, waiting for another writer to
    /// finish. Don't call it while holding another `RepoLock` or the store
    /// lock (lock order: repo lock, then store).
    pub fn lock(&self, name: &str) -> RepoLock<'_> {
        let gate = self.gate.read().unwrap_or_else(|p| p.into_inner());
        let mut busy = self.busy.lock().unwrap_or_else(|p| p.into_inner());
        while busy.contains(name) {
            busy = self.freed.wait(busy).unwrap_or_else(|p| p.into_inner());
        }
        busy.insert(name.to_string());
        drop(busy);
        RepoLock {
            repos: self,
            name: name.to_string(),
            _gate: gate,
        }
    }

    /// Block every git write (backups).
    pub fn freeze(&self) -> RwLockWriteGuard<'_, ()> {
        self.gate.write().unwrap_or_else(|p| p.into_inner())
    }

    pub fn exists(&self, name: &str) -> bool {
        self.path(name).join("HEAD").is_file()
    }

    /// Open an existing repo.
    pub fn open_repo(&self, name: &str) -> anyhow::Result<Repository> {
        let p = self.path(name);
        Repository::open_bare(&p).with_context(|| format!("open {}", p.display()))
    }

    /// Open a repo for reading; objects written through it (merge
    /// scratch) stay in memory and vanish with the handle.
    pub fn open_scratch(&self, name: &str) -> anyhow::Result<Repository> {
        let repo = self.open_repo(name)?;
        repo.odb()?.add_new_mempack_backend(1000)?;
        Ok(repo)
    }

    fn open_or_init(&self, lock: &RepoLock) -> anyhow::Result<Repository> {
        if self.exists(lock.name()) {
            return self.open_repo(lock.name());
        }
        let p = self.path(lock.name());
        let mut o = RepositoryInitOptions::new();
        o.bare(true).initial_head("main").mkpath(true);
        Repository::init_opts(&p, &o).with_context(|| format!("init {}", p.display()))
    }

    /// Commit `text` as `SKILL.md` on `main` (parents: the current `main`
    /// tip, if any, then `merge_parent`), optionally tagging it
    /// `v<version>`.
    pub fn commit(
        &self,
        lock: &RepoLock,
        text: &str,
        merge_parent: Option<Oid>,
        author: &Author,
        message: &str,
        tag: Option<&str>,
    ) -> anyhow::Result<Oid> {
        let repo = self.open_or_init(lock)?;
        let tree = repo.find_tree(skill_tree(&repo, text)?)?;
        let mut parents = Vec::new();
        if let Some(h) = main_head(&repo) {
            parents.push(repo.find_commit(h)?);
        }
        if let Some(m) = merge_parent {
            parents.push(repo.find_commit(m)?);
        }
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        let sig = author.signature()?;
        let oid = repo.commit(Some(MAIN), &sig, &sig, message, &tree, &refs)?;
        if let Some(v) = tag {
            repo.reference(&tag_ref(v), oid, true, "publish")?;
        }
        Ok(oid)
    }

    /// Point `main` of the locked repo at `to` (fast-forward; the caller
    /// checked ancestry).
    pub fn set_main(&self, lock: &RepoLock, to: Oid) -> anyhow::Result<()> {
        let repo = self.open_or_init(lock)?;
        repo.reference(MAIN, to, true, "fast-forward")?;
        Ok(())
    }

    /// Fetch `src_ref` of repo `src` into `dst_ref` of the locked repo
    /// (objects included, no tags). `None` (and `dst_ref` left alone) when
    /// `src` has no such ref.
    pub fn fetch(
        &self,
        lock: &RepoLock,
        src: &str,
        src_ref: &str,
        dst_ref: &str,
    ) -> anyhow::Result<Option<Oid>> {
        if !self.exists(src) {
            return Ok(None);
        }
        let Ok(want) = self.open_repo(src)?.refname_to_id(src_ref) else {
            return Ok(None);
        };
        let repo = self.open_or_init(lock)?;
        if repo.refname_to_id(dst_ref).ok() == Some(want) {
            return Ok(Some(want));
        }
        if repo.find_commit(want).is_err() {
            let src_path = self.path(src);
            let url = src_path.to_str().context("non-UTF-8 repo path")?;
            let mut remote = repo.remote_anonymous(url)?;
            let mut fo = FetchOptions::new();
            fo.download_tags(AutotagOption::None);
            remote
                .fetch(&[format!("+{src_ref}:{dst_ref}")], Some(&mut fo), None)
                .with_context(|| format!("fetch {src} {src_ref}"))?;
        }
        repo.reference(dst_ref, want, true, "fetch")?;
        Ok(Some(want))
    }

    /// `SKILL.md` at `commit` (hex) in repo `name`.
    pub fn read_skill(&self, name: &str, commit: &str) -> anyhow::Result<Option<String>> {
        if !self.exists(name) {
            return Ok(None);
        }
        let Ok(oid) = Oid::from_str(commit) else {
            return Ok(None);
        };
        skill_at(&self.open_repo(name)?, oid)
    }

    /// `main`'s history, newest first.
    pub fn history(&self, name: &str) -> anyhow::Result<Vec<CommitInfo>> {
        if !self.exists(name) {
            return Ok(Vec::new());
        }
        let repo = self.open_repo(name)?;
        let Some(head) = main_head(&repo) else {
            return Ok(Vec::new());
        };
        let mut walk = repo.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
        walk.push(head)?;
        let mut out = Vec::new();
        for oid in walk.take(MAX_HISTORY) {
            let c = repo.find_commit(oid?)?;
            out.push(CommitInfo {
                id: c.id().to_string(),
                parents: c.parent_ids().map(|p| p.to_string()).collect(),
                author: c.author().name().unwrap_or("").to_string(),
                message: c.message().unwrap_or("").trim_end().to_string(),
                at: c.time().seconds(),
            });
        }
        Ok(out)
    }
}

fn tag_ref(version: &str) -> String {
    format!("refs/tags/v{version}")
}

pub fn main_head(repo: &Repository) -> Option<Oid> {
    repo.refname_to_id(MAIN).ok()
}

/// The commit tagged `v<version>`, if any.
pub fn tag_commit(repo: &Repository, version: &str) -> Option<Oid> {
    repo.refname_to_id(&tag_ref(version)).ok()
}

/// Commit id (hex) → tag name (`v1.2.0`).
pub fn tags(repo: &Repository) -> anyhow::Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for r in repo.references_glob("refs/tags/*")? {
        let r = r?;
        if let (Some(target), Some(name)) = (r.target(), r.name()) {
            out.insert(
                target.to_string(),
                name.trim_start_matches("refs/tags/").to_string(),
            );
        }
    }
    Ok(out)
}

/// A tree holding just `SKILL.md` = `text`.
fn skill_tree(repo: &Repository, text: &str) -> anyhow::Result<Oid> {
    let blob = repo.blob(text.as_bytes())?;
    let mut tb = repo.treebuilder(None)?;
    tb.insert(SKILL_FILE, blob, 0o100644)?;
    Ok(tb.write()?)
}

/// `SKILL.md` at `commit`, `None` if the commit (or file) is missing.
pub fn skill_at(repo: &Repository, commit: Oid) -> anyhow::Result<Option<String>> {
    let c = match repo.find_commit(commit) {
        Ok(c) => c,
        Err(e) if not_found(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let tree = c.tree()?;
    let Some(entry) = tree.get_name(SKILL_FILE) else {
        return Ok(None);
    };
    let blob = repo.find_blob(entry.id())?;
    Ok(Some(String::from_utf8_lossy(blob.content()).into_owned()))
}

/// Resolve a commit id (4–40 hex digits) or `main`.
pub fn resolve(repo: &Repository, rev: &str) -> Option<Oid> {
    if rev == "main" {
        return main_head(repo);
    }
    if !(4..=40).contains(&rev.len()) || !rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    repo.revparse_single(rev)
        .ok()?
        .peel_to_commit()
        .ok()
        .map(|c| c.id())
}

/// `a` contains `b` (equal or descendant).
pub fn contains(repo: &Repository, a: Oid, b: Oid) -> bool {
    a == b || repo.graph_descendant_of(a, b).unwrap_or(false)
}

fn patch_text(diff: &git2::Diff) -> anyhow::Result<String> {
    let mut out = Vec::new();
    diff.print(DiffFormat::Patch, |_, _, line| {
        if matches!(line.origin(), '+' | '-' | ' ') {
            out.push(line.origin() as u8);
        }
        out.extend_from_slice(line.content());
        true
    })?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Unified diff of `SKILL.md` between two commits.
pub fn diff_commits(repo: &Repository, from: Oid, to: Oid) -> anyhow::Result<String> {
    let a = repo.find_commit(from)?.tree()?;
    let b = repo.find_commit(to)?.tree()?;
    let mut opts = DiffOptions::new();
    opts.pathspec(SKILL_FILE);
    let diff = repo.diff_tree_to_tree(Some(&a), Some(&b), Some(&mut opts))?;
    patch_text(&diff)
}

/// Unified diff of two `SKILL.md` texts.
pub fn diff_texts(old: &str, new: &str) -> anyhow::Result<String> {
    let p = Path::new(SKILL_FILE);
    let mut patch =
        git2::Patch::from_buffers(old.as_bytes(), Some(p), new.as_bytes(), Some(p), None)?;
    let buf = patch.to_buf()?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// `name` and `version` always differ between a skill and its forks (and
/// between versions), and are set by whoever writes the result, so they
/// are neutralized on every side before merging.
fn neutral(text: &str) -> String {
    format::set_frontmatter_scalars(text, &[("name", "x"), ("version", "0.0.0")])
}

/// Frontmatter-neutral 3-way merge of `SKILL.md` between commits `ours`
/// and `theirs` (both in `repo`), via `merge_trees` on their merge base.
/// Use a [`Repos::open_scratch`] handle: the neutralized blobs are written
/// to its odb.
pub fn merge_skill(repo: &Repository, ours: Oid, theirs: Oid) -> anyhow::Result<Merge> {
    let base = repo.merge_base(ours, theirs).ok();
    let tree_of = |c: Option<Oid>| -> anyhow::Result<git2::Tree<'_>> {
        let text = match c {
            Some(c) => skill_at(repo, c)?.unwrap_or_default(),
            None => String::new(),
        };
        Ok(repo.find_tree(skill_tree(repo, &neutral(&text))?)?)
    };
    let (b, o, t) = (tree_of(base)?, tree_of(Some(ours))?, tree_of(Some(theirs))?);
    let idx = repo.merge_trees(&b, &o, &t, None)?;
    if idx.has_conflicts() {
        return Ok(Merge::Conflict);
    }
    let entry = idx
        .get_path(Path::new(SKILL_FILE), 0)
        .context("merge result has no SKILL.md")?;
    let blob = repo.find_blob(entry.id)?;
    Ok(Merge::Clean(
        String::from_utf8_lossy(blob.content()).into_owned(),
    ))
}

/// The `version` frontmatter of `text` (empty if unparseable).
pub fn version_of(text: &str) -> String {
    format::parse_skill(text)
        .map(|d| d.version)
        .unwrap_or_default()
}

/// Write `name` / `version` into merged text.
pub fn stamp(text: &str, name: &str, version: &str) -> String {
    format::set_frontmatter_scalars(text, &[("name", name), ("version", version)])
}
