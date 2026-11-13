//! Skills MCP tools — the agent-side door to `service::skills` (the REST
//! routes in `routes/skills.rs` are the other; both share the service's
//! validation, ownership and proprietary-content guard). The caller is the
//! session's owner ([`Actor::for_session`]): shared state (groups, sources)
//! needs an admin owner, a skill needs an admin or the user who added it.
//!
//! `skill_read` is what a session uses to load an assigned skill's body —
//! the system prompt only lists names and descriptions
//! (`service::skills::session`).

use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::{McpToolDef, ToolCallContext};
use crate::service::skills::{self, Actor, SkillError};

/// Every skills tool name, in definition order.
pub(crate) const SKILL_TOOL_NAMES: &[&str] = &[
    "skill_list",
    "skill_search",
    "skill_get",
    "skill_create",
    "skill_update",
    "skill_delete",
    "skill_group_list",
    "skill_group_create",
    "skill_group_update",
    "skill_group_delete",
    "skill_subscribe",
    "skill_unsubscribe",
    "skill_set_version",
    "session_set_skills",
    "session_get_skills",
    "skill_publish",
    "skill_source_list",
    "skill_source_sync",
    "team_memory_list",
    "team_memory_get",
    "team_memory_add",
    "team_memory_update",
    "team_memory_remove",
    "skill_read",
];

fn e(err: SkillError) -> anyhow::Error {
    anyhow::anyhow!(err.to_string())
}

fn s<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn req<'a>(args: &'a Value, key: &str, tool: &str) -> anyhow::Result<&'a str> {
    s(args, key).ok_or_else(|| anyhow::anyhow!("{tool} requires '{key}'"))
}

fn strings(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(|v| v.as_array()).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()
    })
}

fn data_dir(ctx: &ToolCallContext) -> anyhow::Result<std::path::PathBuf> {
    ctx.data_dir
        .clone()
        .or_else(|| ctx.db.data_dir().map(|p| p.to_path_buf()))
        .ok_or_else(|| anyhow::anyhow!("skills: no data dir on this server"))
}

/// A skill reference: `id`, else `name`.
fn skill_ref<'a>(args: &'a Value, tool: &str) -> anyhow::Result<&'a str> {
    s(args, "id")
        .or_else(|| s(args, "name"))
        .ok_or_else(|| anyhow::anyhow!("{tool} requires 'id' or 'name'"))
}

/// A source reference: `<prefix>source_id`, else `<prefix>source_name`, else
/// (when `default_team`) the only team source.
async fn source_ref(
    ctx: &ToolCallContext,
    args: &Value,
    prefix: &str,
    default_team: bool,
) -> anyhow::Result<String> {
    if let Some(id) = s(args, &format!("{prefix}source_id")) {
        return Ok(id.to_string());
    }
    if let Some(name) = s(args, &format!("{prefix}source_name")) {
        return ctx
            .db
            .get_skill_source_by_name(name)
            .await?
            .map(|r| r.id)
            .ok_or_else(|| anyhow::anyhow!("no skill source named '{name}'"));
    }
    if default_team {
        let teams: Vec<_> = ctx
            .db
            .list_skill_sources()
            .await?
            .into_iter()
            .filter(|r| r.kind == "team")
            .collect();
        if teams.len() == 1 {
            return Ok(teams[0].id.clone());
        }
        anyhow::bail!(
            "pass source_id or source_name ({} team sources configured)",
            teams.len()
        );
    }
    anyhow::bail!("pass {prefix}source_id or {prefix}source_name")
}

async fn group_ref(ctx: &ToolCallContext, args: &Value, tool: &str) -> anyhow::Result<String> {
    if let Some(id) = s(args, "id") {
        return Ok(id.to_string());
    }
    let slug = req(args, "slug", tool)?;
    ctx.db
        .find_skill_group(None, slug)
        .await?
        .map(|g| g.id)
        .ok_or_else(|| anyhow::anyhow!("no local skill group '{slug}'"))
}

/// The session a `session_*_skills` call targets: the caller's own, or one
/// owned by the same user (admins: any).
async fn target_session(
    ctx: &ToolCallContext,
    args: &Value,
    actor: &Actor,
) -> anyhow::Result<String> {
    let Some(target) = s(args, "session_id").filter(|t| *t != ctx.session_id) else {
        return Ok(ctx.session_id.clone());
    };
    let session = ctx
        .db
        .get_session(target)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session not found: {target}"))?;
    if actor.is_admin || (actor.user_id.is_some() && session.user_id == actor.user_id) {
        Ok(session.id)
    } else {
        anyhow::bail!("session {target} belongs to another user")
    }
}

impl McpToolRegistry {
    pub(crate) async fn handle_skill_tool(
        &self,
        tool: &str,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, tool = %tool, "MCP tool: skills");
        let db = &ctx.db;
        match tool {
            "skill_read" => {
                let name = req(&args, "name", tool)?;
                let r = skills::session::read_for_session(db, &ctx.session_id, name).await?;
                Ok(json!({
                    "name": r.skill.name,
                    "version": r.version,
                    "description": r.skill.description,
                    "body": r.body,
                }))
            }
            "skill_list" => {
                let items = skills::list(db, s(&args, "q"), s(&args, "scope"), s(&args, "group"))
                    .await
                    .map_err(e)?;
                let items: Vec<Value> = items.iter().map(skills::compact).collect();
                Ok(json!({ "skills": items, "count": items.len() }))
            }
            "skill_search" => {
                let mut out =
                    skills::search(db, &data_dir(ctx)?, s(&args, "q"), s(&args, "source_id"))
                        .await
                        .map_err(e)?;
                if let Some(items) = out.get_mut("items").and_then(|v| v.as_array_mut()) {
                    for it in items.iter_mut() {
                        if let Some(o) = it.as_object_mut() {
                            o.remove("updated_at");
                        }
                    }
                }
                Ok(out)
            }
            "skill_get" => {
                let skill = skills::find_ref(db, skill_ref(&args, tool)?)
                    .await
                    .map_err(e)?;
                match s(&args, "version") {
                    Some(v) => skills::get_version(db, &skill.id, v).await.map_err(e),
                    None => Ok(serde_json::to_value(
                        skills::get(db, &skill.id).await.map_err(e)?,
                    )?),
                }
            }
            "skill_create" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let input = skills::CreateSkill {
                    name: req(&args, "name", tool)?.to_string(),
                    description: s(&args, "description").unwrap_or("").to_string(),
                    tags: strings(&args, "tags").unwrap_or_default(),
                    groups: strings(&args, "groups").unwrap_or_default(),
                    body: args
                        .get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    version: s(&args, "version").map(str::to_string),
                };
                let v = skills::create(db, &actor, input).await.map_err(e)?;
                Ok(json!({ "status": "ok", "skill": skills::compact(&v) }))
            }
            "skill_update" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let skill = skills::find_ref(db, skill_ref(&args, tool)?)
                    .await
                    .map_err(e)?;
                let input = skills::UpdateSkill {
                    description: args
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    tags: strings(&args, "tags"),
                    groups: strings(&args, "groups"),
                    body: args
                        .get("body")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    bump: s(&args, "bump").map(str::to_string),
                };
                let v = skills::update(db, &actor, &skill.id, input)
                    .await
                    .map_err(e)?;
                Ok(json!({ "status": "ok", "skill": skills::compact(&v) }))
            }
            "skill_delete" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let skill = skills::find_ref(db, skill_ref(&args, tool)?)
                    .await
                    .map_err(e)?;
                skills::delete(db, &actor, &skill.id).await.map_err(e)?;
                Ok(json!({ "status": "ok", "deleted": skill.name }))
            }
            "skill_group_list" => {
                let groups = skills::list_groups(db).await.map_err(e)?;
                Ok(json!({ "groups": groups }))
            }
            "skill_group_create" | "skill_group_update" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let existing = if tool == "skill_group_update" {
                    Some(group_ref(ctx, &args, tool).await?)
                } else {
                    None
                };
                let current = match &existing {
                    Some(id) => skills::list_groups(db)
                        .await
                        .map_err(e)?
                        .into_iter()
                        .find(|g| &g.id == id),
                    None => None,
                };
                let input = skills::GroupInput {
                    slug: s(&args, "new_slug")
                        .or_else(|| s(&args, "slug"))
                        .map(str::to_string)
                        .or_else(|| current.as_ref().map(|g| g.slug.clone()))
                        .ok_or_else(|| anyhow::anyhow!("{tool} requires 'slug'"))?,
                    name: s(&args, "name")
                        .map(str::to_string)
                        .or_else(|| current.as_ref().map(|g| g.name.clone()))
                        .unwrap_or_default(),
                    description: args
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .or_else(|| current.as_ref().map(|g| g.description.clone()))
                        .unwrap_or_default(),
                    skills: strings(&args, "skills")
                        .or_else(|| current.as_ref().map(|g| g.skills.clone()))
                        .unwrap_or_default(),
                };
                let g = match existing {
                    Some(id) => skills::update_group(db, &actor, &id, input).await,
                    None => skills::create_group(db, &actor, input).await,
                }
                .map_err(e)?;
                Ok(json!({ "status": "ok", "group": g }))
            }
            "skill_group_delete" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let id = group_ref(ctx, &args, tool).await?;
                skills::delete_group(db, &actor, &id).await.map_err(e)?;
                Ok(json!({ "status": "ok" }))
            }
            "skill_subscribe" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let source = source_ref(ctx, &args, "", false).await?;
                let v = skills::subscribe(
                    db,
                    &data_dir(ctx)?,
                    &actor,
                    &source,
                    req(&args, "name", tool)?,
                    s(&args, "version").map(str::to_string),
                )
                .await
                .map_err(e)?;
                Ok(json!({ "status": "ok", "skill": skills::compact(&v) }))
            }
            "skill_unsubscribe" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let skill = skills::find_ref(db, skill_ref(&args, tool)?)
                    .await
                    .map_err(e)?;
                skills::unsubscribe(db, &actor, &skill.id)
                    .await
                    .map_err(e)?;
                Ok(json!({ "status": "ok", "unsubscribed": skill.name }))
            }
            "skill_set_version" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let skill = skills::find_ref(db, skill_ref(&args, tool)?)
                    .await
                    .map_err(e)?;
                let v = skills::set_subscription(
                    db,
                    &data_dir(ctx)?,
                    &actor,
                    &skill.id,
                    s(&args, "version").map(str::to_string),
                )
                .await
                .map_err(e)?;
                Ok(json!({ "status": "ok", "skill": skills::compact(&v) }))
            }
            "session_get_skills" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let sid = target_session(ctx, &args, &actor).await?;
                skills::session_skills(db, &sid).await.map_err(e)
            }
            "session_set_skills" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let sid = target_session(ctx, &args, &actor).await?;
                // An omitted list keeps the session's current one.
                let rows = db.list_session_skills(&sid).await?;
                let skills_arg = strings(&args, "skills")
                    .unwrap_or_else(|| rows.iter().filter_map(|r| r.skill_id.clone()).collect());
                let groups_arg = strings(&args, "groups")
                    .unwrap_or_else(|| rows.iter().filter_map(|r| r.group_id.clone()).collect());
                let out = skills::set_session_skills(db, &sid, skills_arg, groups_arg)
                    .await
                    .map_err(e)?;
                Ok(json!({
                    "status": "ok",
                    "session_id": sid,
                    "assignment": out,
                    "note": "Applies from this session's next agent spawn (e.g. after a clear or restart).",
                }))
            }
            "skill_publish" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let skill = skills::find_ref(db, skill_ref(&args, tool)?)
                    .await
                    .map_err(e)?;
                let target = source_ref(ctx, &args, "target_", false).await?;
                skills::publish(db, &data_dir(ctx)?, &actor, &skill.id, &target)
                    .await
                    .map_err(e)
            }
            "skill_source_list" => {
                let sources = skills::list_sources(db).await.map_err(e)?;
                Ok(json!({ "sources": sources }))
            }
            "skill_source_sync" => {
                let actor = Actor::for_session(db, &ctx.session_id).await?;
                let source = source_ref(ctx, &args, "", false).await?;
                let v = skills::sync_source(db, &data_dir(ctx)?, &actor, &source)
                    .await
                    .map_err(e)?;
                Ok(json!({ "status": "ok", "source": v }))
            }
            "team_memory_list" => {
                let source = source_ref(ctx, &args, "", true).await?;
                let items = skills::list_memory(db, &data_dir(ctx)?, &source)
                    .await
                    .map_err(e)?;
                let items: Vec<Value> = items
                    .iter()
                    .map(|m| json!({ "slug": m.slug, "title": m.title, "updated": m.updated }))
                    .collect();
                Ok(json!({ "source_id": source, "memory": items }))
            }
            "team_memory_get" => {
                let source = source_ref(ctx, &args, "", true).await?;
                let m = skills::get_memory(db, &data_dir(ctx)?, &source, req(&args, "slug", tool)?)
                    .await
                    .map_err(e)?;
                Ok(serde_json::to_value(m)?)
            }
            "team_memory_add" | "team_memory_update" => {
                let source = source_ref(ctx, &args, "", true).await?;
                let body = args
                    .get("body")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("{tool} requires 'body'"))?
                    .to_string();
                let m = skills::write_memory(
                    db,
                    &data_dir(ctx)?,
                    &source,
                    req(&args, "slug", tool)?,
                    s(&args, "title").map(str::to_string),
                    body,
                    tool == "team_memory_add",
                )
                .await
                .map_err(e)?;
                Ok(
                    json!({ "status": "ok", "slug": m.slug, "title": m.title, "updated": m.updated }),
                )
            }
            "team_memory_remove" => {
                let source = source_ref(ctx, &args, "", true).await?;
                let slug = req(&args, "slug", tool)?;
                skills::remove_memory(db, &data_dir(ctx)?, &source, slug)
                    .await
                    .map_err(e)?;
                Ok(json!({ "status": "ok", "removed": slug }))
            }
            _ => anyhow::bail!("unknown tool: {tool}"),
        }
    }
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

fn def(name: &str, description: &str, schema: Value) -> McpToolDef {
    McpToolDef {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
    }
}

/// The skills tools' `tools/list` entries (appended to
/// `schemas::tool_definitions`).
pub(crate) fn tool_definitions() -> Vec<McpToolDef> {
    let id_or_name = json!({
        "id": { "type": "string", "description": "Skill id." },
        "name": { "type": "string", "description": "Skill name, when the id is unknown (local skills win)." }
    });
    let with = |extra: Value| {
        let mut p = id_or_name.clone();
        if let (Some(p), Some(x)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                p.insert(k.clone(), v.clone());
            }
        }
        p
    };
    let strs =
        |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
    let source_props = json!({
        "source_id": { "type": "string" },
        "source_name": { "type": "string", "description": "Source name, when the id is unknown." }
    });
    let memory_props = |extra: Value| {
        let mut p = json!({
            "source_id": { "type": "string", "description": "Team source id (omit when only one team source exists)." },
            "source_name": { "type": "string" },
            "slug": { "type": "string", "description": "kebab-case note id." }
        });
        if let (Some(p), Some(x)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                p.insert(k.clone(), v.clone());
            }
        }
        p
    };
    let group_props = json!({
        "slug": { "type": "string", "description": "kebab-case group id." },
        "name": { "type": "string" },
        "description": { "type": "string" },
        "skills": strs("Member skill names (replaces the list).")
    });
    vec![
        def(
            "skill_list",
            "List installed skills (local + subscribed): name, scope, version, pin, groups. Filter by `q` (name/description/tag text), `scope` (local|team|public) or `group` (slug).",
            obj(
                json!({ "q": { "type": "string" }, "scope": { "type": "string", "enum": ["local", "team", "public"] }, "group": { "type": "string" } }),
                &[],
            ),
        ),
        def(
            "skill_search",
            "Browse the skill catalog: local skills plus every source's index (team repos, the public Peckboard catalog), with each skill's available versions and whether it is installed.",
            obj(
                json!({ "q": { "type": "string" }, "source_id": { "type": "string", "description": "Only this source ('local' for local skills)." } }),
                &[],
            ),
        ),
        def(
            "skill_get",
            "Read one installed skill with its body (at the version it is used at), or a specific stored `version`.",
            obj(with(json!({ "version": { "type": "string" } })), &[]),
        ),
        def(
            "skill_create",
            "Create a local skill (SKILL.md: name, description, body). `name` is kebab-case; version defaults to 1.0.0. `groups` (slugs) are created if missing.",
            obj(
                json!({
                    "name": { "type": "string", "description": "^[a-z0-9][a-z0-9-]{0,63}$" },
                    "description": { "type": "string", "description": "One line: when to use it." },
                    "body": { "type": "string", "description": "Markdown instructions." },
                    "tags": strs("Tags."),
                    "groups": strs("Group slugs."),
                    "version": { "type": "string", "description": "Semver; default 1.0.0." }
                }),
                &["name", "body"],
            ),
        ),
        def(
            "skill_update",
            "Edit a local skill. Content changes store a NEW immutable version (`bump`: patch (default) | minor | major); a groups-only change re-tags without one.",
            obj(
                with(json!({
                    "description": { "type": "string" },
                    "body": { "type": "string" },
                    "tags": strs("Replaces the tags."),
                    "groups": strs("Replaces the local groups."),
                    "bump": { "type": "string", "enum": ["patch", "minor", "major"] }
                })),
                &[],
            ),
        ),
        def(
            "skill_delete",
            "Delete an installed skill, its stored versions and every session assignment of it.",
            obj(id_or_name.clone(), &[]),
        ),
        def(
            "skill_group_list",
            "List skill groups (local and source-managed) with their member skill names.",
            obj(json!({}), &[]),
        ),
        def(
            "skill_group_create",
            "Create a local skill group (admin owner only).",
            obj(group_props.clone(), &["slug"]),
        ),
        def(
            "skill_group_update",
            "Edit a local skill group by `id` or `slug` (admin owner only). Omitted fields keep their value; `new_slug` renames.",
            obj(
                {
                    let mut p = group_props.clone();
                    p["id"] = json!({ "type": "string" });
                    p["new_slug"] = json!({ "type": "string" });
                    p
                },
                &[],
            ),
        ),
        def(
            "skill_group_delete",
            "Delete a local skill group by `id` or `slug` (admin owner only). Its skills stay installed.",
            obj(
                json!({ "id": { "type": "string" }, "slug": { "type": "string" } }),
                &[],
            ),
        ),
        def(
            "skill_subscribe",
            "Install a skill from a source (by source_id or source_name). `version` pins it; omitted follows the latest on every sync.",
            obj(
                {
                    let mut p = source_props.clone();
                    p["name"] = json!({ "type": "string" });
                    p["version"] = json!({ "type": "string" });
                    p
                },
                &["name"],
            ),
        ),
        def(
            "skill_unsubscribe",
            "Uninstall a subscribed (team/public) skill.",
            obj(id_or_name.clone(), &[]),
        ),
        def(
            "skill_set_version",
            "Pin a subscribed skill to `version`, or omit `version` to follow the latest.",
            obj(with(json!({ "version": { "type": "string" } })), &[]),
        ),
        def(
            "session_set_skills",
            "Set the skills (ids or names) and skill groups (ids or slugs) assigned to a session — this one by default. An omitted list is kept. Takes effect from the session's next agent spawn.",
            obj(
                json!({
                    "session_id": { "type": "string" },
                    "skills": strs("Skill ids or names."),
                    "groups": strs("Group ids or slugs.")
                }),
                &[],
            ),
        ),
        def(
            "session_get_skills",
            "The skills and groups assigned to a session (this one by default), and the resolved skill list with the version each is used at.",
            obj(json!({ "session_id": { "type": "string" } }), &[]),
        ),
        def(
            "skill_publish",
            "Publish a skill's newest version to a source (target_source_id or target_source_name): a team repo gets a commit + push; the public catalog gets a prepared file + PR instructions. Refused when the proprietary-content check finds secrets, emails, internal hosts/IPs or home paths.",
            obj(
                with(json!({
                    "target_source_id": { "type": "string" },
                    "target_source_name": { "type": "string" }
                })),
                &[],
            ),
        ),
        def(
            "skill_source_list",
            "List skill sources (public catalog, team git repos) with their last sync time / error.",
            obj(json!({}), &[]),
        ),
        def(
            "skill_source_sync",
            "Pull a source's catalog and move its subscribed skills to their latest versions (pinned ones stay). Admin owner only.",
            obj(source_props.clone(), &[]),
        ),
        def(
            "team_memory_list",
            "List the team's shared memory notes (memory/<slug>.md in a team source).",
            obj(
                json!({ "source_id": { "type": "string" }, "source_name": { "type": "string" } }),
                &[],
            ),
        ),
        def(
            "team_memory_get",
            "Read one team memory note.",
            obj(memory_props(json!({})), &["slug"]),
        ),
        def(
            "team_memory_add",
            "Add a team memory note (committed + pushed to the team repo). Refused when the proprietary-content check finds secrets or internal details.",
            obj(
                memory_props(
                    json!({ "title": { "type": "string" }, "body": { "type": "string" } }),
                ),
                &["slug", "body"],
            ),
        ),
        def(
            "team_memory_update",
            "Replace a team memory note's body (and optionally title). Guarded like team_memory_add.",
            obj(
                memory_props(
                    json!({ "title": { "type": "string" }, "body": { "type": "string" } }),
                ),
                &["slug", "body"],
            ),
        ),
        def(
            "team_memory_remove",
            "Delete a team memory note (committed + pushed).",
            obj(memory_props(json!({})), &["slug"]),
        ),
        def(
            "skill_read",
            "Load the full instructions of a skill assigned to this session (listed under Available Skills in your system prompt). Read it before doing the work it covers.",
            obj(json!({ "name": { "type": "string" } }), &["name"]),
        ),
    ]
}
