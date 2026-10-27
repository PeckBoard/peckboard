//! View tools — an agent reads and edits the calling session's OWNER's
//! saved views (widget dashboards): `list_views`, `get_view`,
//! `create_view`, `update_view`, `add_view_widget`, `remove_view_widget`,
//! `delete_view`. Every DB call is keyed by the owner's user id, so another
//! user's view answers like a missing one. Widget refs go through the same
//! [`views::check_widget_refs`] as the REST routes (a session the owner may
//! not open is refused), and every write sends the owner's open tabs a
//! `views-changed` frame so an open dashboard refreshes live.

use serde_json::{Value, json};

use super::super::McpToolRegistry;
use super::ui_control::str_arg;
use super::ui_match::{Caller, pick_one};
use crate::db::crud::{ViewWidget, WidgetKind, validate_view_name, validate_widgets};
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::views::{self, RefError, ViewAction};

/// Turn agent-supplied widget objects into [`ViewWidget`]s, filling what an
/// agent may leave out: `id` (generated), `w` / `h` (the kind's default
/// size), and `x` / `y` (the first free slot that overlaps nothing already
/// placed — `existing` first, then earlier widgets of this batch).
fn materialize(existing: &[ViewWidget], input: &[Value]) -> anyhow::Result<Vec<ViewWidget>> {
    let mut placed: Vec<ViewWidget> = existing.to_vec();
    let mut out = Vec::with_capacity(input.len());
    for raw in input {
        let Some(obj) = raw.as_object() else {
            anyhow::bail!("each widget must be an object");
        };
        let mut obj = obj.clone();
        let kind: WidgetKind = serde_json::from_value(
            obj.get("kind")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("each widget needs a 'kind'"))?,
        )
        .map_err(|e| anyhow::anyhow!("invalid widget kind: {e}"))?;
        if !obj.get("id").is_some_and(|v| v.is_string()) {
            obj.insert("id".into(), json!(format!("w-{}", uuid::Uuid::new_v4())));
        }
        let (dw, dh) = views::default_size(kind);
        let w = obj.get("w").and_then(Value::as_i64).unwrap_or(dw as i64) as i32;
        let h = obj.get("h").and_then(Value::as_i64).unwrap_or(dh as i64) as i32;
        obj.insert("w".into(), json!(w));
        obj.insert("h".into(), json!(h));
        let has_pos =
            obj.get("x").is_some_and(Value::is_i64) && obj.get("y").is_some_and(Value::is_i64);
        if !has_pos && (1..=12).contains(&w) {
            let (x, y) = views::free_slot(&placed, w, h);
            obj.insert("x".into(), json!(x));
            obj.insert("y".into(), json!(y));
        }
        let widget: ViewWidget = serde_json::from_value(Value::Object(obj))
            .map_err(|e| anyhow::anyhow!("invalid widget: {e}"))?;
        placed.push(widget.clone());
        out.push(widget);
    }
    Ok(out)
}

fn widgets_arg(args: &Value, key: &str) -> anyhow::Result<Option<Vec<Value>>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => Ok(Some(a.clone())),
        Some(_) => anyhow::bail!("'{key}' must be an array of widgets"),
    }
}

fn ref_error(e: RefError) -> anyhow::Error {
    match e {
        RefError::NotFound(m) => anyhow::anyhow!(m),
        RefError::Internal(e) => e,
    }
}

impl McpToolRegistry {
    /// The owner's view named by `id` (exact) or `name` (fuzzy), or the
    /// tool answer to return instead (not found / ambiguous).
    async fn resolve_view(
        &self,
        ctx: &ToolCallContext,
        caller: &Caller,
        args: &Value,
        tool: &str,
    ) -> anyhow::Result<Result<String, Value>> {
        let (id, name) = (str_arg(args, "id"), str_arg(args, "name"));
        if id.is_none() && name.is_none() {
            anyhow::bail!("{tool} requires 'id' (or 'name')");
        }
        let candidates = self
            .ui_candidates(ctx, caller, &["view"], id.as_deref(), name.as_deref())
            .await?;
        Ok(pick_one(candidates, tool).map(|c| c.id))
    }

    async fn checked(
        &self,
        ctx: &ToolCallContext,
        caller: &Caller,
        widgets: &[ViewWidget],
    ) -> anyhow::Result<()> {
        validate_widgets(widgets).map_err(|e| anyhow::anyhow!(e))?;
        views::check_widget_refs(&ctx.db, &caller.user_id, caller.is_admin, widgets)
            .await
            .map_err(ref_error)
    }

    async fn view_answer(
        &self,
        ctx: &ToolCallContext,
        caller: &Caller,
        view: (crate::db::models::SessionView, Vec<ViewWidget>),
    ) -> anyhow::Result<Value> {
        views::full_json(&ctx.db, &caller.user_id, caller.is_admin, view).await
    }

    fn notify(&self, ctx: &ToolCallContext, caller: &Caller, view_id: &str, action: ViewAction) {
        views::notify_changed(&ctx.broadcaster, &caller.user_id, view_id, action, None);
    }

    pub(crate) async fn handle_list_views(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        let list = ctx.db.list_session_views(&caller.user_id).await?;
        Ok(json!({ "views": list.iter().map(views::summary_json).collect::<Vec<_>>() }))
    }

    pub(crate) async fn handle_get_view(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        let id = match self.resolve_view(ctx, &caller, &args, "get_view").await? {
            Ok(id) => id,
            Err(answer) => return Ok(answer),
        };
        let view = ctx
            .db
            .get_session_view(&caller.user_id, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("view not found"))?;
        self.view_answer(ctx, &caller, view).await
    }

    pub(crate) async fn handle_create_view(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        let name = validate_view_name(&str_arg(&args, "name").unwrap_or_default())
            .map_err(|e| anyhow::anyhow!(e))?;
        let widgets = materialize(&[], &widgets_arg(&args, "widgets")?.unwrap_or_default())?;
        self.checked(ctx, &caller, &widgets).await?;
        let view = ctx
            .db
            .create_session_view(&caller.user_id, &name, widgets)
            .await?;
        let view_id = view.0.id.clone();
        tracing::info!(session_id = %ctx.session_id, %view_id, "MCP tool: create_view");
        self.notify(ctx, &caller, &view_id, ViewAction::Created);
        let mut out = self.view_answer(ctx, &caller, view).await?;
        if args.get("show").and_then(Value::as_bool).unwrap_or(false) {
            let nav = self.navigate_user(
                ctx,
                &caller,
                json!({ "target": "view", "id": view_id, "name": name }),
                &format!("view {name}"),
            );
            out["shown"] = nav["delivered"].clone();
        }
        Ok(out)
    }

    pub(crate) async fn handle_update_view(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        let id = match self
            .resolve_view(ctx, &caller, &args, "update_view")
            .await?
        {
            Ok(id) => id,
            Err(answer) => return Ok(answer),
        };
        // `name` selects the view when no id is given, so a rename needs
        // `new_name` in that case; with an id, `name` is the new name.
        let rename = if str_arg(&args, "id").is_some() {
            str_arg(&args, "new_name").or_else(|| str_arg(&args, "name"))
        } else {
            str_arg(&args, "new_name")
        };
        let name = rename
            .as_deref()
            .map(validate_view_name)
            .transpose()
            .map_err(|e| anyhow::anyhow!(e))?;
        let widgets = match widgets_arg(&args, "widgets")? {
            Some(raw) => {
                let w = materialize(&[], &raw)?;
                self.checked(ctx, &caller, &w).await?;
                Some(w)
            }
            None => None,
        };
        let view = ctx
            .db
            .update_session_view(&caller.user_id, &id, name, widgets)
            .await?
            .ok_or_else(|| anyhow::anyhow!("view not found"))?;
        self.notify(ctx, &caller, &id, ViewAction::Updated);
        self.view_answer(ctx, &caller, view).await
    }

    pub(crate) async fn handle_add_view_widget(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        let id = match self
            .resolve_view(ctx, &caller, &args, "add_view_widget")
            .await?
        {
            Ok(id) => id,
            Err(answer) => return Ok(answer),
        };
        let raw = args
            .get("widget")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("add_view_widget requires 'widget'"))?;
        let (_, mut widgets) = ctx
            .db
            .get_session_view(&caller.user_id, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("view not found"))?;
        let added = materialize(&widgets, std::slice::from_ref(&raw))?;
        let widget_id = added[0].id.clone();
        widgets.extend(added);
        self.checked(ctx, &caller, &widgets).await?;
        let view = ctx
            .db
            .update_session_view(&caller.user_id, &id, None, Some(widgets))
            .await?
            .ok_or_else(|| anyhow::anyhow!("view not found"))?;
        self.notify(ctx, &caller, &id, ViewAction::Updated);
        let mut out = self.view_answer(ctx, &caller, view).await?;
        out["added_widget_id"] = json!(widget_id);
        Ok(out)
    }

    pub(crate) async fn handle_remove_view_widget(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        let id = match self
            .resolve_view(ctx, &caller, &args, "remove_view_widget")
            .await?
        {
            Ok(id) => id,
            Err(answer) => return Ok(answer),
        };
        let widget_id = str_arg(&args, "widget_id")
            .ok_or_else(|| anyhow::anyhow!("remove_view_widget requires 'widget_id'"))?;
        let (_, mut widgets) = ctx
            .db
            .get_session_view(&caller.user_id, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("view not found"))?;
        let before = widgets.len();
        widgets.retain(|w| w.id != widget_id);
        if widgets.len() == before {
            anyhow::bail!("view has no widget '{widget_id}' (see get_view)");
        }
        let view = ctx
            .db
            .update_session_view(&caller.user_id, &id, None, Some(widgets))
            .await?
            .ok_or_else(|| anyhow::anyhow!("view not found"))?;
        self.notify(ctx, &caller, &id, ViewAction::Updated);
        self.view_answer(ctx, &caller, view).await
    }

    pub(crate) async fn handle_delete_view(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let caller = self.ui_caller(ctx).await?;
        let id = match self
            .resolve_view(ctx, &caller, &args, "delete_view")
            .await?
        {
            Ok(id) => id,
            Err(answer) => return Ok(answer),
        };
        if !ctx.db.delete_session_view(&caller.user_id, &id).await? {
            anyhow::bail!("view not found");
        }
        tracing::info!(session_id = %ctx.session_id, view_id = %id, "MCP tool: delete_view");
        self.notify(ctx, &caller, &id, ViewAction::Deleted);
        Ok(json!({ "status": "ok", "deleted": id }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession, NewUser};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
    use crate::ws::broadcaster::Broadcaster;

    #[tokio::test]
    async fn view_tools_are_owner_only_check_refs_and_auto_place() {
        let db = Db::in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        for u in ["u1", "u2"] {
            db.create_user(NewUser {
                id: u.into(),
                username: u.into(),
                email: None,
                password_hash: "h".into(),
                role: "user".into(),
                created_at: now.clone(),
                updated_at: now.clone(),
            })
            .await
            .unwrap();
        }
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "infra".into(),
            path: "/tmp".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        for (id, user) in [("a", "u1"), ("a2", "u1"), ("b", "u2")] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                created_at: now.clone(),
                last_activity: now.clone(),
                user_id: Some(user.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let broadcaster = Broadcaster::new();
        let mut rx = broadcaster.subscribe_user_frames();
        broadcaster.register_connection(1, "u1");
        broadcaster.register_connection(2, "u2");
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
        let (ca, cb) = (ctx("a"), ctx("b"));

        // A foreign session ref is refused.
        let err = reg
            .handle_tool_call(
                "create_view",
                json!({"name": "Ops", "widgets": [{"kind": "session", "sessionId": "b"}]}),
                &ca,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("session not found"), "{err}");

        // Create with an own session; position + size filled in.
        let v = reg
            .handle_tool_call(
                "create_view",
                json!({"name": "Ops", "widgets": [{"kind": "session", "sessionId": "a2"}]}),
                &ca,
            )
            .await
            .unwrap();
        let view_id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["widgets"][0]["x"], 0);
        assert_eq!(v["widgets"][0]["w"], 6);
        let f = rx.try_recv().unwrap();
        assert_eq!(f.event().event_type, "views-changed");
        assert!(f.is_for(1) && !f.is_for(2));

        // add_view_widget auto-places next to the existing pane.
        let v = reg
            .handle_tool_call(
                "add_view_widget",
                json!({"name": "ops", "widget": {"kind": "note", "body": "hi"}}),
                &ca,
            )
            .await
            .unwrap();
        let note = &v["widgets"][1];
        assert_eq!((note["x"].clone(), note["y"].clone()), (json!(6), json!(0)));
        let note_id = v["added_widget_id"].as_str().unwrap().to_string();

        // Another user can neither see nor edit it.
        let listed = reg
            .handle_tool_call("list_views", json!({}), &cb)
            .await
            .unwrap();
        assert_eq!(listed["views"].as_array().unwrap().len(), 0);
        let nf = reg
            .handle_tool_call("get_view", json!({"id": view_id}), &cb)
            .await
            .unwrap();
        assert_eq!(nf["status"], "not_found");
        let nf = reg
            .handle_tool_call("delete_view", json!({"id": view_id}), &cb)
            .await
            .unwrap();
        assert_eq!(nf["status"], "not_found");

        // Remove + delete by the owner.
        let v = reg
            .handle_tool_call(
                "remove_view_widget",
                json!({"id": view_id, "widget_id": note_id}),
                &ca,
            )
            .await
            .unwrap();
        assert_eq!(v["widgets"].as_array().unwrap().len(), 1);
        reg.handle_tool_call("delete_view", json!({"id": view_id}), &ca)
            .await
            .unwrap();
        assert!(db.get_session_view("u1", &view_id).await.unwrap().is_none());
    }
}
