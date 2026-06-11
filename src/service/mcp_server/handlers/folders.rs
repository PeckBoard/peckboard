use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    /// `list_folders` — only the caller's own folder. Folder isolation
    /// means workers in folder A must not even see folders B, C, …; the
    /// payload shape stays an array so existing clients keep working
    /// without a special-case for the single-folder result.
    pub(crate) async fn handle_list_folders(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, folder_id = %ctx.folder_id, "MCP tool: list_folders");
        // Resolve only the caller's folder. If it's gone, return an empty
        // list (the session row would have folded into a 401 at the route
        // layer first, but be defensive).
        let folder = ctx.db.get_folder(&ctx.folder_id).await?;
        let items: Vec<Value> = folder
            .iter()
            .map(|f| {
                serde_json::json!({
                    "id": f.id,
                    "name": f.name,
                    "path": f.path,
                    "created_at": f.created_at,
                })
            })
            .collect();
        Ok(serde_json::json!({ "folders": items, "count": items.len() }))
    }

    /// `create_folder` — the MCP surface is intentionally tight: an
    /// existing-by-path lookup acts as a getter (returns the caller's
    /// own folder if its path matches), but creating a NEW folder via
    /// MCP would let any session escape its boundary and start operating
    /// in fresh space. That decision belongs to the human running the UI,
    /// not to an agent. The HTTP `/api/folders` route remains the way to
    /// register new folders.
    pub(crate) async fn handle_create_folder(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_folder requires 'name'"))?
            .to_string();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_folder requires 'path'"))?
            .to_string();

        tracing::info!(session_id = %ctx.session_id, name = %name, path = %path, "MCP tool: create_folder");

        // The path must resolve to the caller's own folder (idempotent
        // lookup). Any other path — including one that doesn't exist
        // yet — is rejected so MCP can't be used to materialise new
        // folders out of an agent's instructions.
        let caller = ctx
            .db
            .get_folder(&ctx.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller folder vanished"))?;
        if caller.path != path {
            anyhow::bail!(
                "create_folder is restricted to the caller's own folder; \
                 register new folders via the HTTP `/api/folders` route"
            );
        }
        Ok(serde_json::json!({
            "status": "ok",
            "folder": {
                "id": caller.id,
                "name": caller.name,
                "path": caller.path,
            }
        }))
    }
}
