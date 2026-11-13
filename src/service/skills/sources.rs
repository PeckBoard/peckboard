//! Remote skill sources: the public `index.json` catalog (HTTP) and team
//! repos (a private git repo cloned under `<data_dir>/skills/<source_id>`).
//!
//! Both use the same repo layout (see `tmp-skills-contract.md` / the
//! module docs in `super`):
//!
//! ```text
//! skills/<name>/<version>/SKILL.md
//! groups/<slug>.json      {"slug","name","description","skills":[name,...]}
//! memory/<slug>.md        (team repos only)
//! index.json              generated catalog
//! ```
//!
//! Team repos are server-managed clones: sync is fetch + hard reset to the
//! remote branch, publish is write + commit + push (rebasing once on a
//! rejected push). git runs with the same `-c` hardening as every other
//! server-side git call; credentials come from a `secret_ref`
//! (`ssh_key:<id>` → a temporary 0600 identity file, `env:<NAME>` → an HTTP
//! auth header passed through `GIT_CONFIG_*` env, never argv).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::format::{self, SkillDoc};
use crate::db::Db;
use crate::db::models::SkillSourceRow;

/// The default public catalog the migration seeds as source `public`.
pub const DEFAULT_PUBLIC_INDEX_URL: &str =
    "https://raw.githubusercontent.com/PeckBoard/skills/main/index.json";
/// Overrides the seeded public source's URL at runtime (e2e mock index).
pub const INDEX_URL_ENV: &str = "PECKBOARD_SKILLS_INDEX_URL";
/// Id of the seeded public source.
pub const PUBLIC_SOURCE_ID: &str = "public";

const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
const MAX_SKILL_BYTES: usize = 512 * 1024;
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

// ── Catalog (index.json) ────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexSkill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    /// Newest first.
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub latest: Option<String>,
}

impl IndexSkill {
    pub fn latest_version(&self) -> Option<String> {
        self.latest
            .clone()
            .filter(|v| !v.is_empty())
            .or_else(|| self.versions.first().cloned())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexGroup {
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RepoIndex {
    #[serde(default = "schema_one")]
    pub schema: u32,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub skills: Vec<IndexSkill>,
    #[serde(default)]
    pub groups: Vec<IndexGroup>,
}

fn schema_one() -> u32 {
    1
}

/// The URL a public source is read from: the env override wins for the
/// seeded source.
pub fn effective_url(source: &SkillSourceRow) -> String {
    if source.id == PUBLIC_SOURCE_ID
        && let Ok(u) = std::env::var(INDEX_URL_ENV)
        && !u.trim().is_empty()
    {
        return u.trim().to_string();
    }
    source.url.clone()
}

/// `https://host/a/b/index.json` → `https://host/a/b/`.
fn base_url(index_url: &str) -> String {
    match index_url.rsplit_once('/') {
        Some((base, _)) => format!("{base}/"),
        None => format!("{index_url}/"),
    }
}

async fn http_get_text(url: &str, cap: usize) -> anyhow::Result<String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        anyhow::bail!("source url must be http(s)");
    }
    let client = crate::http_client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    if bytes.len() > cap {
        anyhow::bail!("response from {url} exceeds {cap} bytes");
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Fetch a public source's catalog.
pub async fn fetch_public_index(source: &SkillSourceRow) -> anyhow::Result<RepoIndex> {
    let url = effective_url(source);
    let text = http_get_text(&url, MAX_INDEX_BYTES).await?;
    let mut index: RepoIndex =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("bad index.json: {e}"))?;
    for s in &mut index.skills {
        format::sort_versions_desc(&mut s.versions);
    }
    Ok(index)
}

/// Fetch one `SKILL.md` from a public source.
pub async fn fetch_public_skill(
    source: &SkillSourceRow,
    name: &str,
    version: &str,
) -> anyhow::Result<SkillDoc> {
    if !format::valid_name(name) || semver::Version::parse(version).is_err() {
        anyhow::bail!("invalid skill name or version");
    }
    let url = format!(
        "{}skills/{name}/{version}/SKILL.md",
        base_url(&effective_url(source))
    );
    let text = http_get_text(&url, MAX_SKILL_BYTES).await?;
    let doc = format::parse_skill(&text).map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
    if doc.name != name || doc.version != version {
        anyhow::bail!("{url} declares {}@{}", doc.name, doc.version);
    }
    Ok(doc)
}

// ── Repo layout on disk (team clones, the public outbox) ────────────────

pub fn repo_dir(data_dir: &Path, source_id: &str) -> PathBuf {
    data_dir.join("skills").join(source_id)
}

fn skill_path(repo: &Path, name: &str, version: &str) -> PathBuf {
    repo.join("skills")
        .join(name)
        .join(version)
        .join("SKILL.md")
}

/// Read the catalog straight from a checkout (authoritative for team repos —
/// a hand-edited repo may lack an up-to-date `index.json`).
pub fn scan_repo(repo: &Path) -> RepoIndex {
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo.join("skills")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !format::valid_name(&name) {
                continue;
            }
            let mut versions: Vec<String> = std::fs::read_dir(e.path())
                .into_iter()
                .flatten()
                .flatten()
                .map(|v| v.file_name().to_string_lossy().to_string())
                .filter(|v| semver::Version::parse(v).is_ok())
                .filter(|v| skill_path(repo, &name, v).is_file())
                .collect();
            if versions.is_empty() {
                continue;
            }
            format::sort_versions_desc(&mut versions);
            let latest = versions[0].clone();
            let doc = read_skill(repo, &name, &latest).ok();
            skills.push(IndexSkill {
                name: name.clone(),
                description: doc
                    .as_ref()
                    .map(|d| d.description.clone())
                    .unwrap_or_default(),
                tags: doc.as_ref().map(|d| d.tags.clone()).unwrap_or_default(),
                groups: doc.as_ref().map(|d| d.groups.clone()).unwrap_or_default(),
                versions,
                latest: Some(latest),
            });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    let mut groups = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo.join("groups")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(g) = std::fs::read_to_string(&p)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<IndexGroup>(&t).map_err(|e| e.to_string()))
                && format::valid_name(&g.slug)
            {
                groups.push(g);
            }
        }
    }
    groups.sort_by(|a, b| a.slug.cmp(&b.slug));
    RepoIndex {
        schema: 1,
        generated_at: Some(chrono::Utc::now().to_rfc3339()),
        skills,
        groups,
    }
}

pub fn read_skill(repo: &Path, name: &str, version: &str) -> anyhow::Result<SkillDoc> {
    if !format::valid_name(name) || semver::Version::parse(version).is_err() {
        anyhow::bail!("invalid skill name or version");
    }
    let text = std::fs::read_to_string(skill_path(repo, name, version))?;
    format::parse_skill(&text).map_err(|e| anyhow::anyhow!(e))
}

/// Write `doc` at `skills/<name>/<version>/SKILL.md`. A version is
/// immutable: an existing file with different content is refused.
pub fn write_skill(repo: &Path, doc: &SkillDoc) -> anyhow::Result<PathBuf> {
    let path = skill_path(repo, &doc.name, &doc.version);
    let text = format::serialize_skill(doc);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == text {
            return Ok(path);
        }
        anyhow::bail!(
            "{}@{} is already published with different content — bump the version",
            doc.name,
            doc.version
        );
    }
    std::fs::create_dir_all(path.parent().expect("skill dir"))?;
    std::fs::write(&path, text)?;
    Ok(path)
}

pub fn write_group(repo: &Path, group: &IndexGroup) -> anyhow::Result<()> {
    let dir = repo.join("groups");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(format!("{}.json", group.slug)),
        serde_json::to_string_pretty(group)? + "\n",
    )?;
    Ok(())
}

/// Regenerate `index.json` from the checkout.
pub fn write_index(repo: &Path) -> anyhow::Result<()> {
    let index = scan_repo(repo);
    std::fs::write(
        repo.join("index.json"),
        serde_json::to_string_pretty(&index)? + "\n",
    )?;
    Ok(())
}

pub fn list_memory(repo: &Path) -> Vec<format::MemoryDoc> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo.join("memory")) {
        for e in entries.flatten() {
            let p = e.path();
            let Some(slug) = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".md"))
                .map(str::to_string)
            else {
                continue;
            };
            if !format::valid_name(&slug) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                out.push(format::parse_memory(&slug, &text));
            }
        }
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

pub fn memory_path(repo: &Path, slug: &str) -> anyhow::Result<PathBuf> {
    if !format::valid_name(slug) {
        anyhow::bail!("invalid memory slug '{slug}'");
    }
    Ok(repo.join("memory").join(format!("{slug}.md")))
}

// ── git ─────────────────────────────────────────────────────────────────

/// Credentials for one git operation. Dropping it deletes any temporary
/// identity file.
#[derive(Default)]
pub struct GitAuth {
    env: Vec<(String, String)>,
    key_file: Option<PathBuf>,
}

impl Drop for GitAuth {
    fn drop(&mut self) {
        if let Some(p) = self.key_file.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Resolve a source's `secret_ref` into git credentials.
pub async fn resolve_auth(
    db: &Db,
    data_dir: &Path,
    source: &SkillSourceRow,
) -> anyhow::Result<GitAuth> {
    let Some(r) = source
        .secret_ref
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    else {
        return Ok(GitAuth::default());
    };
    if let Some(key_id) = r.strip_prefix("ssh_key:") {
        let vault = crate::service::ssh_keys::load_or_create_vault_key(data_dir)?;
        let (pem, passphrase) =
            crate::service::ssh_keys::resolve_for_use(db, &vault, key_id).await?;
        if passphrase.is_some() {
            anyhow::bail!(
                "SSH key '{key_id}' is passphrase-protected; git sync needs an unencrypted key"
            );
        }
        let dir = data_dir.join("skills").join(".keys");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}-{}.key", source.id, uuid::Uuid::new_v4()));
        write_private(&path, pem.as_bytes())?;
        let cmd = format!(
            "ssh -i '{}' -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
            path.display()
        );
        return Ok(GitAuth {
            env: vec![("GIT_SSH_COMMAND".into(), cmd)],
            key_file: Some(path),
        });
    }
    if let Some(name) = r.strip_prefix("env:") {
        let db2 = db.clone();
        let (vars, _, _) = tokio::task::spawn_blocking(move || {
            crate::service::secret_mask::command_env_blocking(&db2, None)
        })
        .await?;
        let token = vars
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
            .or_else(|| std::env::var(name).ok())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("env var '{name}' is not set (or is a locked secret)")
            })?;
        use base64::Engine as _;
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        return Ok(GitAuth {
            env: vec![
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
                (
                    "GIT_CONFIG_VALUE_0".into(),
                    format!("Authorization: Basic {basic}"),
                ),
            ],
            key_file: None,
        });
    }
    anyhow::bail!("unknown secret_ref '{r}' — use ssh_key:<id> or env:<NAME>")
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        if !bytes.ends_with(b"\n") {
            f.write_all(b"\n")?;
        }
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Only plain transports: https / ssh / scp-style / local paths (tests).
pub fn validate_git_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    if u.is_empty() || u.starts_with('-') {
        return Err("git url is empty or starts with '-'".into());
    }
    let ok = u.starts_with("https://")
        || u.starts_with("ssh://")
        || u.starts_with("file://")
        || u.starts_with('/')
        || (u.contains('@') && u.contains(':') && !u.contains("::"));
    if ok {
        Ok(())
    } else {
        Err("git url must be https://, ssh://, user@host:path, or a local path".into())
    }
}

/// Server-internal git in a Peckboard-managed clone under the data dir — no
/// agent chooses the program or writes the repo, and the sandbox would deny
/// the data dir it lives in.
#[allow(clippy::disallowed_methods)]
async fn git(dir: &Path, args: &[&str], auth: &GitAuth) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(crate::sandbox::GIT_HARDENING_ARGS)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for (k, v) in &auth.env {
        cmd.env(k, v);
    }
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("git {} timed out", args.first().unwrap_or(&"")))??;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            crate::service::redact::mask_credentials(stderr.trim())
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Per-source lock so two syncs/publishes never run git in the same clone
/// at once.
fn source_lock(source_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(source_id.to_string())
        .or_default()
        .clone()
}

/// Bring the team clone up to the remote branch (clone on first use). An
/// empty remote (or a branch that doesn't exist yet) leaves a fresh local
/// branch for the first publish to create.
async fn sync_clone(repo: &Path, source: &SkillSourceRow, auth: &GitAuth) -> anyhow::Result<()> {
    validate_git_url(&source.url).map_err(|e| anyhow::anyhow!(e))?;
    let branch = source.branch.trim();
    if branch.is_empty() || branch.starts_with('-') {
        anyhow::bail!("invalid branch '{branch}'");
    }
    if !repo.join(".git").exists() {
        let parent = repo.parent().expect("repo parent");
        std::fs::create_dir_all(parent)?;
        if repo.exists() {
            std::fs::remove_dir_all(repo)?;
        }
        let repo_s = repo.to_string_lossy().to_string();
        git(
            parent,
            &["clone", "--quiet", "--", &source.url, &repo_s],
            auth,
        )
        .await?;
    } else {
        git(repo, &["remote", "set-url", "origin", &source.url], auth).await?;
    }
    git(repo, &["fetch", "--quiet", "--prune", "origin"], auth).await?;
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let has_remote = git(
        repo,
        &["rev-parse", "--verify", "--quiet", &remote_ref],
        auth,
    )
    .await
    .is_ok();
    if has_remote {
        let origin_branch = format!("origin/{branch}");
        git(
            repo,
            &["checkout", "--quiet", "-B", branch, &origin_branch],
            auth,
        )
        .await?;
        git(repo, &["reset", "--quiet", "--hard", &origin_branch], auth).await?;
        git(repo, &["clean", "-fdq"], auth).await?;
    } else {
        // Empty remote / new branch: start (or stay on) a local branch. An
        // empty clone has no commit to branch from, so point the unborn
        // HEAD at the branch instead.
        if git(repo, &["checkout", "--quiet", "-B", branch], auth)
            .await
            .is_err()
        {
            let head = format!("refs/heads/{branch}");
            git(repo, &["symbolic-ref", "HEAD", &head], auth).await?;
        }
    }
    Ok(())
}

/// Sync a team source's clone and return its catalog.
pub async fn sync_team(
    db: &Db,
    data_dir: &Path,
    source: &SkillSourceRow,
) -> anyhow::Result<RepoIndex> {
    let lock = source_lock(&source.id);
    let _g = lock.lock().await;
    let auth = resolve_auth(db, data_dir, source).await?;
    let repo = repo_dir(data_dir, &source.id);
    sync_clone(&repo, source, &auth).await?;
    Ok(scan_repo(&repo))
}

/// Sync, apply `edit` to the checkout, then commit + push. `edit` returns the
/// commit message, or `None` when it changed nothing. A rejected push is
/// retried once after re-syncing and re-applying the edit.
pub async fn team_write<F>(
    db: &Db,
    data_dir: &Path,
    source: &SkillSourceRow,
    edit: F,
) -> anyhow::Result<Option<String>>
where
    F: Fn(&Path) -> anyhow::Result<Option<String>>,
{
    let lock = source_lock(&source.id);
    let _g = lock.lock().await;
    let auth = resolve_auth(db, data_dir, source).await?;
    let repo = repo_dir(data_dir, &source.id);
    let mut last_err = None;
    for _attempt in 0..2 {
        sync_clone(&repo, source, &auth).await?;
        let Some(message) = edit(&repo)? else {
            return Ok(None);
        };
        write_index(&repo)?;
        git(&repo, &["add", "-A"], &auth).await?;
        if git(&repo, &["diff", "--cached", "--quiet"], &auth)
            .await
            .is_ok()
        {
            return Ok(None);
        }
        git(
            &repo,
            &[
                "-c",
                "user.name=Peckboard",
                "-c",
                "user.email=peckboard@users.noreply.github.com",
                "commit",
                "--quiet",
                "-m",
                &message,
            ],
            &auth,
        )
        .await?;
        let refspec = format!("HEAD:refs/heads/{}", source.branch.trim());
        match git(&repo, &["push", "--quiet", "origin", &refspec], &auth).await {
            Ok(_) => return Ok(Some(message)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("push failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_strips_index_file() {
        assert_eq!(
            base_url("https://raw.githubusercontent.com/PeckBoard/skills/main/index.json"),
            "https://raw.githubusercontent.com/PeckBoard/skills/main/"
        );
    }

    #[test]
    fn git_urls_are_restricted_to_plain_transports() {
        assert!(validate_git_url("https://github.com/o/r.git").is_ok());
        assert!(validate_git_url("git@github.com:o/r.git").is_ok());
        assert!(validate_git_url("/tmp/bare.git").is_ok());
        assert!(validate_git_url("ext::sh -c touch% /tmp/pwned").is_err());
        assert!(validate_git_url("--upload-pack=evil").is_err());
    }

    #[test]
    fn scan_repo_reads_layout_and_write_skill_is_immutable() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = |v: &str, body: &str| SkillDoc {
            name: "alpha".into(),
            description: "first".into(),
            version: v.into(),
            tags: vec!["t".into()],
            groups: vec!["g".into()],
            body: body.into(),
        };
        write_skill(tmp.path(), &doc("1.0.0", "a")).unwrap();
        write_skill(tmp.path(), &doc("1.1.0", "b")).unwrap();
        // Same content is a no-op, different content on an existing version
        // is refused.
        write_skill(tmp.path(), &doc("1.1.0", "b")).unwrap();
        assert!(write_skill(tmp.path(), &doc("1.1.0", "changed")).is_err());
        write_group(
            tmp.path(),
            &IndexGroup {
                slug: "g".into(),
                name: "G".into(),
                description: String::new(),
                skills: vec!["alpha".into()],
            },
        )
        .unwrap();
        let idx = scan_repo(tmp.path());
        assert_eq!(idx.skills.len(), 1);
        assert_eq!(idx.skills[0].versions, vec!["1.1.0", "1.0.0"]);
        assert_eq!(idx.skills[0].latest.as_deref(), Some("1.1.0"));
        assert_eq!(idx.groups[0].skills, vec!["alpha"]);
    }
}
