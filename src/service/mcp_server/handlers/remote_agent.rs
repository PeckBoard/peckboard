//! `remote_agent_*` MCP tools — bridge session tool calls to enrolled
//! remote-control daemons (`peckboard-agent`) through the
//! [`crate::ws::agent::DeviceRegistry`].
//!
//! Every capability call is a thin pass-through: the server validates
//! `device_id`, ownership, and the device's kill-switch (`status`), then
//! forwards the remaining arguments verbatim as a
//! `ServerFrame::Request { capability, args }` and awaits the daemon's
//! `Result` via the registry's correlation-id bridge. Per-capability arg
//! validation and the local allow/deny config live in the daemon — it is
//! the authoritative gate for what runs on the user's machine.
//!
//! Session-role scoping is a DELIBERATE product decision (owner sign-off,
//! 2026-09-21 security review): worker sessions keep full access to these
//! tools — a card may legitimately say "deploy on my laptop". The layers
//! that contain a prompt-injected worker are (1) strict per-user device
//! ownership in [`resolve_device`], (2) the daemon's per-capability
//! allow/deny config + kill-switch (deny-by-default, only `echo` ships
//! enabled), and (3) the per-action `device_activity` audit log. Revisit
//! if devices ever become shareable across users.
//!
//! `remote_agent_echo` exists to prove the full
//! session → server → device → result loop with no OS executors; it stays
//! useful afterwards as a connectivity probe.

use std::time::Duration;

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::{Device, NewDeviceActivity, device_status};
use crate::service::mcp_server::context::ToolCallContext;
use crate::ws::agent::{DeviceRegistry, RequestError};

/// Wire capability names (must match the daemon's `Hello.capabilities`
/// entries and its executor dispatch).
const CAP_ECHO: &str = "echo";
const CAP_TERMINAL: &str = "terminal";
const CAP_SERVER: &str = "server";
const CAP_SCREENSHOT: &str = "screenshot";
const CAP_MOUSE: &str = "mouse";
const CAP_KEYBOARD: &str = "keyboard";

/// Default / maximum deadline for `remote_agent_run` (the daemon streams
/// long jobs as events later; the tool call itself is bounded).
const RUN_DEFAULT_SECS: u64 = 120;
const RUN_MAX_SECS: u64 = 600;

impl McpToolRegistry {
    /// Dispatch entry for every `remote_agent_*` tool.
    pub(crate) async fn handle_remote_agent_tool(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        match name {
            "remote_agent_list" => self.handle_remote_agent_list(ctx).await,
            "remote_agent_echo" => {
                self.remote_agent_call(ctx, args, CAP_ECHO, Duration::from_secs(10))
                    .await
            }
            "remote_agent_run" => {
                let secs = args
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(RUN_DEFAULT_SECS)
                    .clamp(1, RUN_MAX_SECS);
                self.remote_agent_call(ctx, args, CAP_TERMINAL, Duration::from_secs(secs))
                    .await
            }
            "remote_agent_server" => {
                self.remote_agent_call(ctx, args, CAP_SERVER, Duration::from_secs(60))
                    .await
            }
            "remote_agent_screenshot" => {
                let result = self
                    .remote_agent_call(
                        ctx,
                        normalize_screenshot_args(args),
                        CAP_SCREENSHOT,
                        Duration::from_secs(30),
                    )
                    .await?;
                Ok(image_result(result))
            }
            "remote_agent_mouse" => {
                self.remote_agent_call(ctx, args, CAP_MOUSE, Duration::from_secs(30))
                    .await
            }
            "remote_agent_keyboard" => {
                self.remote_agent_call(ctx, args, CAP_KEYBOARD, Duration::from_secs(30))
                    .await
            }
            _ => anyhow::bail!("unknown tool: {name}"),
        }
    }

    /// `remote_agent_list` — the caller's enrolled devices (never other
    /// users'), with live connection state from the registry.
    async fn handle_remote_agent_list(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        let registry = registry(ctx)?;
        let user_id = caller_user_id(ctx).await?;
        let devices = ctx.db.list_devices_by_user(&user_id).await?;
        let rows: Vec<Value> = devices
            .iter()
            .filter(|d| d.status != device_status::REVOKED)
            .map(|d| {
                serde_json::json!({
                    "device_id": d.id,
                    "name": d.name,
                    "platform": d.platform,
                    "status": d.status,
                    "online": registry.is_online(&d.id),
                    "in_flight": registry.in_flight(&d.id),
                    "last_seen_at": d.last_seen_at,
                })
            })
            .collect();
        Ok(serde_json::json!({ "devices": rows, "count": rows.len() }))
    }

    /// Resolve + authorize the target device, then forward `capability`
    /// with the caller's remaining args and await the daemon's reply.
    async fn remote_agent_call(
        &self,
        ctx: &ToolCallContext,
        args: Value,
        capability: &str,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let registry = registry(ctx)?;
        let device_id = args
            .get("device_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("device_id is required (see remote_agent_list)"))?
            .to_string();
        let device = resolve_device(ctx, &device_id).await?;

        // Forward everything except the routing field; the daemon owns
        // per-capability argument validation.
        let mut payload = args;
        if let Some(o) = payload.as_object_mut() {
            o.remove("device_id");
        }

        tracing::info!(
            session_id = %ctx.session_id,
            device_id = %device.id,
            capability,
            "MCP tool: remote_agent request"
        );

        let summary = args_summary(capability, &payload);
        let outcome = registry
            .send_request(&ctx.broadcaster, &device.id, capability, payload, timeout)
            .await;

        // Audit log — one row per bridged action, success or failure
        // (`GET /api/devices/:id/activity` renders these). Best-effort: a
        // failed write must not fail the action itself.
        let status = match &outcome {
            Ok(_) => "ok",
            Err(RequestError::Offline) => "offline",
            Err(RequestError::Disconnected) => "disconnected",
            Err(RequestError::Timeout) => "timeout",
            Err(RequestError::Agent(_)) => "error",
        };
        if let Err(e) = ctx
            .db
            .insert_device_activity(NewDeviceActivity {
                id: uuid::Uuid::new_v4().to_string(),
                device_id: device.id.clone(),
                session_id: Some(ctx.session_id.clone()),
                capability: capability.to_string(),
                args_summary: summary,
                status: status.to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .await
        {
            tracing::warn!(device_id = %device.id, "device_activity write failed: {e}");
        }

        match outcome {
            Ok(result) => Ok(serde_json::json!({
                "ok": true,
                "device_id": device.id,
                "device_name": device.name,
                "result": result,
            })),
            Err(RequestError::Offline) => anyhow::bail!(
                "device '{}' is offline — the peckboard-agent daemon is not connected",
                device.name
            ),
            Err(RequestError::Disconnected) => anyhow::bail!(
                "device '{}' disconnected before replying; retry once it reconnects",
                device.name
            ),
            Err(RequestError::Timeout) => anyhow::bail!(
                "device '{}' did not reply within {}s (a cancel was sent)",
                device.name,
                timeout.as_secs()
            ),
            Err(RequestError::Agent(msg)) => {
                anyhow::bail!(
                    "device '{}' refused or failed the request: {msg}",
                    device.name
                )
            }
        }
    }
}

/// The registry handle, or a clear error on dispatch paths that don't
/// carry one (in-process plugin-provider tool calls, unit tests).
fn registry(ctx: &ToolCallContext) -> anyhow::Result<&DeviceRegistry> {
    ctx.device_registry
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("remote agent tools are unavailable on this dispatch path"))
}

/// The caller's user id: the session's owner, falling back to the sole
/// user on single-user installs. Devices are per-user; a session with no
/// resolvable owner gets nothing rather than everything.
async fn caller_user_id(ctx: &ToolCallContext) -> anyhow::Result<String> {
    ctx.db
        .resolve_spawned_session_owner(Some(&ctx.session_id))
        .await
        .ok_or_else(|| {
            anyhow::anyhow!("no user is associated with this session; remote agents are per-user")
        })
}

/// Ownership + kill-switch gate. Unknown ids and other users' devices
/// both return the same "not found" (no existence oracle); a disabled or
/// revoked device is refused server-side even if its socket is still up.
async fn resolve_device(ctx: &ToolCallContext, device_id: &str) -> anyhow::Result<Device> {
    let user_id = caller_user_id(ctx).await?;
    let device = ctx
        .db
        .get_device(device_id)
        .await?
        .filter(|d| d.user_id == user_id)
        .ok_or_else(|| anyhow::anyhow!("device not found: {device_id}"))?;
    if device.status != device_status::ACTIVE {
        anyhow::bail!(
            "device '{}' is {} — re-enable it from the Agents panel first",
            device.name,
            device.status
        );
    }
    Ok(device)
}

/// The daemon's screenshot executor selects a display via `monitor`; an
/// earlier tool schema called the field `display` (and the daemon
/// silently ignored it). Accept both, preferring an explicit `monitor`,
/// and never forward `display`.
fn normalize_screenshot_args(mut args: Value) -> Value {
    if let Some(o) = args.as_object_mut()
        && let Some(d) = o.remove("display")
        && !o.contains_key("monitor")
    {
        o.insert("monitor".into(), d);
    }
    args
}

/// Map a screenshot payload onto the `_image_base64` convention
/// (`routes/mcp.rs` turns it into an MCP image content block). Payloads
/// without an image pass through untouched so daemon-side errors stay
/// readable.
fn image_result(mut result: Value) -> Value {
    let image = result
        .get_mut("result")
        .and_then(|r| r.as_object_mut())
        .and_then(|o| {
            let data = o.remove("image_base64")?;
            let mime = o
                .remove("mime")
                .and_then(|m| m.as_str().map(str::to_string))
                .unwrap_or_else(|| "image/png".into());
            Some((data, mime))
        });
    if let Some((data, mime)) = image
        && let Some(o) = result.as_object_mut()
    {
        o.insert("_image_base64".into(), data);
        o.insert("_image_mime".into(), Value::String(mime));
    }
    result
}

/// Short, redacted summary of a bridged request's arguments for the
/// audit log. Keyboard `text` is never recorded — it can carry secrets
/// typed onto the remote machine; everything else is the serialized
/// payload truncated to 200 chars.
fn args_summary(capability: &str, payload: &Value) -> String {
    let mut p = payload.clone();
    if capability == CAP_KEYBOARD
        && let Some(o) = p.as_object_mut()
        && o.contains_key("text")
    {
        o.insert("text".into(), Value::String("[redacted]".into()));
    }
    let s = serde_json::to_string(&p).unwrap_or_default();
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_display_arg_aliases_to_monitor() {
        // Legacy `display` becomes `monitor`.
        let v = normalize_screenshot_args(serde_json::json!({ "display": 1 }));
        assert_eq!(v, serde_json::json!({ "monitor": 1 }));

        // An explicit `monitor` wins; `display` is never forwarded.
        let v = normalize_screenshot_args(serde_json::json!({ "monitor": 2, "display": 1 }));
        assert_eq!(v, serde_json::json!({ "monitor": 2 }));

        // No selector at all passes through untouched.
        let v = normalize_screenshot_args(serde_json::json!({ "list": true }));
        assert_eq!(v, serde_json::json!({ "list": true }));
    }

    #[test]
    fn screenshot_payload_maps_to_the_image_convention() {
        let mapped = image_result(serde_json::json!({
            "ok": true,
            "device_id": "d1",
            "result": { "image_base64": "aGVsbG8=", "mime": "image/jpeg", "display": 0 }
        }));
        assert_eq!(mapped["_image_base64"], "aGVsbG8=");
        assert_eq!(mapped["_image_mime"], "image/jpeg");
        assert!(
            mapped["result"].get("image_base64").is_none(),
            "image bytes must not be duplicated in the text block"
        );
        assert_eq!(mapped["result"]["display"], 0);
    }

    #[test]
    fn non_image_payload_passes_through_untouched() {
        let v = serde_json::json!({ "ok": true, "result": { "note": "no screen" } });
        assert_eq!(image_result(v.clone()), v);
    }

    #[test]
    fn args_summary_redacts_keyboard_text_and_truncates() {
        let kb = args_summary(CAP_KEYBOARD, &serde_json::json!({ "text": "hunter2" }));
        assert!(!kb.contains("hunter2"));
        assert!(kb.contains("[redacted]"));

        let long = args_summary(
            CAP_TERMINAL,
            &serde_json::json!({ "command": "x".repeat(500) }),
        );
        assert!(long.chars().count() <= 201, "truncated to 200 + ellipsis");
        assert!(long.ends_with('…'));

        let short = args_summary(CAP_ECHO, &serde_json::json!({ "text": "hi" }));
        assert_eq!(short, "{\"text\":\"hi\"}");
    }
}
