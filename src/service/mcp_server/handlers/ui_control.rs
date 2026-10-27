//! `ui_open` / `ui_tabs` — an agent drives what the USER is looking at in
//! their open browser tab(s).
//!
//! `ui_open` resolves a target (a session / project / card / folder / saved
//! view by id or fuzzy name, a Settings sub-page, a plugin page, or a
//! top-level page) server-side, then sends a `ui-navigate` frame to the
//! calling session's OWNER only ([`Broadcaster::send_to_user`] with
//! [`UserAudience::Foreground`]: the tab(s) the user is looking at, else the
//! most recently active one). Nothing is ever broadcast to other users. The
//! voice assistant's `show_view` (`voice_nav.rs`) rides the same frame.
//!
//! [`Broadcaster::send_to_user`]: crate::ws::broadcaster::Broadcaster::send_to_user

use serde_json::{Value, json};

use super::super::McpToolRegistry;
use super::ui_match::{Caller, normalize, pick_one};
use crate::service::mcp_server::context::ToolCallContext;
use crate::ws::broadcaster::{UserAudience, WsEvent};

/// Per-user WS frame the browser turns into a navigation.
pub(crate) const UI_NAVIGATE_EVENT: &str = "ui-navigate";

/// Top-level pages `target: "page"` may open.
pub(crate) const PAGES: &[&str] = &[
    "sessions",
    "projects",
    "folders",
    "views",
    "settings",
    "reports",
    "repeating_tasks",
    "terminals",
    "review",
    "usage",
    "agents",
];

/// Settings sub-pages (`SettingsPage.tsx` `GROUPS`) and whether each is
/// admin-only there (the routes behind them are admin-gated server-side).
pub(crate) const SETTINGS_PAGES: &[(&str, bool)] = &[
    ("account", false),
    ("appearance", false),
    ("sounds", false),
    ("assistant", false),
    ("chat", false),
    ("prompts", false),
    ("workflows", true),
    ("variables", false),
    ("providers", false),
    ("mcp", true),
    ("ssh-keys", false),
    ("remote-access", true),
    ("external-access", true),
    ("plugins", false),
    ("registry", false),
    ("server", true),
    ("security", true),
    ("data", true),
    ("tls", true),
    ("users", true),
];

/// Plugin ids and page item ids are slugs; anything else never names a page.
fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Trimmed, non-empty string argument.
pub(super) fn str_arg(args: &Value, k: &str) -> Option<String> {
    args.get(k)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl McpToolRegistry {
    /// `ui_open` — see the module docs.
    pub(crate) async fn handle_ui_open(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        tracing::info!(session_id = %ctx.session_id, ?args, "MCP tool: ui_open");
        self.open_target(ctx, &caller, &args, "ui_open").await
    }

    /// Resolve `args` (`target`, `id`, `name`, `sub`, `query`) and navigate
    /// the caller's user there. Shared by `ui_open` and `show_view`.
    pub(super) async fn open_target(
        &self,
        ctx: &ToolCallContext,
        caller: &Caller,
        args: &Value,
        tool: &str,
    ) -> anyhow::Result<Value> {
        let target = str_arg(args, "target").unwrap_or_else(|| "auto".into());
        let id = str_arg(args, "id");
        let name = str_arg(args, "name");
        let sub = str_arg(args, "sub");

        match target.as_str() {
            "page" => {
                let page = name
                    .as_deref()
                    .or(id.as_deref())
                    .map(|p| normalize(p).replace(' ', "_"))
                    .ok_or_else(|| anyhow::anyhow!("{tool} target 'page' requires 'name'"))?;
                let page = PAGES
                    .iter()
                    .find(|p| **p == page || p.trim_end_matches('s') == page)
                    .ok_or_else(|| {
                        anyhow::anyhow!("unknown page '{page}'; one of: {}", PAGES.join(", "))
                    })?;
                return Ok(self.navigate_user(
                    ctx,
                    caller,
                    json!({ "target": "page", "page": page }),
                    page,
                ));
            }
            "settings" => {
                let Some(sub) = sub.or(name).or(id) else {
                    return Ok(self.navigate_user(
                        ctx,
                        caller,
                        json!({ "target": "settings" }),
                        "Settings",
                    ));
                };
                let key = sub.to_lowercase().replace([' ', '_'], "-");
                let Some((page, admin_only)) = SETTINGS_PAGES.iter().find(|(p, _)| *p == key)
                else {
                    let all: Vec<&str> = SETTINGS_PAGES.iter().map(|(p, _)| *p).collect();
                    anyhow::bail!("unknown settings page '{sub}'; one of: {}", all.join(", "));
                };
                if *admin_only && !caller.is_admin {
                    anyhow::bail!(
                        "settings page '{page}' is admin-only and this session's user is not an admin"
                    );
                }
                return Ok(self.navigate_user(
                    ctx,
                    caller,
                    json!({ "target": "settings", "sub": page }),
                    &format!("Settings → {page}"),
                ));
            }
            "plugin_page" => {
                let plugin = id.or(name).ok_or_else(|| {
                    anyhow::anyhow!("{tool} target 'plugin_page' requires 'id' (the plugin id)")
                })?;
                let item = sub.ok_or_else(|| {
                    anyhow::anyhow!("{tool} target 'plugin_page' requires 'sub' (the page item id)")
                })?;
                if !is_slug(&plugin) || !is_slug(&item) {
                    anyhow::bail!(
                        "plugin and page ids may only hold letters, digits, '-', '_', '.'"
                    );
                }
                let query = str_arg(args, "query").map(|q| q.trim_start_matches('?').to_string());
                if query.as_ref().is_some_and(|q| q.len() > 2000) {
                    anyhow::bail!("query must be at most 2000 characters");
                }
                return Ok(self.navigate_user(
                    ctx,
                    caller,
                    json!({ "target": "plugin_page", "plugin": plugin, "item": item, "query": query }),
                    &format!("plugin page {plugin}/{item}"),
                ));
            }
            _ => {}
        }

        let kinds: &[&str] = match target.as_str() {
            "auto" => &["project", "session", "folder", "view"],
            "project" => &["project"],
            "session" => &["session"],
            "card" => &["card"],
            "folder" => &["folder"],
            "view" => &["view"],
            other => anyhow::bail!(
                "unknown target '{other}'; use session, project, card, folder, view, settings, \
                 plugin_page, page, or auto"
            ),
        };
        if id.is_none() && name.is_none() {
            anyhow::bail!("{tool} target '{target}' requires 'id' or 'name'");
        }
        let candidates = self
            .ui_candidates(ctx, caller, kinds, id.as_deref(), name.as_deref())
            .await?;
        let c = match pick_one(candidates, tool) {
            Ok(c) => c,
            Err(answer) => return Ok(answer),
        };
        let payload = match c.kind {
            "card" => json!({
                "target": "card",
                "id": c.id,
                "project_id": c.project_id,
                "name": c.name,
            }),
            kind => json!({ "target": kind, "id": c.id, "name": c.name }),
        };
        Ok(self.navigate_user(ctx, caller, payload, &format!("{} {}", c.kind, c.name)))
    }

    /// Send `data` as a `ui-navigate` frame to the caller's user, stamped
    /// with the source session (the browser's toast names it).
    pub(super) fn navigate_user(
        &self,
        ctx: &ToolCallContext,
        caller: &Caller,
        mut data: Value,
        what: &str,
    ) -> Value {
        data["source"] = json!({ "session_id": caller.session_id, "name": caller.session_name });
        let delivered = ctx.broadcaster.send_to_user(
            &caller.user_id,
            UserAudience::Foreground,
            WsEvent {
                event_type: UI_NAVIGATE_EVENT.into(),
                session_id: caller.session_id.clone(),
                data: data.clone(),
            },
        );
        if delivered == 0 {
            return json!({
                "status": "no_browser",
                "delivered": 0,
                "message": format!("The user has no Peckboard tab open, so {what} was not opened."),
                "opened": data,
            });
        }
        json!({
            "status": "ok",
            "delivered": delivered,
            "message": format!("Opened {what} on the user's screen."),
            "opened": data,
        })
    }

    /// `ui_tabs` — the caller's user's saved in-app tabs, most recent first.
    pub(crate) async fn handle_ui_tabs(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        let tabs: Vec<Value> = ctx
            .db
            .list_user_tabs(&caller.user_id)
            .await?
            .into_iter()
            .map(|t| {
                json!({
                    "type": t.item_type,
                    "id": t.item_id,
                    "last_active": t.last_active,
                })
            })
            .collect();
        Ok(json!({ "tabs": tabs }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession, NewUser};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
    use crate::ws::broadcaster::Broadcaster;

    async fn seed(db: &Db) {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "infra".into(),
            path: "/tmp".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        for (u, role) in [("u1", "user"), ("u2", "user")] {
            db.create_user(NewUser {
                id: u.into(),
                username: u.into(),
                email: None,
                password_hash: "h".into(),
                role: role.into(),
                created_at: now.clone(),
                updated_at: now.clone(),
            })
            .await
            .unwrap();
        }
        for (id, name, user) in [
            ("chat", "Planner", "u1"),
            ("s-dev", "Stashify dev", "u1"),
            ("s-other", "Stashify secrets", "u2"),
        ] {
            db.create_session(NewSession {
                id: id.into(),
                name: name.into(),
                folder_id: "f1".into(),
                created_at: now.clone(),
                last_activity: now.clone(),
                user_id: Some(user.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn ui_open_navigates_only_the_owners_browser() {
        let db = Db::in_memory().unwrap();
        seed(&db).await;
        let broadcaster = Broadcaster::new();
        let mut global = broadcaster.subscribe_all();
        let mut rx = broadcaster.subscribe_user_frames();
        broadcaster.register_connection(1, "u1");
        broadcaster.register_connection(2, "u2");
        let ctx = ToolCallContext {
            session_id: "chat".into(),
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
        let open = |args: serde_json::Value| reg.handle_tool_call("ui_open", args, &ctx);

        // Fuzzy session name; u2's "Stashify secrets" is never a candidate.
        let ok = open(serde_json::json!({"target": "session", "name": "stashify"}))
            .await
            .unwrap();
        assert_eq!(ok["opened"]["id"], "s-dev");
        assert_eq!(ok["delivered"], 1);
        let f = rx.try_recv().unwrap();
        assert!(f.is_for(1) && !f.is_for(2));
        assert_eq!(f.event().event_type, "ui-navigate");
        assert_eq!(f.event().data["source"]["name"], "Planner");
        assert!(global.try_recv().is_err(), "never broadcast globally");

        // Another user's session by exact id: not found, nothing sent.
        let nf = open(serde_json::json!({"target": "session", "id": "s-other"}))
            .await
            .unwrap();
        assert_eq!(nf["status"], "not_found");
        assert!(rx.try_recv().is_err());

        // Settings: a normal page opens, an admin-only one is refused.
        let ok = open(serde_json::json!({"target": "settings", "sub": "appearance"}))
            .await
            .unwrap();
        assert_eq!(ok["opened"]["sub"], "appearance");
        let err = open(serde_json::json!({"target": "settings", "sub": "users"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("admin-only"), "{err}");

        // Plugin page with a query.
        let ok = open(serde_json::json!({
            "target": "plugin_page", "id": "app-manager", "sub": "apps", "query": "?q=graphify"
        }))
        .await
        .unwrap();
        assert_eq!(ok["opened"]["query"], "q=graphify");

        // ui_tabs answers the owner's saved tabs.
        db.upsert_user_tab("u1", "session", "s-dev").await.unwrap();
        let tabs = reg
            .handle_tool_call("ui_tabs", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(tabs["tabs"][0]["id"], "s-dev");
    }
}
