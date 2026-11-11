use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    /// `list_folders` — only the caller's own folder, except for the global
    /// voice assistant (admin-only, unscoped), which sees every folder.
    /// Folder isolation means workers in folder A must not even see folders
    /// B, C, …; the payload shape stays an array either way.
    pub(crate) async fn handle_list_folders(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, folder_id = %ctx.folder_id, "MCP tool: list_folders");
        let folders = if ctx.project_id.is_none() && ctx.is_voice().await {
            ctx.db.list_folders().await?
        } else {
            // If the caller's folder is gone, return an empty list (the
            // session row would have folded into a 401 at the route layer
            // first, but be defensive).
            ctx.db
                .get_folder(&ctx.folder_id)
                .await?
                .into_iter()
                .collect()
        };
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

    /// `create_folder` — the global voice assistant registers any folder
    /// (same unsafe-path refusal as `POST /api/folders`; an already
    /// registered path is returned as-is). Every other session gets only an
    /// idempotent lookup of its own folder: creating a NEW folder would let
    /// it escape its boundary and start operating in fresh space.
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
        let create = args
            .get("create_if_missing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        tracing::info!(session_id = %ctx.session_id, name = %name, path = %path, "MCP tool: create_folder");

        if ctx.project_id.is_none() && ctx.is_voice().await {
            let folder_id = register_folder(ctx, &path, Some(&name), create).await?;
            let folder = ctx
                .db
                .get_folder(&folder_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("folder vanished after registering"))?;
            return Ok(serde_json::json!({
                "status": "ok",
                "folder": { "id": folder.id, "name": folder.name, "path": folder.path }
            }));
        }

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

/// Find-or-register the folder at `path` for the voice assistant. Callers
/// must have checked `ctx.is_voice()` first.
pub(crate) async fn register_folder(
    ctx: &ToolCallContext,
    path: &str,
    name: Option<&str>,
    create_dir: bool,
) -> anyhow::Result<String> {
    let db = ctx.db.clone();
    let (path, name) = (path.to_string(), name.map(str::to_string));
    tokio::task::spawn_blocking(move || {
        crate::plugin::host::folder_id_for_path(&db, &path, name.as_deref(), create_dir)
    })
    .await?
    .map_err(|e| anyhow::anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession};

    async fn seed(db: &Db, expert_kind: Option<&str>) -> ToolCallContext {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "home".into(),
            name: "Home".into(),
            path: "/tmp/peckboard-home-folder".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_folder(NewFolder {
            id: "other".into(),
            name: "Other".into(),
            path: "/tmp/peckboard-other-folder".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            folder_id: "home".into(),
            name: "caller".into(),
            is_expert: expert_kind.is_some(),
            expert_kind: expert_kind.map(str::to_string),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await
        .unwrap();
        ToolCallContext {
            session_id: "s1".into(),
            project_id: None,
            card_id: None,
            folder_id: "home".into(),
            db: std::sync::Arc::new(db.clone()),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
            device_registry: None,
            background: None,
        }
    }

    /// The voice assistant sees and registers folders anywhere; any other
    /// session sees only its own and cannot register new ones.
    #[tokio::test]
    async fn voice_assistant_lists_and_registers_any_folder_others_cannot() {
        let reg = McpToolRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let new_path = dir.path().join("preflight-pcb");
        std::fs::create_dir(&new_path).unwrap();
        let args = serde_json::json!({
            "name": "Preflight PCB",
            "path": new_path.to_string_lossy(),
        });

        let chat = seed(&Db::in_memory().unwrap(), None).await;
        let listed = reg.handle_list_folders(&chat).await.unwrap();
        assert_eq!(listed["count"], 1);
        assert!(reg.handle_create_folder(args.clone(), &chat).await.is_err());

        let voice = seed(&Db::in_memory().unwrap(), Some("voice")).await;
        let listed = reg.handle_list_folders(&voice).await.unwrap();
        assert_eq!(listed["count"], 2);
        let made = reg
            .handle_create_folder(args.clone(), &voice)
            .await
            .unwrap();
        assert_eq!(made["folder"]["name"], "Preflight PCB");
        // Registering the same path again returns the same folder.
        let again = reg.handle_create_folder(args, &voice).await.unwrap();
        assert_eq!(again["folder"]["id"], made["folder"]["id"]);
        assert_eq!(reg.handle_list_folders(&voice).await.unwrap()["count"], 3);
        // A missing directory needs create_if_missing.
        let missing = dir.path().join("not-yet");
        let refused = reg
            .handle_create_folder(
                serde_json::json!({ "name": "x", "path": missing.to_string_lossy() }),
                &voice,
            )
            .await;
        assert!(refused.is_err());
        // With create_if_missing it creates the directory, anywhere.
        let nested = dir.path().join("new").join("deep");
        let created = reg
            .handle_create_folder(
                serde_json::json!({
                    "name": "Deep",
                    "path": nested.to_string_lossy(),
                    "create_if_missing": true,
                }),
                &voice,
            )
            .await
            .unwrap();
        assert!(nested.is_dir());
        // And create_project can do the same through folder_path.
        let pcb2 = dir.path().join("pcb2");
        let project = reg
            .handle_create_project(
                serde_json::json!({
                    "name": "Preflight PCB",
                    "folder_path": pcb2.to_string_lossy(),
                    "create_folder_if_missing": true,
                }),
                &voice,
            )
            .await
            .unwrap();
        assert!(pcb2.is_dir());
        let pid = project["project"]["id"]
            .as_str()
            .or_else(|| project["id"].as_str())
            .expect("project id")
            .to_string();
        let row = voice.db.get_project(&pid).await.unwrap().unwrap();
        let folder = voice.db.get_folder(&row.folder_id).await.unwrap().unwrap();
        assert_eq!(folder.path, pcb2.to_string_lossy());
        assert_ne!(created["folder"]["id"], serde_json::json!(row.folder_id));
    }
}
