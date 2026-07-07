//! Cost-aware model auto-switch tools: `get_model_guidance` and
//! `switch_session_model`. Visible only to sessions whose resolved
//! auto-switch toggle is ON — enforced at dispatch in `routes/mcp.rs`, the
//! same hard-gate shape the pre-hatcher allowlist uses (advertisement
//! trimming alone is not enough; a session could still call by name).
//!
//! The switch reuses the plain same-provider+account respawn: it writes the
//! new model (and optional library system prompt) onto the session row and
//! winds the child down after the current turn, so the orchestrator resumes
//! the SAME session (`--resume`) on the new model. Crossing a provider or
//! account boundary is refused — that would need a full handover, which the
//! worker respawn path deliberately doesn't do.

use serde_json::Value;

use super::super::McpToolRegistry;
use super::super::context::ToolCallContext;
use crate::handover::continuity_key;
use crate::provider::registry::ProviderRegistry;

/// Max self-service switches recorded for one session. A money-loop-adjacent
/// defense against a model flip-flopping between tiers indefinitely.
const SWITCH_CAP: usize = 3;

/// Utilization (%) above which the server nudges harder toward downgrading.
const DOWNGRADE_PRESSURE_PCT: f64 = 70.0;

/// Normalize a stored model id (possibly bare, possibly account-scoped) to a
/// full `provider:model[@account]` id so it can be matched against the
/// registry catalog, whose ids always carry the provider prefix.
fn full_model_id(model_id: &str) -> String {
    let (provider, rest) = ProviderRegistry::parse_model_id(model_id, "claude");
    format!("{provider}:{rest}")
}

/// Resolve whether auto-switch is ON for a session: an explicit toggle wins,
/// else the default (ON for workers, OFF for chats). Shared with the
/// dispatch gate in `routes/mcp.rs`.
pub fn autoswitch_enabled(model_autoswitch: Option<bool>, is_worker: bool) -> bool {
    model_autoswitch.unwrap_or(is_worker)
}

impl McpToolRegistry {
    pub(crate) async fn handle_get_model_guidance(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: get_model_guidance");
        let session = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found"))?;
        let registry = ctx
            .provider_registry
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("provider registry not available"))?;

        let current_model = session
            .model
            .clone()
            .ok_or_else(|| anyhow::anyhow!("session has no resolved model yet"))?;
        let current_full = full_model_id(&current_model);
        let (provider, account) = continuity_key(&current_model);

        // Build the catalog once; map full id -> (display, tier).
        let catalog = registry.list_all_models().await;
        let current_tier = catalog
            .iter()
            .find(|(id, _)| *id == current_full)
            .map(|(_, m)| m.tier);

        // Cheaper-but-capable candidates: same provider AND account, strictly
        // lower tier. Ordered by tier descending (closest capable first).
        let mut candidates: Vec<Value> = Vec::new();
        if let Some(cur_tier) = current_tier {
            let mut rows: Vec<(&String, &crate::provider::stream::ModelInfo)> = catalog
                .iter()
                .filter(|(id, m)| {
                    let (p, a) = continuity_key(id);
                    p == provider && a == account && m.tier < cur_tier && **id != current_full
                })
                .map(|(id, m)| (id, m))
                .collect();
            rows.sort_by_key(|(_, m)| std::cmp::Reverse(m.tier));
            for (id, m) in rows {
                candidates.push(serde_json::json!({
                    "model": id,
                    "display_name": m.display_name,
                    "tier": m.tier,
                }));
            }
        }

        // Plan usage is a Claude-only signal (subscription buckets). For
        // other providers there's nothing to report.
        let mut usage_value = Value::Null;
        let mut recommendation = "your_call";
        if provider == "claude" {
            let key = account
                .clone()
                .unwrap_or_else(|| crate::provider::claude::plan_usage::DEFAULT_KEY.to_string());
            let snap = crate::provider::claude::plan_usage::snapshot();
            if let Some(entry) = snap.get(&key)
                && let Some(usage) = &entry.usage
            {
                let buckets = [
                    usage.five_hour.as_ref(),
                    usage.seven_day.as_ref(),
                    usage.seven_day_sonnet.as_ref(),
                    usage.seven_day_opus.as_ref(),
                ];
                let peak = buckets
                    .iter()
                    .flatten()
                    .map(|b| b.utilization)
                    .fold(0.0_f64, f64::max);
                if peak >= DOWNGRADE_PRESSURE_PCT && !candidates.is_empty() {
                    recommendation = "downgrade_strongly";
                } else if !candidates.is_empty() {
                    recommendation = "downgrade_ok";
                }
                usage_value = serde_json::to_value(usage).unwrap_or(Value::Null);
            }
        } else if !candidates.is_empty() {
            recommendation = "downgrade_ok";
        }

        let prompts = ctx.db.list_system_prompts().await?;
        let prompt_list: Vec<Value> = prompts
            .iter()
            .map(|p| {
                let summary: String = p.body.split_whitespace().collect::<Vec<_>>().join(" ");
                let summary: String = summary.chars().take(140).collect();
                serde_json::json!({ "name": p.name, "summary": summary })
            })
            .collect();

        let switches_used = self.count_model_switches(ctx).await?;

        Ok(serde_json::json!({
            "current_model": current_full,
            "current_tier": current_tier,
            "provider": provider,
            "candidates": candidates,
            "plan_usage": usage_value,
            "recommendation": recommendation,
            "system_prompts": prompt_list,
            "switches_used": switches_used,
            "switch_cap": SWITCH_CAP,
            "note": "Switch ONLY if the plan is simple enough for the cheaper model to implement without problems. Pick a system_prompt_name matching the work type. You may also switch UP if the cheap model hits a wall.",
        }))
    }

    pub(crate) async fn handle_switch_session_model(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let target = args
            .get("model")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("switch_session_model requires 'model'"))?
            .trim()
            .to_string();
        let rationale = args
            .get("rationale")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("switch_session_model requires a non-empty 'rationale'")
            })?
            .to_string();
        let system_prompt_name = args
            .get("system_prompt_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let compact = args
            .get("compact")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        tracing::info!(
            session_id = %ctx.session_id,
            target = %target,
            "MCP tool: switch_session_model"
        );

        let session = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found"))?;
        if !autoswitch_enabled(session.model_autoswitch, session.is_worker) {
            anyhow::bail!("model auto-switch is disabled for this session");
        }
        let registry = ctx
            .provider_registry
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("provider registry not available"))?;

        let current_model = session
            .model
            .clone()
            .ok_or_else(|| anyhow::anyhow!("session has no resolved model to switch from"))?;
        let current_full = full_model_id(&current_model);
        let target_full = full_model_id(&target);
        if target_full == current_full {
            anyhow::bail!("already on {current_full}");
        }

        // Same provider AND account only: a plain respawn carries the
        // conversation via `--resume`; crossing a continuity boundary would
        // need a handover the worker respawn path doesn't perform.
        if continuity_key(&current_model) != continuity_key(&target) {
            anyhow::bail!(
                "can only switch within the same provider and account (no cross-provider/account handover on workers)"
            );
        }

        let catalog = registry.list_all_models().await;
        let from_tier = catalog
            .iter()
            .find(|(id, _)| *id == current_full)
            .map(|(_, m)| m.tier);
        let to_tier = match catalog.iter().find(|(id, _)| *id == target_full) {
            Some((_, m)) => m.tier,
            None => anyhow::bail!("unknown model: {target_full}"),
        };

        // Money-loop defense: cap self-service switches per session.
        let switches_used = self.count_model_switches(ctx).await?;
        if switches_used >= SWITCH_CAP {
            anyhow::bail!("switch cap reached ({SWITCH_CAP}); staying on {current_full}");
        }

        // Resolve an optional focusing system prompt from the library.
        let prompt_body = match &system_prompt_name {
            Some(name) => Some(
                ctx.db
                    .get_system_prompt_by_name(name)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no system prompt named '{name}'"))?
                    .body,
            ),
            None => None,
        };

        // Record the switch as a first-class event up front (drives the
        // report and the per-session cap). Broadcast so the live stream
        // shows it. Shared by both the plain and compacting paths.
        let data = serde_json::json!({
            "from": current_full,
            "to": target_full,
            "from_tier": from_tier,
            "to_tier": to_tier,
            "rationale": rationale,
            "system_prompt_name": system_prompt_name,
            "compact": compact,
        });
        if let Ok(ev) = ctx
            .db
            .append_event(&ctx.session_id, "model-switch", data)
            .await
        {
            ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "event".into(),
                session_id: ctx.session_id.clone(),
                data: serde_json::json!({
                    "id": ev.id,
                    "seq": ev.seq,
                    "ts": ev.ts,
                    "kind": ev.kind,
                    "data": serde_json::from_str::<Value>(&ev.data).unwrap_or_default(),
                }),
            });
        }

        if compact {
            // Faithful "finish → compact → upgrade" hop. Apply the focusing
            // prompt now (finalize_handover flips the model but preserves
            // system_prompt), then hand the ACTUAL model change to a
            // compacting handover: the outgoing model summarizes and the
            // incoming one resumes on that compacted context instead of the
            // full transcript. The handover needs the AppState/session
            // manager the mcp route holds, so we signal it with a
            // `_begin_handover` marker (mirrors the `_image_base64`
            // convention the route already unwraps).
            if prompt_body.is_some() {
                let update = crate::db::models::UpdateSession {
                    system_prompt: prompt_body.clone().map(Some),
                    system_prompt_name: system_prompt_name.clone().map(Some),
                    ..Default::default()
                };
                ctx.db.update_session(&ctx.session_id, update).await?;
            }
            return Ok(serde_json::json!({
                "status": "ok",
                "from": current_full,
                "to": target_full,
                "system_prompt_name": system_prompt_name,
                "switches_used": switches_used + 1,
                "switch_cap": SWITCH_CAP,
                "compact": true,
                "_begin_handover": { "from": current_full, "to": target_full },
                "note": "Compacting handover dispatched. Wrap up this turn — the outgoing model writes a summary, then the session resumes on the new model with that compacted context.",
            }));
        }

        // Plain path: write the new model (and prompt, if chosen) onto the
        // session row. The child is wound down below so the resume reads
        // these. When a library prompt was chosen, record its name alongside
        // the body so the reference column stays consistent; leave both
        // untouched when no prompt was selected (this tool only sets a
        // focusing prompt).
        let update = crate::db::models::UpdateSession {
            model: Some(Some(target_full.clone())),
            system_prompt: prompt_body.clone().map(Some),
            system_prompt_name: system_prompt_name.clone().map(Some),
            ..Default::default()
        };
        ctx.db.update_session(&ctx.session_id, update).await?;

        // Wind the current child down after this turn; the orchestrator
        // resumes the same session on the new model (and prompt).
        crate::provider::manager::shutdown_after_turn_via_registry(registry, &ctx.session_id).await;

        Ok(serde_json::json!({
            "status": "ok",
            "from": current_full,
            "to": target_full,
            "system_prompt_name": system_prompt_name,
            "switches_used": switches_used + 1,
            "switch_cap": SWITCH_CAP,
            "note": "Switch applied. Wrap up this turn — it takes effect when the session resumes on the new model.",
        }))
    }

    /// Count `model-switch` events already recorded for this session.
    async fn count_model_switches(&self, ctx: &ToolCallContext) -> anyhow::Result<usize> {
        let events = ctx.db.list_events_by_session(&ctx.session_id, None).await?;
        Ok(events.iter().filter(|e| e.kind == "model-switch").count())
    }
}
