//! Server-side administration (`peckboard-skills-hub admin …`), run on the
//! hub's host against its data dir. Nothing administrative is reachable
//! over HTTP. SQLite WAL lets these run while the server is up.

use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use peckboard_skill_format::format;
use peckboard_skill_format::guard::{self, NoSecrets};
use serde::Deserialize;

use crate::auth;
use crate::git::Repos;
use crate::store::{GroupRow, Store, commit_version};

/// Env var holding the admin publisher key (base64 Ed25519 public key)
/// that imported skills are owned by.
pub const ADMIN_KEY_ENV: &str = "PECKBOARD_SKILLS_ADMIN_KEY";

/// The admin key: `$PECKBOARD_SKILLS_ADMIN_KEY`, else one generated once
/// and kept (as its 32-byte signing seed, base64, 0600) in
/// `<data-dir>/admin.key`.
pub fn admin_key(data_dir: &Path) -> anyhow::Result<String> {
    if let Ok(k) = std::env::var(ADMIN_KEY_ENV)
        && !k.trim().is_empty()
    {
        let (_, key) = auth::parse_public_key(&k)
            .with_context(|| format!("{ADMIN_KEY_ENV} is not a base64 Ed25519 public key"))?;
        return Ok(key);
    }
    let path = data_dir.join("admin.key");
    let seed: [u8; 32] = match std::fs::read_to_string(&path) {
        Ok(text) => auth::decode_b64(&text)
            .and_then(|b| b.try_into().ok())
            .with_context(|| format!("{} is not a base64 32-byte seed", path.display()))?,
        Err(_) => {
            let seed: [u8; 32] = rand::random();
            write_private(&path, &STANDARD.encode(seed))?;
            seed
        }
    };
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    Ok(STANDARD.encode(sk.verifying_key().to_bytes()))
}

#[cfg(unix)]
fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    std::fs::write(path, text).with_context(|| format!("create {}", path.display()))
}

fn canonical_key(k: &str) -> anyhow::Result<String> {
    auth::parse_public_key(k)
        .map(|(_, c)| c)
        .with_context(|| format!("not a base64 Ed25519 public key: {k}"))
}

pub fn ban(store: &Store, key: &str, banned: bool) -> anyhow::Result<()> {
    let key = canonical_key(key)?;
    store.set_banned(&key, banned)?;
    store.audit(if banned { "ban" } else { "unban" }, Some(&key), "admin");
    Ok(())
}

/// Remove one version, or every version of `name`, from the index and
/// stop serving it. Returns how many versions were taken down.
pub fn takedown(store: &Store, name: &str, version: Option<&str>) -> anyhow::Result<usize> {
    if store.skill_owner(name)?.is_none() {
        anyhow::bail!("no skill named '{name}'");
    }
    if let Some(v) = version
        && store.version(name, v)?.is_none()
    {
        anyhow::bail!("'{name}' has no version {v}");
    }
    let n = store.takedown(name, version)?;
    store.audit(
        "takedown",
        None,
        &format!("{name}@{}", version.unwrap_or("*")),
    );
    Ok(n)
}

/// Hand an original skill to another publisher key — e.g. a skill imported
/// under the admin key back to the PeckBoard install that wrote it, so that
/// install can publish new versions and merge requests into it.
pub fn transfer(store: &Store, name: &str, to_key: &str) -> anyhow::Result<()> {
    let key = canonical_key(to_key)?;
    let from = store
        .skill_owner(name)?
        .with_context(|| format!("no skill named '{name}'"))?;
    if !store.transfer(name, &key)? {
        anyhow::bail!("'{name}' is a fork; only original skills can be transferred");
    }
    store.audit("transfer", Some(&key), &format!("{name} from {from}"));
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub skipped: Vec<String>,
    pub groups: usize,
    pub warnings: Vec<String>,
}

#[derive(Deserialize)]
struct GroupFile {
    slug: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    skills: Vec<String>,
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v
}

/// Import a PeckBoard/skills repo checkout (`skills/<name>/<version>/SKILL.md`
/// and `groups/*.json`), owned by `owner`. Existing versions are left
/// alone; skills owned by someone else are skipped. Group membership listed
/// only in a group file is folded into the member's version metadata.
pub fn import(
    store: &mut Store,
    repos: &Repos,
    dir: &Path,
    owner: &str,
) -> anyhow::Result<ImportReport> {
    if !dir.join("skills").is_dir() && !dir.join("groups").is_dir() {
        anyhow::bail!(
            "{} has neither skills/ nor groups/ — not a skills repo checkout",
            dir.display()
        );
    }
    let mut rep = ImportReport::default();
    for skill_dir in subdirs(&dir.join("skills")) {
        let name = skill_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if name.contains('@') {
            rep.skipped.push(format!("{name}: forks can't be imported"));
            continue;
        }
        // Oldest first, so the repo's history runs in version order.
        let mut labels: Vec<String> = subdirs(&skill_dir)
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .collect();
        format::sort_versions_desc(&mut labels);
        labels.reverse();
        for ver_dir in labels.iter().map(|l| skill_dir.join(l)) {
            let ver = ver_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let label = format!("{name}@{ver}");
            let Ok(text) = std::fs::read_to_string(ver_dir.join("SKILL.md")) else {
                rep.skipped.push(format!("{label}: no SKILL.md"));
                continue;
            };
            let doc = match format::parse_skill(&text) {
                Ok(d) if d.name == name && d.version == ver => d,
                Ok(d) => {
                    rep.skipped.push(format!(
                        "{label}: frontmatter says {}@{}",
                        d.name, d.version
                    ));
                    continue;
                }
                Err(e) => {
                    rep.skipped.push(format!("{label}: {e}"));
                    continue;
                }
            };
            if let Some(o) = store.skill_owner(&name)?
                && o != owner
            {
                rep.skipped
                    .push(format!("{label}: name owned by another key"));
                continue;
            }
            if store.version(&name, &ver)?.is_some() {
                rep.skipped.push(format!("{label}: already present"));
                continue;
            }
            let groups: Vec<String> = doc
                .groups
                .iter()
                .filter(|g| format::valid_name(g))
                .cloned()
                .collect();
            let doc = format::SkillDoc { groups, ..doc };
            for f in guard::check_skill(&doc, &NoSecrets) {
                rep.warnings.push(format!(
                    "{label}: line {} {}: {}",
                    f.line, f.kind, f.excerpt
                ));
            }
            let lock = repos.lock(&name);
            commit_version(
                store,
                repos,
                &lock,
                owner,
                &doc,
                &text,
                None,
                &format!("Import {ver}"),
                None,
            )?;
            rep.imported.push(label);
        }
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("groups"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    for path in files {
        let g: GroupFile = match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|t| serde_json::from_str::<GroupFile>(&t).map_err(anyhow::Error::from))
        {
            Ok(g) if format::valid_name(&g.slug) => g,
            Ok(g) => {
                rep.skipped
                    .push(format!("group '{}': invalid slug", g.slug));
                continue;
            }
            Err(e) => {
                rep.skipped.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let row = GroupRow {
            name: if g.name.is_empty() {
                g.slug.clone()
            } else {
                g.name.clone()
            },
            slug: g.slug.clone(),
            description: g.description.clone(),
            owner_key: owner.to_string(),
        };
        if !store.upsert_group(&row)? {
            rep.skipped
                .push(format!("group '{}': owned by another key", g.slug));
            continue;
        }
        rep.groups += 1;
        for member in &g.skills {
            if store.skill_owner(member)?.as_deref() != Some(owner) {
                continue;
            }
            for v in store.versions_of(member)? {
                if !v.groups.contains(&g.slug) {
                    let mut gs = v.groups.clone();
                    gs.push(g.slug.clone());
                    store.set_version_groups(member, &v.version, &gs)?;
                }
            }
        }
    }
    store.audit(
        "import",
        Some(owner),
        &format!(
            "{}: {} versions, {} groups",
            dir.display(),
            rep.imported.len(),
            rep.groups
        ),
    );
    Ok(rep)
}

/// Human-readable listing for `admin list`.
pub fn list(store: &Store) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut last = String::new();
    for v in store.all_versions()? {
        if v.name != last {
            let owner = store.skill_owner(&v.name)?.unwrap_or_default();
            out.push_str(&format!("{}  owner={owner}\n", v.name));
            last = v.name.clone();
        }
        let flags = match (v.yanked, v.removed) {
            (_, true) => " [taken down]",
            (true, false) => " [yanked]",
            _ => "",
        };
        out.push_str(&format!("  {}  {}{flags}\n", v.version, v.published_at));
    }
    for g in store.groups()? {
        out.push_str(&format!(
            "group {}  \"{}\"  owner={}\n",
            g.slug, g.name, g.owner_key
        ));
    }
    let banned: Vec<String> = {
        let mut st = store
            .conn()
            .prepare("SELECT public_key FROM publishers WHERE banned = 1 ORDER BY public_key")?;
        st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    for k in banned {
        out.push_str(&format!("banned {k}\n"));
    }
    Ok(out)
}
