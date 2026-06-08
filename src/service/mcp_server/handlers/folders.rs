use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::NewFolder;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    pub(crate) async fn handle_list_folders(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_folders");
        let folders = ctx.db.list_folders().await?;
        let items: Vec<Value> = folders
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
        let create_if_missing = args
            .get("create_if_missing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        tracing::info!(session_id = %ctx.session_id, name = %name, path = %path, create_if_missing, "MCP tool: create_folder");

        let folder = self
            .upsert_folder(ctx, &name, &path, create_if_missing)
            .await?;
        Ok(serde_json::json!({
            "status": "ok",
            "folder": {
                "id": folder.id,
                "name": folder.name,
                "path": folder.path,
            }
        }))
    }

    /// Resolve a folder by path, or register a new one. If the path doesn't
    /// exist on disk, optionally create the directory.
    pub(crate) async fn upsert_folder(
        &self,
        ctx: &ToolCallContext,
        name: &str,
        path: &str,
        create_if_missing: bool,
    ) -> anyhow::Result<crate::db::models::Folder> {
        // If a folder is already registered with this path, return it
        let folders = ctx.db.list_folders().await?;
        if let Some(existing) = folders.iter().find(|f| f.path == path) {
            return Ok(existing.clone());
        }

        let dir = std::path::Path::new(path);
        if !dir.exists() {
            if create_if_missing {
                std::fs::create_dir_all(dir)
                    .map_err(|e| anyhow::anyhow!("failed to create directory '{path}': {e}"))?;
                tracing::info!(path = %path, "Created directory on disk");
            } else {
                anyhow::bail!(
                    "path does not exist: {path} (set create_if_missing=true to create it)"
                );
            }
        }

        let now = chrono::Utc::now().to_rfc3339();
        let folder = ctx
            .db
            .create_folder(NewFolder {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.to_string(),
                path: path.to_string(),
                created_at: now,
            })
            .await?;
        Ok(folder)
    }
}
