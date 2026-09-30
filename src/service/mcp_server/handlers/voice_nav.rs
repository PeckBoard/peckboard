//! `show_view` — the voice assistant drives what page the user is looking
//! at. It resolves a target (project / session / card / folder by id or by
//! spoken name, or a top-level page) server-side, then broadcasts a
//! `voice-navigate` WS event on the voice session's own stream. Only the
//! browser(s) with the voice panel open are subscribed to that stream, and
//! the WS layer already gates subscription to the session's owner, so the
//! jump lands exactly where the user is talking.

use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::db::models::Session;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::voice_relay::VOICE_EXPERT_KIND;
use crate::ws::broadcaster::WsEvent;

/// WS event type the browser turns into a navigation.
pub(crate) const VOICE_NAVIGATE_EVENT: &str = "voice-navigate";

/// Top-level pages `show_view` may open with `target: "page"`.
const PAGES: &[&str] = &[
    "sessions",
    "projects",
    "folders",
    "settings",
    "reports",
    "repeating_tasks",
    "usage",
    "agents",
];

/// Most candidates listed back when a name is ambiguous or unmatched.
const MAX_CANDIDATES: usize = 6;

#[derive(Clone)]
struct Candidate {
    kind: &'static str,
    id: String,
    name: String,
    /// Project a card belongs to (the board to open it on).
    project_id: Option<String>,
    score: u32,
}

impl Candidate {
    fn describe(&self) -> Value {
        json!({ "kind": self.kind, "id": self.id, "name": self.name })
    }
}

/// Lowercase, alphanumerics only, words single-space separated — so
/// "Stashify-Dev" and "stashify dev" compare equal.
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// How well `name` matches spoken `query`; 0 = no match. Exact beats
/// prefix beats substring beats "every query word appears somewhere".
fn score(query: &str, name: &str) -> u32 {
    let (q, n) = (normalize(query), normalize(name));
    if q.is_empty() || n.is_empty() {
        return 0;
    }
    let (qc, nc) = (q.replace(' ', ""), n.replace(' ', ""));
    if q == n || qc == nc {
        100
    } else if n.starts_with(&q) || nc.starts_with(&qc) {
        80
    } else if n.contains(&q) || nc.contains(&qc) {
        60
    } else if q.split(' ').all(|w| n.contains(w)) {
        40
    } else {
        0
    }
}

impl McpToolRegistry {
    /// `show_view` — voice-session-only; see the module docs.
    pub(crate) async fn handle_show_view(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let str_arg = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let target = str_arg("target").unwrap_or_else(|| "auto".into());
        let id = str_arg("id");
        let name = str_arg("name");
        tracing::info!(session_id = %ctx.session_id, %target, ?id, ?name, "MCP tool: show_view");

        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;
        if caller.expert_kind.as_deref() != Some(VOICE_EXPERT_KIND) {
            anyhow::bail!("show_view is only available to the voice assistant session");
        }
        let user_id = caller
            .user_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("voice session has no owner"))?;

        if target == "page" {
            let page = name
                .as_deref()
                .or(id.as_deref())
                .map(|p| normalize(p).replace(' ', "_"))
                .ok_or_else(|| anyhow::anyhow!("show_view target 'page' requires 'name'"))?;
            let page = PAGES
                .iter()
                .find(|p| **p == page || p.trim_end_matches('s') == page)
                .ok_or_else(|| {
                    anyhow::anyhow!("unknown page '{page}'; one of: {}", PAGES.join(", "))
                })?;
            return Ok(self.navigate(ctx, json!({ "target": "page", "page": page }), page));
        }

        let kinds: &[&str] = match target.as_str() {
            "auto" => &["project", "session", "folder"],
            "project" => &["project"],
            "session" => &["session"],
            "card" => &["card"],
            "folder" => &["folder"],
            other => anyhow::bail!(
                "unknown target '{other}'; use project, session, card, folder, page, or auto"
            ),
        };
        if id.is_none() && name.is_none() {
            anyhow::bail!("show_view requires 'id' or 'name'");
        }

        let candidates = self
            .view_candidates(
                ctx,
                &caller,
                &user_id,
                kinds,
                id.as_deref(),
                name.as_deref(),
            )
            .await?;
        let top = candidates.first().map(|c| c.score).unwrap_or(0);
        let best: Vec<&Candidate> = candidates.iter().filter(|c| c.score == top).collect();

        if best.len() == 1 {
            let c = best[0];
            let payload = match c.kind {
                "card" => json!({
                    "target": "card",
                    "id": c.id,
                    "project_id": c.project_id,
                    "name": c.name,
                }),
                kind => json!({ "target": kind, "id": c.id, "name": c.name }),
            };
            return Ok(self.navigate(ctx, payload, &format!("{} {}", c.kind, c.name)));
        }

        let listed: Vec<Value> = candidates
            .iter()
            .take(MAX_CANDIDATES)
            .map(Candidate::describe)
            .collect();
        Ok(json!({
            "status": if best.is_empty() { "not_found" } else { "ambiguous" },
            "message": if best.is_empty() {
                "Nothing matched; nothing was opened. Ask the user what they meant.".to_string()
            } else {
                "Several matches; nothing was opened. Ask the user which one, then call show_view again with its id.".to_string()
            },
            "candidates": listed,
        }))
    }

    fn navigate(&self, ctx: &ToolCallContext, data: Value, what: &str) -> Value {
        ctx.broadcaster.broadcast(WsEvent {
            event_type: VOICE_NAVIGATE_EVENT.into(),
            session_id: ctx.session_id.clone(),
            data: data.clone(),
        });
        json!({
            "status": "ok",
            "message": format!("Opened {what} on the user's screen."),
            "opened": data,
        })
    }

    /// Everything the voice session's user may open, scored against the
    /// query and sorted best-first; zero scores dropped. An exact id
    /// always wins.
    async fn view_candidates(
        &self,
        ctx: &ToolCallContext,
        caller: &Session,
        user_id: &str,
        kinds: &[&str],
        id: Option<&str>,
        name: Option<&str>,
    ) -> anyhow::Result<Vec<Candidate>> {
        let rate = |cand_id: &str, cand_name: &str| -> u32 {
            if id == Some(cand_id) {
                1000
            } else {
                name.map(|n| score(n, cand_name)).unwrap_or(0)
            }
        };
        let mut out = Vec::new();

        if kinds.contains(&"project") || kinds.contains(&"card") {
            let projects = ctx.db.list_projects().await?;
            if kinds.contains(&"project") {
                for p in &projects {
                    out.push(Candidate {
                        kind: "project",
                        id: p.id.clone(),
                        name: p.name.clone(),
                        project_id: None,
                        score: rate(&p.id, &p.name),
                    });
                }
            }
            if kinds.contains(&"card") {
                for p in &projects {
                    for c in ctx.db.list_cards_by_project(&p.id).await? {
                        let score = rate(&c.id, &c.title);
                        out.push(Candidate {
                            kind: "card",
                            id: c.id,
                            name: c.title,
                            project_id: Some(c.project_id),
                            score,
                        });
                    }
                }
            }
        }

        if kinds.contains(&"session") {
            for s in ctx.db.list_sessions().await? {
                // Never offer the voice session itself, nor sessions this
                // user may not open.
                if s.id == caller.id
                    || !crate::auth::access::may_access_session(
                        false,
                        user_id,
                        s.user_id.as_deref(),
                        s.project_id.as_deref(),
                    )
                {
                    continue;
                }
                // A plain chat outranks a worker of the same name.
                let bonus = u32::from(!s.is_worker);
                let score = rate(&s.id, &s.name);
                out.push(Candidate {
                    kind: "session",
                    id: s.id,
                    name: s.name,
                    project_id: None,
                    score: if score > 0 { score + bonus } else { 0 },
                });
            }
        }

        if kinds.contains(&"folder") {
            for f in ctx.db.list_folders().await? {
                let score = rate(&f.id, &f.name);
                out.push(Candidate {
                    kind: "folder",
                    id: f.id,
                    name: f.name,
                    project_id: None,
                    score,
                });
            }
        }

        out.retain(|c| c.score > 0);
        out.sort_by(|a, b| b.score.cmp(&a.score));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewProject, NewSession};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
    use crate::ws::broadcaster::Broadcaster;

    async fn seed_session(db: &Db, id: &str, name: &str, kind: Option<&str>, user: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: id.into(),
            name: name.into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            is_expert: kind.is_some(),
            expert_kind: kind.map(str::to_string),
            user_id: Some(user.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    async fn seed_project(db: &Db, id: &str, name: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_project(NewProject {
            id: id.into(),
            name: name.into(),
            context: String::new(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "default".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: now.clone(),
            worktree_isolation: false,
            last_accessed_at: now,
            budget_usd_cents: None,
            budget_period: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn show_view_resolves_names_and_broadcasts_navigation() {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "infra".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        seed_session(&db, "voice", "Voice assistant", Some("voice"), "u1").await;
        seed_session(&db, "chat", "chat", None, "u1").await;
        seed_session(&db, "s-dev", "Stashify dev", None, "u1").await;
        seed_session(&db, "s-other", "Stashify secrets", None, "u2").await;
        seed_project(&db, "p1", "Stashify").await;
        seed_project(&db, "p2", "Web app").await;
        seed_project(&db, "p3", "Web api").await;

        let broadcaster = Broadcaster::new();
        let mut rx = broadcaster.subscribe_all();
        let ctx = |sid: &str| ToolCallContext {
            session_id: sid.into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: Arc::new(db.clone()),
            broadcaster: broadcaster.clone(),
            provider_registry: None,
            data_dir: None,
            device_registry: None,
            background: None,
        };
        let reg = McpToolRegistry::new();
        let call = |args: serde_json::Value, sid: &'static str| {
            let reg = &reg;
            let c = ctx(sid);
            async move { reg.handle_tool_call("show_view", args, &c).await }
        };

        // Voice-only.
        let err = call(serde_json::json!({"name": "stashify"}), "chat")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("voice"), "{err}");

        // Exact project name beats the "Stashify dev" session prefix match.
        let ok = call(serde_json::json!({"name": "stashify"}), "voice")
            .await
            .unwrap();
        assert_eq!(ok["opened"]["target"], "project");
        assert_eq!(ok["opened"]["id"], "p1");
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.event_type, "voice-navigate");
        assert_eq!(ev.session_id, "voice");
        assert_eq!(ev.data["id"], "p1");

        // Session target: fuzzy, and another user's session never matches.
        let ok = call(
            serde_json::json!({"target": "session", "name": "stashify"}),
            "voice",
        )
        .await
        .unwrap();
        assert_eq!(ok["opened"]["id"], "s-dev");

        // Folder by name.
        let ok = call(serde_json::json!({"name": "infra"}), "voice")
            .await
            .unwrap();
        assert_eq!(ok["opened"]["target"], "folder");
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        // Ambiguous: candidates, no navigation.
        let amb = call(
            serde_json::json!({"target": "project", "name": "web"}),
            "voice",
        )
        .await
        .unwrap();
        assert_eq!(amb["status"], "ambiguous");
        assert_eq!(amb["candidates"].as_array().unwrap().len(), 2);
        assert!(rx.try_recv().is_err());

        // Page.
        let ok = call(
            serde_json::json!({"target": "page", "name": "Settings"}),
            "voice",
        )
        .await
        .unwrap();
        assert_eq!(ok["opened"]["page"], "settings");
    }
}
