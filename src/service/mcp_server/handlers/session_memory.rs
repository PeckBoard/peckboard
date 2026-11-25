//! `memory_*` MCP tools: the calling session's short-term notes and its
//! folder's long-term memory.
//!
//! Every tool resolves its pools from the verified session behind the
//! bearer token — `ctx.session_id` for the session's own notes,
//! `ctx.folder_id` for the folder's long-term notes — so an agent can only
//! ever read or write its own session's notes and its own folder's memory.
//! Ids from anywhere else come back as "not found", exactly like unknown
//! ids.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::{FolderMemory, SessionMemory};
use crate::service::mcp_server::context::ToolCallContext;

fn entry_json(m: &SessionMemory) -> Value {
    serde_json::json!({
        "id": m.id,
        "term": if m.expires_at.is_some() { "short" } else { "kept" },
        "content": m.content,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
        "expires_at": m.expires_at,
        "fading": crate::db::crud::memory_is_faded(m, chrono::Utc::now()),
    })
}

fn long_json(m: &FolderMemory) -> Value {
    serde_json::json!({
        "id": m.id,
        "term": "long",
        "category": m.category,
        "title": m.title,
        "content": m.content,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
    })
}

fn pool_json(entries: &[SessionMemory]) -> Value {
    serde_json::json!({
        "entries": entries.iter().map(entry_json).collect::<Vec<_>>(),
        "count": entries.len(),
        "total_chars": entries.iter().map(|m| m.content.chars().count()).sum::<usize>(),
        "limits": {
            "max_entries": crate::db::crud::MAX_MEMORY_ENTRIES,
            "max_entry_chars": crate::db::crud::MAX_MEMORY_ENTRY_CHARS,
            "max_total_chars": crate::db::crud::MAX_MEMORY_TOTAL_CHARS,
        },
    })
}

fn required_str<'a>(args: &'a Value, key: &str, tool: &str) -> anyhow::Result<&'a str> {
    optional_str(args, key).ok_or_else(|| anyhow::anyhow!("{tool} requires non-empty '{key}'"))
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Long-term notes are shared with every session of the folder, so
/// secret-shaped values are masked before they are stored. Returns the
/// stored text and whether anything was masked.
fn mask_shared(text: &str) -> (String, bool) {
    let masked = crate::service::redact::mask_credentials(&crate::service::redact::mask_text(text));
    let changed = masked != text;
    (masked, changed)
}

fn folder_of(ctx: &ToolCallContext) -> anyhow::Result<&str> {
    if ctx.folder_id.is_empty() {
        anyhow::bail!("this session has no folder, so it has no long-term memory");
    }
    Ok(&ctx.folder_id)
}

/// Tell open UIs (the Memory modal) that this session's pool changed.
fn broadcast_change(ctx: &ToolCallContext) {
    ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "session-memory".into(),
        session_id: ctx.session_id.clone(),
        data: serde_json::json!({ "session_id": ctx.session_id }),
    });
}

/// Tell open UIs that the folder's long-term memory changed.
fn broadcast_folder_change(ctx: &ToolCallContext) {
    ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "folder-memory".into(),
        session_id: ctx.session_id.clone(),
        data: serde_json::json!({ "folder_id": ctx.folder_id }),
    });
}

impl McpToolRegistry {
    pub(crate) async fn handle_memory_add(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let content = required_str(&args, "content", "memory_add")?;
        // The session must exist — a memory row must never dangle (the FK
        // would refuse anyway, but say why).
        ctx.db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found"))?;
        match optional_str(&args, "term").unwrap_or("short") {
            "long" => {
                let folder_id = folder_of(ctx)?;
                let title = required_str(&args, "title", "memory_add (term=long)")?;
                let category = required_str(&args, "category", "memory_add (term=long)")?;
                let (title, title_masked) = mask_shared(title);
                let (content, content_masked) = mask_shared(content);
                let entry = ctx
                    .db
                    .add_folder_memory(folder_id, category, &title, &content, Some(&ctx.session_id))
                    .await?;
                broadcast_folder_change(ctx);
                let mut note = String::from(
                    "Saved to the folder's long-term memory: every session working in this \
                     folder sees it from its next start.",
                );
                if title_masked || content_masked {
                    note.push_str(
                        " A value that looked like a secret was masked — long-term notes \
                         are shared, so keep credentials out of them.",
                    );
                }
                Ok(serde_json::json!({ "status": "ok", "entry": long_json(&entry), "note": note }))
            }
            "short" => {
                let hours = crate::routes::settings::short_term_memory_hours(&ctx.db).await;
                let entry = ctx
                    .db
                    .add_short_term_memory(&ctx.session_id, content, hours)
                    .await?;
                broadcast_change(ctx);
                Ok(serde_json::json!({
                    "status": "ok",
                    "entry": entry_json(&entry),
                    "note": format!(
                        "Saved as a short-term note: deleted {hours} hours after it was last \
                         written (memory_update restarts the clock). It appears in your Memory \
                         section after the next clear, resume, or compaction; you already know \
                         it for this conversation. If it should outlive this work, save it with \
                         term=\"long\" instead."
                    ),
                }))
            }
            other => {
                anyhow::bail!("memory_add 'term' must be \"short\" or \"long\", got '{other}'")
            }
        }
    }

    pub(crate) async fn handle_memory_list(
        &self,
        _args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let entries = ctx.db.list_session_memories(&ctx.session_id).await?;
        let mut out = pool_json(&entries);
        let long = if ctx.folder_id.is_empty() {
            Vec::new()
        } else {
            ctx.db.list_folder_memories(&ctx.folder_id).await?
        };
        out["long_term"] = serde_json::json!({
            "entries": long.iter().map(long_json).collect::<Vec<_>>(),
            "count": long.len(),
            "categories": crate::db::crud::FOLDER_MEMORY_CATEGORIES,
        });
        Ok(out)
    }

    pub(crate) async fn handle_memory_read(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let id = required_str(&args, "id", "memory_read")?;
        if let Some(m) = ctx
            .db
            .list_session_memories(&ctx.session_id)
            .await?
            .iter()
            .find(|m| m.id == id)
        {
            return Ok(entry_json(m));
        }
        if !ctx.folder_id.is_empty()
            && let Some(m) = ctx.db.get_folder_memory(&ctx.folder_id, id).await?
        {
            return Ok(long_json(&m));
        }
        anyhow::bail!("memory '{id}' not found (call memory_list for the current ids)")
    }

    pub(crate) async fn handle_memory_update(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let id = required_str(&args, "id", "memory_update")?;
        let content = optional_str(&args, "content");
        let is_session_note = ctx
            .db
            .list_session_memories(&ctx.session_id)
            .await?
            .iter()
            .any(|m| m.id == id);
        if is_session_note {
            let content = content
                .ok_or_else(|| anyhow::anyhow!("memory_update requires non-empty 'content'"))?;
            let hours = crate::routes::settings::short_term_memory_hours(&ctx.db).await;
            let entry = ctx
                .db
                .update_session_memory(&ctx.session_id, id, content, hours)
                .await?
                .ok_or_else(|| anyhow::anyhow!("memory '{id}' not found"))?;
            broadcast_change(ctx);
            return Ok(serde_json::json!({ "status": "ok", "entry": entry_json(&entry) }));
        }
        let title = optional_str(&args, "title");
        let category = optional_str(&args, "category");
        if content.is_none() && title.is_none() && category.is_none() {
            anyhow::bail!("memory_update needs 'content', 'title' or 'category' to change");
        }
        let title = title.map(|t| mask_shared(t).0);
        let content = content.map(|c| mask_shared(c).0);
        let entry = if ctx.folder_id.is_empty() {
            None
        } else {
            ctx.db
                .update_folder_memory(
                    &ctx.folder_id,
                    id,
                    category,
                    title.as_deref(),
                    content.as_deref(),
                )
                .await?
        }
        .ok_or_else(|| {
            anyhow::anyhow!(
                "memory '{id}' not found in this session's notes or this folder's long-term \
                 memory (call memory_list for the current ids)"
            )
        })?;
        broadcast_folder_change(ctx);
        Ok(serde_json::json!({ "status": "ok", "entry": long_json(&entry) }))
    }

    pub(crate) async fn handle_memory_remove(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let ids: Vec<String> = args
            .get("ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            anyhow::bail!("memory_remove requires a non-empty 'ids' array");
        }
        let own: std::collections::HashSet<String> = ctx
            .db
            .list_session_memories(&ctx.session_id)
            .await?
            .into_iter()
            .map(|m| m.id)
            .collect();
        let long: std::collections::HashSet<String> = if ctx.folder_id.is_empty() {
            Default::default()
        } else {
            ctx.db
                .list_folder_memories(&ctx.folder_id)
                .await?
                .into_iter()
                .map(|m| m.id)
                .collect()
        };
        let unknown: Vec<&String> = ids
            .iter()
            .filter(|id| !own.contains(*id) && !long.contains(*id))
            .collect();
        if !unknown.is_empty() {
            anyhow::bail!(
                "memory ids not found in this session's notes or this folder's long-term \
                 memory: {} (nothing was removed; call memory_list for the current ids)",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let session_ids: Vec<String> = ids.iter().filter(|id| own.contains(*id)).cloned().collect();
        let folder_ids: Vec<String> = ids
            .iter()
            .filter(|id| long.contains(*id))
            .cloned()
            .collect();
        let mut removed = 0;
        if !session_ids.is_empty() {
            removed += ctx
                .db
                .remove_session_memories(&ctx.session_id, &session_ids)
                .await?;
            broadcast_change(ctx);
        }
        if !folder_ids.is_empty() {
            removed += ctx
                .db
                .remove_folder_memories(&ctx.folder_id, &folder_ids)
                .await?;
            broadcast_folder_change(ctx);
        }
        Ok(serde_json::json!({ "status": "ok", "removed": removed }))
    }

    pub(crate) async fn handle_memory_compact(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let entries: Vec<String> = args
            .get("entries")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("memory_compact requires an 'entries' array"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("memory_compact 'entries' must be strings"))
            })
            .collect::<anyhow::Result<_>>()?;
        ctx.db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found"))?;
        let hours = crate::routes::settings::short_term_memory_hours(&ctx.db).await;
        let pool = ctx
            .db
            .compact_session_memories(&ctx.session_id, &entries, hours)
            .await?;
        broadcast_change(ctx);
        let mut out = pool_json(&pool);
        out["status"] = Value::from("ok");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::super::McpToolRegistry;
    use crate::db::models::{NewFolder, NewSession};
    use crate::service::mcp_server::context::ToolCallContext;

    /// Sessions s1 and s2 share folder f1; s3 lives in folder f2.
    async fn setup() -> (ToolCallContext, ToolCallContext, ToolCallContext) {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ts = chrono::Utc::now().to_rfc3339();
        for id in ["f1", "f2"] {
            db.create_folder(NewFolder {
                id: id.into(),
                name: id.into(),
                path: format!("/tmp/{id}"),
                created_at: ts.clone(),
            })
            .await
            .unwrap();
        }
        for (id, folder) in [("s1", "f1"), ("s2", "f1"), ("s3", "f2")] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: folder.into(),
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let broadcaster = crate::ws::broadcaster::Broadcaster::new();
        let ctx = |sid: &str, folder: &str| ToolCallContext {
            session_id: sid.into(),
            project_id: None,
            card_id: None,
            db: db.clone(),
            broadcaster: broadcaster.clone(),
            provider_registry: None,
            data_dir: None,
            folder_id: folder.into(),
            device_registry: None,
            background: None,
        };
        (ctx("s1", "f1"), ctx("s2", "f1"), ctx("s3", "f2"))
    }

    #[tokio::test]
    async fn short_term_notes_are_scoped_to_the_calling_session() {
        let (s1, s2, _s3) = setup().await;
        let reg = McpToolRegistry::new();
        let added = reg
            .handle_tool_call(
                "memory_add",
                serde_json::json!({ "content": "refactoring the parser" }),
                &s1,
            )
            .await
            .unwrap();
        assert_eq!(added["entry"]["term"], "short");
        assert!(added["entry"]["expires_at"].is_string());
        let id = added["entry"]["id"].as_str().unwrap().to_string();

        let listed = reg
            .handle_tool_call("memory_list", serde_json::json!({}), &s1)
            .await
            .unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["entries"][0]["id"], id);

        // Another session — even in the same folder — sees nothing and
        // cannot touch s1's note.
        let other = reg
            .handle_tool_call("memory_list", serde_json::json!({}), &s2)
            .await
            .unwrap();
        assert_eq!(other["count"], 0);
        let err = reg
            .handle_tool_call(
                "memory_update",
                serde_json::json!({ "id": id, "content": "x" }),
                &s2,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
        let err = reg
            .handle_tool_call("memory_remove", serde_json::json!({ "ids": [id] }), &s2)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
        assert_eq!(
            reg.handle_tool_call("memory_list", serde_json::json!({}), &s1)
                .await
                .unwrap()["count"],
            1
        );

        // The owner can.
        let out = reg
            .handle_tool_call("memory_remove", serde_json::json!({ "ids": [id] }), &s1)
            .await
            .unwrap();
        assert_eq!(out["removed"], 1);
    }

    #[tokio::test]
    async fn long_term_notes_are_shared_by_the_folder_and_hidden_from_others() {
        let (s1, s2, s3) = setup().await;
        let reg = McpToolRegistry::new();
        let added = reg
            .handle_tool_call(
                "memory_add",
                serde_json::json!({
                    "term": "long",
                    "category": "architecture",
                    "title": "One binary",
                    "content": "axum + sqlite; api token sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345",
                }),
                &s1,
            )
            .await
            .unwrap();
        let id = added["entry"]["id"].as_str().unwrap().to_string();
        assert!(
            !added["entry"]["content"]
                .as_str()
                .unwrap()
                .contains("abcdefghijklmnop"),
            "secret-shaped values are masked in shared notes: {added}"
        );

        // A second session in the same folder reads and edits it.
        let listed = reg
            .handle_tool_call("memory_list", serde_json::json!({}), &s2)
            .await
            .unwrap();
        assert_eq!(listed["count"], 0, "no session notes of its own");
        assert_eq!(listed["long_term"]["count"], 1);
        assert_eq!(listed["long_term"]["entries"][0]["title"], "One binary");
        let updated = reg
            .handle_tool_call(
                "memory_update",
                serde_json::json!({ "id": id, "content": "axum + diesel + sqlite" }),
                &s2,
            )
            .await
            .unwrap();
        assert_eq!(updated["entry"]["content"], "axum + diesel + sqlite");
        let read = reg
            .handle_tool_call("memory_read", serde_json::json!({ "id": id }), &s1)
            .await
            .unwrap();
        assert_eq!(read["content"], "axum + diesel + sqlite");

        // A session in another folder cannot see, read or remove it.
        let other = reg
            .handle_tool_call("memory_list", serde_json::json!({}), &s3)
            .await
            .unwrap();
        assert_eq!(other["long_term"]["count"], 0);
        for (tool, args) in [
            ("memory_read", serde_json::json!({ "id": id })),
            ("memory_remove", serde_json::json!({ "ids": [id] })),
        ] {
            let err = reg
                .handle_tool_call(tool, args, &s3)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("not found"), "{tool}: {err}");
        }

        // A long-term note needs a title and a known category.
        let err = reg
            .handle_tool_call(
                "memory_add",
                serde_json::json!({ "term": "long", "content": "x" }),
                &s1,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("title"), "got: {err}");
    }

    #[tokio::test]
    async fn compact_replaces_the_short_term_notes() {
        let (s1, _s2, _s3) = setup().await;
        let reg = McpToolRegistry::new();
        for content in ["a", "b"] {
            reg.handle_tool_call("memory_add", serde_json::json!({ "content": content }), &s1)
                .await
                .unwrap();
        }
        let out = reg
            .handle_tool_call(
                "memory_compact",
                serde_json::json!({ "entries": ["a and b"] }),
                &s1,
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "ok");
        assert_eq!(out["count"], 1);
        assert_eq!(out["entries"][0]["content"], "a and b");
    }
}
