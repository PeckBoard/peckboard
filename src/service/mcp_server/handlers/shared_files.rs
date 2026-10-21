//! `share_file` — hand the user a downloadable file in the chat.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::shared_files::{self, Origin};

impl McpToolRegistry {
    pub(crate) async fn handle_share_file(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("share_file requires 'path'"))?;
        let caption = args.get("caption").and_then(|v| v.as_str());
        let payload = share_from_folder(ctx, path, Origin::Share, caption).await?;
        Ok(serde_json::json!({
            "status": "ok",
            "filename": payload["filename"],
            "size": payload["size"],
            "note": "The file is now shown in the chat with preview and download.",
        }))
    }

    /// `write_file` created a new artifact (image, video, archive, pdf):
    /// show it in the chat as a downloadable card too. Best-effort.
    pub(crate) async fn auto_share_created(&self, ctx: &ToolCallContext, diff: &Value) {
        if diff.get("created").and_then(|v| v.as_bool()) != Some(true) {
            return;
        }
        let Some(path) = diff.get("path").and_then(|v| v.as_str()) else {
            return;
        };
        if !shared_files::is_auto_share_path(path) {
            return;
        }
        if let Err(e) = share_from_folder(ctx, path, Origin::Created, None).await {
            tracing::debug!(session_id = %ctx.session_id, path, "auto-share skipped: {e}");
        }
    }
}

/// Jail `path` to the caller's folder, snapshot it, emit `file-shared`.
async fn share_from_folder(
    ctx: &ToolCallContext,
    path: &str,
    origin: Origin,
    caption: Option<&str>,
) -> anyhow::Result<Value> {
    let data_dir = ctx
        .data_dir
        .clone()
        .ok_or_else(|| anyhow::anyhow!("file sharing is unavailable in this context"))?;
    let folder = ctx
        .db
        .get_folder(&ctx.folder_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("caller folder not found"))?;
    let path_owned = path.to_string();
    let (canon, rel) = tokio::task::spawn_blocking(move || {
        let root = std::fs::canonicalize(&folder.path)
            .map_err(|e| format!("folder path unavailable: {e}"))?;
        shared_files::resolve_in_folder(&root, &path_owned)
    })
    .await?
    .map_err(|e| anyhow::anyhow!(e))?;
    shared_files::share(
        &ctx.db,
        &ctx.broadcaster,
        &data_dir,
        &ctx.session_id,
        &canon,
        &rel,
        origin,
        caption,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};
    use std::sync::Arc;

    #[tokio::test]
    async fn share_file_jails_to_the_folder_and_emits_an_event() {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let folder_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(folder_dir.path().join("out.zip"), b"PK").unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"s").unwrap();

        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: folder_dir.path().to_string_lossy().to_string(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "s".into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await
        .unwrap();
        let ctx = ToolCallContext {
            session_id: "s1".into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: db.clone(),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: Some(data_dir.path().to_path_buf()),
            device_registry: None,
            background: None,
        };
        let reg = McpToolRegistry::new();

        for bad in [
            "../secret.txt".to_string(),
            outside
                .path()
                .join("secret.txt")
                .to_string_lossy()
                .to_string(),
        ] {
            let err = reg
                .handle_tool_call("share_file", serde_json::json!({ "path": bad }), &ctx)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("project folder"), "{err}");
        }

        let ok = reg
            .handle_tool_call(
                "share_file",
                serde_json::json!({ "path": "out.zip", "caption": "the build" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(ok["filename"], "out.zip");
        let ev = db
            .latest_event_of_kind("s1", shared_files::EVENT_KIND)
            .await
            .unwrap()
            .expect("file-shared event");
        let data: Value = serde_json::from_str(&ev.data).unwrap();
        assert_eq!(data["origin"], "share");
        assert_eq!(data["caption"], "the build");
        assert_eq!(data["sourcePath"], "out.zip");
        let id = data["id"].as_str().unwrap();
        assert!(
            shared_files::lookup(data_dir.path(), "s1", id)
                .await
                .is_some()
        );
    }
}
