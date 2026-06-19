//! Per-turn token usage extraction from the Claude CLI stream.
//!
//! Accuracy notes, verified against the CLI's actual output:
//!
//! - The `result` event's `modelUsage` map is the authoritative per-model
//!   account of a turn — it includes subagent (Task tool) and utility-call
//!   tokens that the top-level `usage` object (main loop only) misses.
//! - `modelUsage` is CUMULATIVE across turns within one long-running CLI
//!   process, so a turn's tokens are the delta against the previous
//!   `result`'s snapshot.
//! - Every `assistant` stream event carries that API call's usage in
//!   `message.usage`, snapshotted per content block (same `message.id`,
//!   latest wins). Accumulating these covers the crash path: a turn that
//!   dies before its `result` still gets its tokens recorded.

use std::collections::HashMap;

/// The four billed token slices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct UsageSlices {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
}

impl UsageSlices {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }

    pub fn context(&self) -> i64 {
        self.input + self.cache_read + self.cache_creation
    }

    fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    fn saturating_sub(self, other: Self) -> Self {
        Self {
            input: (self.input - other.input).max(0),
            output: (self.output - other.output).max(0),
            cache_read: (self.cache_read - other.cache_read).max(0),
            cache_creation: (self.cache_creation - other.cache_creation).max(0),
        }
    }

    fn add(&mut self, other: Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_creation += other.cache_creation;
    }

    /// Parse from an API-shaped usage object (snake_case keys, as found in
    /// `result.usage` and `assistant.message.usage`).
    fn from_api_usage(usage: &serde_json::Value) -> Self {
        let field = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        Self {
            input: field("input_tokens"),
            output: field("output_tokens"),
            cache_read: field("cache_read_input_tokens"),
            cache_creation: field("cache_creation_input_tokens"),
        }
    }

    /// Parse from a `modelUsage` entry (camelCase keys).
    fn from_model_usage(usage: &serde_json::Value) -> Self {
        let field = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        Self {
            input: field("inputTokens"),
            output: field("outputTokens"),
            cache_read: field("cacheReadInputTokens"),
            cache_creation: field("cacheCreationInputTokens"),
        }
    }
}

/// One model's share of a turn, ready to become a `ProviderEvent::Usage`.
#[derive(Debug, Clone)]
pub(super) struct TurnModelUsage {
    pub model: Option<String>,
    pub slices: UsageSlices,
    /// Context-window occupancy at end of turn — only meaningful for the
    /// session's main model; 0 for subagent/utility models.
    pub context_tokens: i64,
}

/// Tracks token usage across one CLI process lifetime.
#[derive(Default)]
pub(super) struct UsageTracker {
    /// Cumulative per-model snapshot as of the last `result` event.
    cumulative: HashMap<String, UsageSlices>,
    /// Latest per-message usage snapshot this turn (crash fallback);
    /// keyed by `message.id`, later snapshots overwrite earlier ones.
    turn_messages: HashMap<String, (Option<String>, UsageSlices)>,
}

impl UsageTracker {
    /// Record per-message usage from an `assistant` stream event. Call on
    /// every stdout line; non-assistant lines are ignored.
    pub fn observe_line(&mut self, json: &serde_json::Value) {
        if json.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            return;
        }
        let Some(msg) = json.get("message") else {
            return;
        };
        let Some(id) = msg.get("id").and_then(|v| v.as_str()) else {
            return;
        };
        let Some(usage) = msg.get("usage") else {
            return;
        };
        let model = msg
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        self.turn_messages
            .insert(id.to_string(), (model, UsageSlices::from_api_usage(usage)));
    }

    /// Settle a turn from its `result` event. Prefers the per-model
    /// `modelUsage` deltas; falls back to the main-loop `usage` object, then
    /// to the accumulated per-message snapshots. Clears the per-turn state.
    pub fn on_result(
        &mut self,
        json: &serde_json::Value,
        main_model: Option<&str>,
    ) -> Vec<TurnModelUsage> {
        let main_context = json
            .get("usage")
            .map(|u| UsageSlices::from_api_usage(u).context());

        let from_model_usage = json
            .get("modelUsage")
            .and_then(|v| v.as_object())
            .map(|models| {
                let mut out: Vec<TurnModelUsage> = Vec::new();
                for (model, v) in models {
                    let current = UsageSlices::from_model_usage(v);
                    let prev = self
                        .cumulative
                        .insert(model.clone(), current)
                        .unwrap_or_default();
                    let delta = current.saturating_sub(prev);
                    if !delta.is_zero() {
                        out.push(TurnModelUsage {
                            model: Some(model.clone()),
                            slices: delta,
                            context_tokens: 0,
                        });
                    }
                }
                // The main-loop context occupancy belongs on the session's
                // main model row; subagent/utility models carry no session
                // context. If no row matches (model id drift), pin it to the
                // largest row so the gauge never silently reads zero.
                if let Some(ctx) = main_context
                    && !out.is_empty()
                {
                    let idx = out
                        .iter()
                        .position(|u| u.model.as_deref() == main_model)
                        .unwrap_or_else(|| {
                            out.iter()
                                .enumerate()
                                .max_by_key(|(_, u)| u.slices.context())
                                .map(|(i, _)| i)
                                .unwrap_or(0)
                        });
                    out[idx].context_tokens = ctx;
                }
                out
            })
            .filter(|out| !out.is_empty());

        let usages = if let Some(out) = from_model_usage {
            out
        } else if let Some(usage) = json.get("usage") {
            let slices = UsageSlices::from_api_usage(usage);
            if slices.is_zero() {
                Vec::new()
            } else {
                vec![TurnModelUsage {
                    model: main_model.map(|s| s.to_string()),
                    context_tokens: slices.context(),
                    slices,
                }]
            }
        } else {
            self.aggregate_turn_messages(main_model)
        };

        self.turn_messages.clear();
        usages
    }

    /// Crash fallback: the process died before this turn's `result`. Settle
    /// whatever the per-message snapshots saw so the tokens aren't lost.
    pub fn take_crash_fallback(&mut self, main_model: Option<&str>) -> Vec<TurnModelUsage> {
        let usages = self.aggregate_turn_messages(main_model);
        self.turn_messages.clear();
        usages
    }

    fn aggregate_turn_messages(&self, main_model: Option<&str>) -> Vec<TurnModelUsage> {
        let mut by_model: Vec<(Option<String>, UsageSlices, i64)> = Vec::new();
        for (model, slices) in self.turn_messages.values() {
            match by_model.iter_mut().find(|(m, _, _)| m == model) {
                Some((_, sum, ctx)) => {
                    sum.add(*slices);
                    *ctx = (*ctx).max(slices.context());
                }
                None => by_model.push((model.clone(), *slices, slices.context())),
            }
        }
        by_model
            .into_iter()
            .filter(|(_, slices, _)| !slices.is_zero())
            .map(|(model, slices, ctx)| {
                let is_main = model.is_none() || model.as_deref() == main_model;
                TurnModelUsage {
                    model,
                    slices,
                    context_tokens: if is_main { ctx } else { 0 },
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_event(model_usage: serde_json::Value, usage: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "type": "result", "modelUsage": model_usage, "usage": usage })
    }

    #[test]
    fn model_usage_is_diffed_across_turns() {
        let mut tracker = UsageTracker::default();
        let turn1 = result_event(
            serde_json::json!({
                "claude-haiku-4-5": { "inputTokens": 450, "outputTokens": 56,
                    "cacheReadInputTokens": 18465, "cacheCreationInputTokens": 6304 }
            }),
            serde_json::json!({ "input_tokens": 10, "output_tokens": 44,
                "cache_read_input_tokens": 18465, "cache_creation_input_tokens": 6304 }),
        );
        let usages = tracker.on_result(&turn1, Some("claude-haiku-4-5"));
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].slices.input, 450);
        assert_eq!(usages[0].slices.output, 56);
        assert_eq!(usages[0].slices.cache_read, 18465);
        // Context comes from the main-loop usage object.
        assert_eq!(usages[0].context_tokens, 10 + 18465 + 6304);

        // Second turn: modelUsage is cumulative; only the delta is recorded.
        let turn2 = result_event(
            serde_json::json!({
                "claude-haiku-4-5": { "inputTokens": 460, "outputTokens": 104,
                    "cacheReadInputTokens": 43234, "cacheCreationInputTokens": 7388 }
            }),
            serde_json::json!({ "input_tokens": 10, "output_tokens": 48,
                "cache_read_input_tokens": 24769, "cache_creation_input_tokens": 1084 }),
        );
        let usages = tracker.on_result(&turn2, Some("claude-haiku-4-5"));
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].slices.input, 10);
        assert_eq!(usages[0].slices.output, 48);
        assert_eq!(usages[0].slices.cache_read, 43234 - 18465);
        assert_eq!(usages[0].slices.cache_creation, 7388 - 6304);
    }

    #[test]
    fn subagent_models_get_their_own_rows_without_context() {
        let mut tracker = UsageTracker::default();
        let result = result_event(
            serde_json::json!({
                "claude-opus-4-8": { "inputTokens": 100, "outputTokens": 200,
                    "cacheReadInputTokens": 5000, "cacheCreationInputTokens": 300 },
                "claude-haiku-4-5": { "inputTokens": 40, "outputTokens": 10,
                    "cacheReadInputTokens": 0, "cacheCreationInputTokens": 0 }
            }),
            serde_json::json!({ "input_tokens": 100, "output_tokens": 200,
                "cache_read_input_tokens": 5000, "cache_creation_input_tokens": 300 }),
        );
        let mut usages = tracker.on_result(&result, Some("claude-opus-4-8"));
        usages.sort_by_key(|u| u.model.clone());
        assert_eq!(usages.len(), 2);
        let haiku = &usages[0];
        let opus = &usages[1];
        assert_eq!(opus.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(opus.context_tokens, 100 + 5000 + 300);
        assert_eq!(haiku.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(haiku.slices.input, 40);
        assert_eq!(haiku.context_tokens, 0);
    }

    #[test]
    fn falls_back_to_usage_object_when_model_usage_missing() {
        let mut tracker = UsageTracker::default();
        let result = serde_json::json!({ "type": "result",
            "usage": { "input_tokens": 9, "output_tokens": 38,
                "cache_read_input_tokens": 18465, "cache_creation_input_tokens": 6308 } });
        let usages = tracker.on_result(&result, Some("m1"));
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].model.as_deref(), Some("m1"));
        assert_eq!(usages[0].slices.total(), 9 + 38 + 18465 + 6308);
    }

    #[test]
    fn crash_fallback_settles_per_message_usage() {
        let mut tracker = UsageTracker::default();
        // Two snapshots of the same message (later wins), then a second
        // message from a subagent model.
        for output in [3, 7] {
            tracker.observe_line(&serde_json::json!({
                "type": "assistant",
                "message": { "id": "msg_1", "model": "m1",
                    "usage": { "input_tokens": 10, "output_tokens": output,
                        "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5 } }
            }));
        }
        tracker.observe_line(&serde_json::json!({
            "type": "assistant",
            "message": { "id": "msg_2", "model": "m2",
                "usage": { "input_tokens": 4, "output_tokens": 2,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } }
        }));

        let mut usages = tracker.take_crash_fallback(Some("m1"));
        usages.sort_by_key(|u| u.model.clone());
        assert_eq!(usages.len(), 2);
        assert_eq!(usages[0].model.as_deref(), Some("m1"));
        assert_eq!(usages[0].slices.output, 7, "latest snapshot wins");
        assert_eq!(usages[0].context_tokens, 10 + 100 + 5);
        assert_eq!(usages[1].model.as_deref(), Some("m2"));
        assert_eq!(usages[1].context_tokens, 0);

        // Cleared after take — a second call settles nothing.
        assert!(tracker.take_crash_fallback(Some("m1")).is_empty());
    }

    #[test]
    fn result_clears_crash_fallback_state() {
        let mut tracker = UsageTracker::default();
        tracker.observe_line(&serde_json::json!({
            "type": "assistant",
            "message": { "id": "msg_1", "model": "m1",
                "usage": { "input_tokens": 10, "output_tokens": 3,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } }
        }));
        let result = serde_json::json!({ "type": "result",
            "usage": { "input_tokens": 10, "output_tokens": 3,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } });
        assert_eq!(tracker.on_result(&result, Some("m1")).len(), 1);
        assert!(
            tracker.take_crash_fallback(Some("m1")).is_empty(),
            "settled turn must not double-count on a later crash"
        );
    }
}
