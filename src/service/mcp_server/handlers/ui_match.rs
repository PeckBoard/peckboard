//! Name matching shared by the tools that drive the user's screen
//! (`ui_open`, the voice assistant's `show_view`) and the view tools: turn a
//! spoken / typed name into the project, session, card, folder, or saved
//! view the caller's user may open, or a short candidate list when it is
//! ambiguous or unmatched.

use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

/// Most candidates listed back when a name is ambiguous or unmatched.
pub(super) const MAX_CANDIDATES: usize = 6;

#[derive(Clone)]
pub(super) struct Candidate {
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    /// Project a card belongs to (the board to open it on).
    pub project_id: Option<String>,
    pub score: u32,
}

impl Candidate {
    pub fn describe(&self) -> Value {
        json!({ "kind": self.kind, "id": self.id, "name": self.name })
    }
}

/// Who is asking: the calling session and its owner. Sessions are offered
/// only when `may_access_session` lets this user open them.
pub(super) struct Caller {
    pub session_id: String,
    pub session_name: String,
    pub user_id: String,
    pub is_admin: bool,
}

impl McpToolRegistry {
    /// The calling session's owner. Tools that act on the user's screen or
    /// the user's views refuse an unowned session.
    pub(super) async fn ui_caller(&self, ctx: &ToolCallContext) -> anyhow::Result<Caller> {
        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;
        let user_id = caller
            .user_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("this session has no owning user"))?;
        let is_admin = ctx
            .db
            .get_user(&user_id)
            .await?
            .is_some_and(|u| u.role == "admin");
        Ok(Caller {
            session_id: caller.id,
            session_name: caller.name,
            user_id,
            is_admin,
        })
    }
}

/// Lowercase, alphanumerics only, words single-space separated — so
/// "Stashify-Dev" and "stashify dev" compare equal.
pub(super) fn normalize(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// How well `name` matches spoken `query`; 0 = no match. Exact beats
/// prefix beats substring beats "every query word appears somewhere".
pub(super) fn score(query: &str, name: &str) -> u32 {
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

/// The single best candidate, or (`Err`) the tool answer to return
/// instead: `not_found` / `ambiguous` plus the top candidates, opening
/// nothing. `tool` names the call to repeat with an exact id.
pub(super) fn pick_one(candidates: Vec<Candidate>, tool: &str) -> Result<Candidate, Value> {
    let top = candidates.first().map(|c| c.score).unwrap_or(0);
    let best = candidates.iter().filter(|c| c.score == top).count();
    if top > 0 && best == 1 {
        return Ok(candidates.into_iter().next().expect("non-empty"));
    }
    let listed: Vec<Value> = candidates
        .iter()
        .take(MAX_CANDIDATES)
        .map(Candidate::describe)
        .collect();
    Err(json!({
        "status": if top == 0 { "not_found" } else { "ambiguous" },
        "message": if top == 0 {
            "Nothing matched; nothing was opened. Ask the user what they meant.".to_string()
        } else {
            format!("Several matches; nothing was opened. Ask the user which one, then call {tool} again with its id.")
        },
        "candidates": listed,
    }))
}

impl McpToolRegistry {
    /// Everything of `kinds` the caller's user may open, scored against
    /// the query and sorted best-first; zero scores dropped. An exact id
    /// always wins. Kinds: project, session, card, folder, view.
    pub(super) async fn ui_candidates(
        &self,
        ctx: &ToolCallContext,
        caller: &Caller,
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
                // Never offer the calling session itself, nor sessions this
                // user may not open.
                if s.id == caller.session_id
                    || !crate::auth::access::may_access_session(
                        caller.is_admin,
                        &caller.user_id,
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

        if kinds.contains(&"view") {
            for v in ctx.db.list_session_views(&caller.user_id).await? {
                let score = rate(&v.id, &v.name);
                out.push(Candidate {
                    kind: "view",
                    id: v.id,
                    name: v.name,
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
