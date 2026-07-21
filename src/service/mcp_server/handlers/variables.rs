//! Agent variable tools — plain name/value state agents read AND write,
//! scoped global or per-folder (folder shadows global on a name collision).
//! Storage: `db::crud::agent_vars`; the user-facing side is
//! `routes/agent_vars.rs` + Settings → Agent Variables. Values are
//! agent-readable by design (no encryption, no masking), so the tool
//! descriptions tell agents to keep secrets in env vars instead.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::NewAgentVar;
use crate::service::mcp_server::context::ToolCallContext;

/// Same grammar and caps as the HTTP surface (`routes/agent_vars.rs`) — one
/// rule regardless of which door a var comes in through.
const NAME_MAX_LEN: usize = 128;
const VALUE_MAX_LEN: usize = 32768;

/// `^[A-Za-z_][A-Za-z0-9_]*$`, length ≤ [`NAME_MAX_LEN`].
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > NAME_MAX_LEN {
        return false;
    }
    let first = name.as_bytes()[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Resolve the tool-call `scope` arg ('folder' default | 'global') to the
/// stored `folder_id` (`Some(caller's folder)` | `None`).
fn scope_to_folder_id(args: &Value, ctx: &ToolCallContext) -> anyhow::Result<Option<String>> {
    match args
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("folder")
    {
        "folder" => Ok(Some(ctx.folder_id.clone())),
        "global" => Ok(None),
        other => anyhow::bail!("invalid scope '{other}' — use 'folder' or 'global'"),
    }
}

impl McpToolRegistry {
    /// `list_variables` — the effective set for the caller's folder: globals
    /// plus the folder's own vars, the folder winning on a name collision
    /// (the shadowed global is dropped, matching what `set_variable` in
    /// folder scope makes agents expect).
    pub(crate) async fn handle_list_variables(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, folder_id = %ctx.folder_id, "MCP tool: list_variables");
        let rows = ctx.db.list_agent_vars_for_folder(&ctx.folder_id).await?;
        let mut merged: std::collections::BTreeMap<String, &crate::db::models::AgentVar> =
            std::collections::BTreeMap::new();
        for r in rows.iter().filter(|r| r.folder_id.is_none()) {
            merged.insert(r.name.clone(), r);
        }
        for r in rows.iter().filter(|r| r.folder_id.is_some()) {
            merged.insert(r.name.clone(), r);
        }
        let items: Vec<Value> = merged
            .values()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "value": r.value,
                    "scope": if r.folder_id.is_some() { "folder" } else { "global" },
                    "updated_at": r.updated_at,
                })
            })
            .collect();
        Ok(serde_json::json!({ "variables": items, "count": items.len() }))
    }

    /// `set_variable` — upsert by (name, scope). Folder scope writes the
    /// caller's own folder; agents can't reach other folders' scopes.
    pub(crate) async fn handle_set_variable(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("set_variable requires 'name'"))?
            .to_string();
        let value = args
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("set_variable requires a string 'value'"))?
            .to_string();
        if !valid_name(&name) {
            anyhow::bail!(
                "invalid name '{name}' — use ^[A-Za-z_][A-Za-z0-9_]*$, max {NAME_MAX_LEN} chars"
            );
        }
        if value.len() > VALUE_MAX_LEN {
            anyhow::bail!("value too long (max {VALUE_MAX_LEN} bytes)");
        }
        let folder_id = scope_to_folder_id(&args, ctx)?;
        let scope = if folder_id.is_some() {
            "folder"
        } else {
            "global"
        };
        tracing::info!(session_id = %ctx.session_id, folder_id = %ctx.folder_id, name = %name, scope = %scope, "MCP tool: set_variable");

        let now = chrono::Utc::now().to_rfc3339();
        let row = ctx
            .db
            .upsert_agent_var(NewAgentVar {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                value,
                folder_id,
                created_at: now.clone(),
                updated_at: now,
            })
            .await?;
        Ok(serde_json::json!({
            "status": "ok",
            "name": row.name,
            "scope": scope,
        }))
    }

    /// `delete_variable` — by (name, scope); folder scope is the caller's
    /// own folder.
    pub(crate) async fn handle_delete_variable(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("delete_variable requires 'name'"))?
            .to_string();
        let folder_id = scope_to_folder_id(&args, ctx)?;
        let scope = if folder_id.is_some() {
            "folder"
        } else {
            "global"
        };
        tracing::info!(session_id = %ctx.session_id, folder_id = %ctx.folder_id, name = %name, scope = %scope, "MCP tool: delete_variable");

        let deleted = ctx.db.delete_agent_var(&name, folder_id.as_deref()).await?;
        Ok(serde_json::json!({
            "status": if deleted { "ok" } else { "not_found" },
            "name": name,
            "scope": scope,
        }))
    }
}
