//! `memory_*` MCP tools: the calling session's durable memory pool.
//!
//! Every tool resolves the pool from `ctx.session_id` — the verified
//! session behind the bearer token — so an agent can only ever read or
//! write its own memories. Ids from another session come back as "not
//! found", exactly like unknown ids.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::SessionMemory;
use crate::service::mcp_server::context::ToolCallContext;

fn entry_json(m: &SessionMemory) -> Value {
    serde_json::json!({
        "id": m.id,
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
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{tool} requires non-empty '{key}'"))
}

/// Tell open UIs (the Memory modal) that this session's pool changed.
fn broadcast_change(ctx: &ToolCallContext) {
    ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "session-memory".into(),
        session_id: ctx.session_id.clone(),
        data: serde_json::json!({ "session_id": ctx.session_id }),
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
        let entry = ctx.db.add_session_memory(&ctx.session_id, content).await?;
        broadcast_change(ctx);
        Ok(serde_json::json!({
            "status": "ok",
            "entry": entry_json(&entry),
            "note": "Saved. It will appear in your Session memory section after the next \
                     clear, resume, or compaction; you already know it for this conversation.",
        }))
    }

    pub(crate) async fn handle_memory_list(
        &self,
        _args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let entries = ctx.db.list_session_memories(&ctx.session_id).await?;
        Ok(pool_json(&entries))
    }

    pub(crate) async fn handle_memory_update(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let id = required_str(&args, "id", "memory_update")?;
        let content = required_str(&args, "content", "memory_update")?;
        let entry = ctx
            .db
            .update_session_memory(&ctx.session_id, id, content)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("memory '{id}' not found in this session's pool (call memory_list for the current ids)")
            })?;
        broadcast_change(ctx);
        Ok(serde_json::json!({ "status": "ok", "entry": entry_json(&entry) }))
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
        let unknown: Vec<&String> = ids.iter().filter(|id| !own.contains(*id)).collect();
        if !unknown.is_empty() {
            anyhow::bail!(
                "memory ids not found in this session's pool: {} (nothing was removed; call memory_list for the current ids)",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let removed = ctx
            .db
            .remove_session_memories(&ctx.session_id, &ids)
            .await?;
        broadcast_change(ctx);
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
        let pool = ctx
            .db
            .compact_session_memories(&ctx.session_id, &entries)
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

    async fn setup() -> (ToolCallContext, ToolCallContext, Arc<crate::db::Db>) {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        for id in ["s1", "s2"] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let broadcaster = crate::ws::broadcaster::Broadcaster::new();
        let ctx = |sid: &str| ToolCallContext {
            session_id: sid.into(),
            project_id: None,
            card_id: None,
            db: db.clone(),
            broadcaster: broadcaster.clone(),
            provider_registry: None,
            data_dir: None,
            folder_id: "f1".into(),
            device_registry: None,
            background: None,
        };
        (ctx("s1"), ctx("s2"), db)
    }

    #[tokio::test]
    async fn tools_are_scoped_to_the_calling_session() {
        let (s1, s2, _db) = setup().await;
        let reg = McpToolRegistry::new();
        let added = reg
            .handle_tool_call(
                "memory_add",
                serde_json::json!({ "content": "prefers dark mode" }),
                &s1,
            )
            .await
            .unwrap();
        let id = added["entry"]["id"].as_str().unwrap().to_string();

        let listed = reg
            .handle_tool_call("memory_list", serde_json::json!({}), &s1)
            .await
            .unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["entries"][0]["id"], id);

        // Another session sees nothing and cannot touch s1's entry.
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
    async fn compact_replaces_the_pool() {
        let (s1, _s2, db) = setup().await;
        let reg = McpToolRegistry::new();
        db.add_session_memory("s1", "a").await.unwrap();
        db.add_session_memory("s1", "b").await.unwrap();
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
