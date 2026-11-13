//! Skills on a running session: which skills a session has (direct
//! assignments first, then each assigned group's members, at their pinned
//! versions) and how a spawn sees them.
//!
//! Every provider gets a short "Available Skills" index in the system prompt
//! and the `skill_read` MCP tool to load a body. Claude sessions also get the
//! skills as native Claude Code skills: they are written as a per-session
//! plugin (`.claude-plugin/plugin.json` + `skills/<name>/SKILL.md`) under
//! `<CLAUDE_CONFIG_DIR>/peckboard-skills/<session_id>/` and loaded with
//! `--plugin-dir`. A per-session directory (not `<CLAUDE_CONFIG_DIR>/skills/`,
//! which every session of the account would load) keeps one session's
//! skills out of another's; the account config dir (or `~/.claude` for the
//! host login) is the one place under the agent sandbox the CLI can read
//! that is not the user's repo.

use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::db::models::{SkillGroupRow, SkillRow};

/// Name of the per-session Claude plugin the skills are namespaced under
/// (`peckboard-skills:<name>` in the CLI).
pub const CLAUDE_PLUGIN_NAME: &str = "peckboard-skills";
/// Spawn-metadata key carrying the Claude plugin dir to `--plugin-dir`.
pub const PLUGIN_DIR_METADATA_KEY: &str = "skills_plugin_dir";

/// One skill a session has, at the version it uses.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSkill {
    pub skill: SkillRow,
    pub version: String,
    pub body: String,
    /// The group slug that brought it in; `None` for a direct assignment.
    pub via_group: Option<String>,
}

/// The installed skill a group member name means: the group's own source
/// first, then a local skill, then any.
fn pick_member<'a>(
    skills: &'a [SkillRow],
    group: &SkillGroupRow,
    name: &str,
) -> Option<&'a SkillRow> {
    let named: Vec<&SkillRow> = skills.iter().filter(|s| s.name == name).collect();
    named
        .iter()
        .find(|s| group.source_id.is_some() && s.source_id == group.source_id)
        .or_else(|| named.iter().find(|s| s.source_id.is_none()))
        .or_else(|| named.first())
        .copied()
}

/// Resolve a session's skills. Duplicates (by name) keep the first: direct
/// assignments outrank groups. A skill whose version body isn't stored is
/// skipped with a warning.
pub async fn resolve(db: &Db, session_id: &str) -> anyhow::Result<Vec<ResolvedSkill>> {
    let rows = db.list_session_skills(session_id).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let skills = db.list_skills().await?;
    let groups = db.list_skill_groups().await?;
    let members = super::group_members(&groups, &db.list_skill_group_members().await?);
    let subs = db.list_skill_subscriptions().await?;

    let mut picked: Vec<(SkillRow, Option<String>)> = Vec::new();
    let mut push = |s: &SkillRow, via: Option<String>| {
        if !picked.iter().any(|(p, _)| p.name == s.name) {
            picked.push((s.clone(), via));
        }
    };
    for r in rows.iter().filter_map(|r| r.skill_id.as_ref()) {
        if let Some(s) = skills.iter().find(|s| &s.id == r) {
            push(s, None);
        }
    }
    for gid in rows.iter().filter_map(|r| r.group_id.as_ref()) {
        let Some(g) = groups.iter().find(|g| &g.id == gid) else {
            continue;
        };
        for name in members.get(&g.id).into_iter().flatten() {
            if let Some(s) = pick_member(&skills, g, name) {
                push(s, Some(g.slug.clone()));
            }
        }
    }

    let mut out = Vec::new();
    for (s, via) in picked {
        let version = super::effective_version(&s, subs.iter().find(|x| x.skill_id == s.id));
        match db.get_skill_version(&s.id, &version).await? {
            Some(v) => out.push(ResolvedSkill {
                skill: s,
                version,
                body: v.body,
                via_group: via,
            }),
            None => tracing::warn!(
                session_id = %session_id,
                skill = %s.name,
                version = %version,
                "skills: version body not stored — skipped"
            ),
        }
    }
    Ok(out)
}

/// The system-prompt section listing a session's skills (name + one-line
/// description), or `None` when it has none.
pub fn prompt_section(skills: &[ResolvedSkill], native_claude: bool) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut s = String::from(
        "# Available Skills\n\nSkills assigned to this session. Before doing work a skill covers, \
         load its full instructions with the `skill_read` tool (pass its name) and follow them.",
    );
    if native_claude {
        s.push_str(&format!(
            " They are also installed as native skills (`{CLAUDE_PLUGIN_NAME}:<name>`)."
        ));
    }
    s.push('\n');
    for r in skills {
        let desc = r.skill.description.trim();
        let desc = if desc.is_empty() {
            "(no description)"
        } else {
            desc
        };
        s.push_str(&format!(
            "- `{}` (v{}): {}\n",
            r.skill.name, r.version, desc
        ));
    }
    Some(s)
}

/// Where a Claude session's skills plugin goes: the account's config dir,
/// else the host login's `~/.claude`.
pub fn claude_skills_base(account_config_dir: Option<&str>) -> Option<PathBuf> {
    match account_config_dir.filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d)),
        None => std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".claude")),
    }
}

fn safe_session_dir(session_id: &str) -> Option<&str> {
    (!session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(session_id)
}

/// Write the session's skills as a Claude plugin under
/// `<base>/peckboard-skills/<session_id>/`, replacing whatever was there.
/// Returns the plugin dir, or `None` (dir removed) when there are no skills.
pub fn materialize_claude(
    base: &Path,
    session_id: &str,
    skills: &[ResolvedSkill],
) -> std::io::Result<Option<PathBuf>> {
    let Some(sid) = safe_session_dir(session_id) else {
        return Ok(None);
    };
    let dir = base.join(CLAUDE_PLUGIN_NAME).join(sid);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    if skills.is_empty() {
        return Ok(None);
    }
    std::fs::create_dir_all(dir.join(".claude-plugin"))?;
    std::fs::write(
        dir.join(".claude-plugin").join("plugin.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "name": CLAUDE_PLUGIN_NAME,
            "version": "1.0.0",
            "description": "Skills assigned to this Peckboard session",
        }))
        .unwrap_or_default(),
    )?;
    for r in skills {
        // Names are validated ^[a-z0-9][a-z0-9-]*$ — safe as a path segment.
        if !super::format::valid_name(&r.skill.name) {
            continue;
        }
        let sdir = dir.join("skills").join(&r.skill.name);
        std::fs::create_dir_all(&sdir)?;
        let desc = if r.skill.description.trim().is_empty() {
            format!("Peckboard skill {}", r.skill.name)
        } else {
            r.skill.description.trim().to_string()
        };
        let text = format!(
            "---\nname: {}\ndescription: {}\nversion: {}\n---\n{}{}",
            r.skill.name,
            super::format::yaml_scalar(&desc),
            r.version,
            r.body,
            if r.body.ends_with('\n') { "" } else { "\n" }
        );
        std::fs::write(sdir.join("SKILL.md"), text)?;
    }
    Ok(Some(dir))
}

/// What a spawn adds for skills: prompt text, and for Claude the plugin dir.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SpawnSkills {
    pub prompt: Option<String>,
    pub claude_plugin_dir: Option<String>,
}

/// Resolve a session's skills for a spawn of `provider_id` running `model`.
/// Never fails the spawn: any error degrades to "no skills" with a warning.
pub async fn for_spawn(db: &Db, session_id: &str, provider_id: &str, model: &str) -> SpawnSkills {
    let skills = match resolve(db, session_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "skills: resolve failed: {e}");
            return SpawnSkills::default();
        }
    };
    let mut claude_plugin_dir = None;
    if provider_id == "claude" {
        let (_, account_id) = crate::provider::registry::split_model_account(model);
        let account_dir = match account_id {
            Some(id) => db
                .get_claude_account(id)
                .await
                .ok()
                .flatten()
                .and_then(|a| a.config_dir),
            None => None,
        };
        if let Some(base) = claude_skills_base(account_dir.as_deref()) {
            let sid = session_id.to_string();
            let sk = skills.clone();
            match tokio::task::spawn_blocking(move || materialize_claude(&base, &sid, &sk)).await {
                Ok(Ok(dir)) => claude_plugin_dir = dir.map(|d| d.to_string_lossy().to_string()),
                Ok(Err(e)) => {
                    tracing::warn!(session_id = %session_id, "skills: claude materialise failed: {e}")
                }
                Err(e) => tracing::warn!(session_id = %session_id, "skills: {e}"),
            }
        }
    }
    SpawnSkills {
        prompt: prompt_section(&skills, claude_plugin_dir.is_some()),
        claude_plugin_dir,
    }
}

/// `skill_read`: the body of a skill assigned to `session_id`.
pub async fn read_for_session(
    db: &Db,
    session_id: &str,
    name: &str,
) -> anyhow::Result<ResolvedSkill> {
    let skills = resolve(db, session_id).await?;
    if let Some(r) = skills.iter().find(|r| r.skill.name == name) {
        return Ok(r.clone());
    }
    let names: Vec<&str> = skills.iter().map(|r| r.skill.name.as_str()).collect();
    if names.is_empty() {
        anyhow::bail!("no skills are assigned to this session");
    }
    anyhow::bail!(
        "skill '{name}' is not assigned to this session — available: {}",
        names.join(", ")
    )
}
