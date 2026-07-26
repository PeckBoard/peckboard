use serde::{Deserialize, Serialize};

use crate::todo::TodoItem;

/// One image returned by a tool (e.g. a Playwright MCP screenshot). The CLI
/// hands these to us inline as base64 inside the `tool_result` content
/// blocks; we carry them on `ToolEnd` so the chat can render them under the
/// tool block. `data_base64` is the raw base64 payload (no `data:` prefix)
/// and `mime_type` is the source media type (`image/png`, `image/jpeg`, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime_type: String,
    pub data_base64: String,
}

/// Machine-readable classification of a [`ProviderEvent::Crashed`].
///
/// `reason` stays the human-readable text; this is what code branches on
/// instead of substring-matching that text. Serialized additively —
/// `error_kind` on the struct, `errorKind` in the event data — with a
/// `#[serde(default)]` of [`CrashKind::Unknown`], so events persisted
/// before the taxonomy existed, and plugin providers that never set it,
/// still deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashKind {
    /// Credentials missing, expired or rejected — the user must re-login;
    /// retrying as-is fails the same way.
    AuthExpired,
    /// The provider throttled us or the quota is exhausted — retry later.
    RateLimit,
    /// The turn exceeded a wall-clock bound and was killed.
    Timeout,
    /// Someone cancelled the run (user, watchdog, project pause). Not an
    /// agent failure.
    Interrupted,
    /// The CLI / child process never started.
    SpawnFailed,
    /// The child died after the turn began but before it settled.
    ExitedMidTurn,
    /// The process exited without ever producing usable output.
    NoOutput,
    /// Nothing more specific could be determined.
    #[default]
    Unknown,
}

impl CrashKind {
    /// Stable wire string — identical to the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            CrashKind::AuthExpired => "auth_expired",
            CrashKind::RateLimit => "rate_limit",
            CrashKind::Timeout => "timeout",
            CrashKind::Interrupted => "interrupted",
            CrashKind::SpawnFailed => "spawn_failed",
            CrashKind::ExitedMidTurn => "exited_mid_turn",
            CrashKind::NoOutput => "no_output",
            CrashKind::Unknown => "unknown",
        }
    }

    /// Best-effort classification of free-form provider text — a stderr
    /// tail, an API error body, a CLI message. Returns
    /// [`CrashKind::Unknown`] when nothing matches; callers that know a
    /// structural fallback (from *how* the run died) should use
    /// [`CrashKind::classify_or`].
    pub fn classify(text: &str) -> CrashKind {
        let text = text.to_ascii_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
        // Rate limiting first: a 429 body often also names the API key.
        if has(&[
            "429",
            "rate limit",
            "rate_limit",
            "too many requests",
            "quota",
            "usage limit",
            "overloaded",
        ]) {
            return CrashKind::RateLimit;
        }
        if has(&[
            "401",
            "unauthorized",
            "unauthenticated",
            "authenticate",
            "authentication",
            "invalid api key",
            "invalid_api_key",
            "api key",
            "credential",
            "not signed in",
            "isn't signed in",
            "no model configured",
            "login",
            "oauth",
            "token expired",
            "expired token",
        ]) {
            return CrashKind::AuthExpired;
        }
        if has(&["timed out", "timeout"]) {
            return CrashKind::Timeout;
        }
        if has(&["interrupted", "cancelled", "canceled"]) {
            return CrashKind::Interrupted;
        }
        CrashKind::Unknown
    }

    /// [`classify`](Self::classify), falling back to a structural kind when
    /// the text carries nothing recognizable.
    pub fn classify_or(text: &str, fallback: CrashKind) -> CrashKind {
        CrashKind::classify(text).or(fallback)
    }

    /// This kind, or `fallback` when it is [`CrashKind::Unknown`]. Lets a
    /// caller prefer a text sniff and fall back to the structural kind it
    /// derived from *how* the run died.
    pub fn or(self, fallback: CrashKind) -> CrashKind {
        match self {
            CrashKind::Unknown => fallback,
            kind => kind,
        }
    }
}

/// Unified event stream from any AI provider.
/// Providers translate their native output format into these events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// Agent initialized / started running.
    Started {
        model: String,
        conversation_id: Option<String>,
        #[serde(default)]
        metadata: serde_json::Value,
    },
    /// Streamed text chunk.
    Text { text: String },
    /// Streamed reasoning/thinking chunk (extended-thinking models). Shown
    /// collapsed in the chat; never part of the assistant's answer text.
    Thinking { text: String },
    /// Agent invoked a tool.
    ToolStart {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool finished.
    ToolEnd {
        tool_use_id: String,
        output: Option<String>,
        error: Option<String>,
        /// Images the tool returned (e.g. Playwright MCP screenshots).
        /// Empty for the overwhelming majority of tools; carried inline so
        /// the chat can render them under the tool block.
        #[serde(default)]
        images: Vec<ToolImage>,
    },
    /// The agent reported its current todo list (a full replace-all snapshot
    /// of its trackable work items). Provider-agnostic — any provider that can
    /// surface work items emits this; the latest one wins.
    Todo { todos: Vec<TodoItem> },
    /// Per-turn token usage rollup. Emitted at end of turn from the
    /// provider's `result` (or, on a crash, from accumulated per-message
    /// usage), just before `Completed`/`Crashed`.
    /// `context_tokens` is the context-window size at end of turn
    /// (input + cache_read + cache_creation); `total_tokens` adds the
    /// generated output on top. Providers that don't expose usage simply
    /// never emit this. Mirrored into the `usage_events` table by
    /// `emit_event`, the same way `Todo` is mirrored into `todos`.
    /// A turn that used several models emits one event per model; the
    /// provider stamps them all with the same `turn_seq` so they roll up
    /// as a single turn. `None` lets the DB layer auto-assign the next
    /// per-session turn number.
    Usage {
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_creation_tokens: i64,
        total_tokens: i64,
        context_tokens: i64,
        model: Option<String>,
        #[serde(default)]
        turn_seq: Option<i32>,
    },
    /// A provider-level system notice worth showing in the chat (e.g. the
    /// Claude CLI reporting that it compacted the conversation mid-session).
    /// Persisted under the existing `system` event kind, so the chat's
    /// system row renders `text` with no frontend change.
    System {
        /// Human-readable label shown in the chat.
        text: String,
        /// The provider's own subtype for the notice (e.g. `compact_boundary`).
        subtype: String,
        /// Raw provider frame, kept for debugging / richer rendering later.
        #[serde(default)]
        detail: serde_json::Value,
    },
    /// Agent finished normally.
    Completed {
        conversation_id: Option<String>,
        /// Additive per-turn metadata from the provider's result frame
        /// (`durationMs`, `numTurns`, `totalCostUsd`, `permissionDenials`).
        /// Its object keys are merged into the event data on top of the
        /// existing ones; `Null` when the provider reports none.
        #[serde(default)]
        result_meta: serde_json::Value,
    },
    /// Agent failed / crashed.
    Crashed {
        reason: String,
        /// Machine-readable classification of `reason`. Additive: absent on
        /// the wire (old plugin payloads, replayed events) means
        /// [`CrashKind::Unknown`].
        #[serde(default)]
        error_kind: CrashKind,
        exit_code: Option<i32>,
        stderr: Option<String>,
    },
    /// Agent requesting permission or user input.
    ControlRequest {
        request_id: String,
        request_type: String,
        payload: serde_json::Value,
    },
}

impl ProviderEvent {
    /// Map this provider event to an event log kind string.
    pub fn event_kind(&self) -> &'static str {
        match self {
            ProviderEvent::Started { .. } => "agent-start",
            ProviderEvent::Text { .. } => "agent-text",
            ProviderEvent::Thinking { .. } => "agent-thinking",
            ProviderEvent::ToolStart { .. } => "agent-tool-start",
            ProviderEvent::ToolEnd { .. } => "agent-tool-end",
            ProviderEvent::Todo { .. } => "todo",
            ProviderEvent::Usage { .. } => "agent-usage",
            ProviderEvent::System { .. } => "system",
            ProviderEvent::Completed { .. } => "agent-end",
            ProviderEvent::Crashed { .. } => "agent-end",
            ProviderEvent::ControlRequest { .. } => "question",
        }
    }

    /// Convert to event log data JSON.
    pub fn event_data(&self) -> serde_json::Value {
        match self {
            ProviderEvent::Started {
                model,
                conversation_id,
                metadata,
            } => serde_json::json!({
                "model": model,
                "conversationId": conversation_id,
                "metadata": metadata,
            }),
            ProviderEvent::Text { text } => serde_json::json!({ "text": text }),
            ProviderEvent::Thinking { text } => serde_json::json!({ "text": text }),
            ProviderEvent::ToolStart {
                tool_use_id,
                name,
                input,
            } => serde_json::json!({
                "toolUseId": tool_use_id,
                "name": name,
                "input": input,
            }),
            ProviderEvent::ToolEnd {
                tool_use_id,
                output,
                error,
                images,
            } => serde_json::json!({
                "toolUseId": tool_use_id,
                "output": output,
                "error": error,
                "images": images
                    .iter()
                    .map(|img| serde_json::json!({
                        "mimeType": img.mime_type,
                        "dataBase64": img.data_base64,
                    }))
                    .collect::<Vec<_>>(),
            }),
            ProviderEvent::Todo { todos } => serde_json::json!({ "todos": todos }),
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                total_tokens,
                context_tokens,
                model,
                turn_seq,
            } => serde_json::json!({
                "inputTokens": input_tokens,
                "outputTokens": output_tokens,
                "cacheReadTokens": cache_read_tokens,
                "cacheCreationTokens": cache_creation_tokens,
                "totalTokens": total_tokens,
                "contextTokens": context_tokens,
                "model": model,
                "turnSeq": turn_seq,
            }),
            ProviderEvent::System {
                text,
                subtype,
                detail,
            } => serde_json::json!({
                "text": text,
                "source": "provider-system",
                "subtype": subtype,
                "detail": detail,
            }),
            ProviderEvent::Completed {
                conversation_id,
                result_meta,
            } => {
                let mut data = serde_json::json!({
                    "status": "complete",
                    "conversationId": conversation_id,
                });
                if let (Some(target), Some(extra)) = (data.as_object_mut(), result_meta.as_object())
                {
                    for (k, v) in extra {
                        target.insert(k.clone(), v.clone());
                    }
                }
                data
            }
            ProviderEvent::Crashed {
                reason,
                error_kind,
                exit_code,
                stderr,
            } => serde_json::json!({
                "status": "crashed",
                "reason": reason,
                "errorKind": error_kind.as_str(),
                "exitCode": exit_code,
                "stderr": stderr,
            }),
            ProviderEvent::ControlRequest {
                request_id,
                request_type,
                payload,
            } => serde_json::json!({
                "requestId": request_id,
                "requestType": request_type,
                "payload": payload,
            }),
        }
    }
}

/// Configuration for spawning an agent run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpawnConfig {
    pub model: String,
    pub effort: Option<String>,
    pub working_dir: String,
    pub mcp_config_path: Option<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    pub permission_mode: Option<String>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Provider-agnostic instruction text appended to the agent's system
    /// prompt for this one spawn (Claude wires it into another
    /// `--append-system-prompt`; mock + recording providers ignore it).
    /// Used by repeating tasks to inform the agent it's a recurring run
    /// and to point at the per-task notes file convention.
    #[serde(default)]
    pub system_prompt_suffix: Option<String>,
    /// Per-session custom system prompt. When `Some(non-empty)`, it EXTENDS
    /// the standing Peckboard system prompt: it is appended after the base
    /// prompt, the shared working-style rules, and any `system_prompt_suffix`
    /// rather than replacing them. Populated from `Session::system_prompt` in
    /// the session manager so every dispatch path (chat, worker, repeating
    /// task) honours it from one place.
    #[serde(default)]
    pub system_prompt_override: Option<String>,
    /// Bare names of MCP tools contributed by active plugins (e.g.
    /// common-tools' `read_file` / `edit_file`), to pre-approve alongside the
    /// core tools. Populated once per dispatch in `SessionManager::final_config`
    /// from the plugin registry; the Claude provider namespaces them into
    /// `--allowedTools`. Other providers ignore it.
    #[serde(default)]
    pub extra_allowed_tools: Vec<String>,
    /// Fully-qualified `mcp__<server>__<tool>` names the user switched OFF
    /// on their external MCP servers (Settings → MCP Servers → per-tool
    /// toggles). Populated once per dispatch in `SessionManager::final_config`
    /// next to the config-file injection. The Claude provider appends them to
    /// `--disallowedTools`; the Ollama provider filters its native external
    /// tool set. Cursor/Grok can't enforce per-tool state through injected
    /// config files and ignore it.
    #[serde(default)]
    pub extra_disallowed_tools: Vec<String>,
    /// Whether the session being spawned is worker-flagged. Set from the
    /// session row in `SessionManager::final_config` — the single dispatch
    /// chokepoint — so values at other construction sites are placeholders.
    /// The Claude provider disables the CLI's built-in auto-compaction when
    /// this is false: only worker sessions may compact automatically;
    /// interactive sessions get the UI's clear / compact / continue prompt
    /// instead (see `crate::handover`).
    #[serde(default)]
    pub is_worker: bool,
    /// Whether the session being spawned is a pre-hatcher research session
    /// (`expert_kind == "pre-hatcher"`). Set from the session row in
    /// `SessionManager::final_config`, like `is_worker`. The Claude provider
    /// turns this into a hard `--disallowedTools` denylist of every built-in
    /// with side effects and narrows `--allowedTools` to the read-only
    /// pre-hatcher MCP set: the MCP server already refuses mutating peckboard
    /// tools from these sessions (`pre_hatcher_allowed_tool_names`), but the
    /// CLI's built-ins (Bash, Write, Task, …) bypass that server-side gate,
    /// and prompt-level read-only rules have been ignored in practice.
    #[serde(default)]
    pub is_pre_hatcher: bool,
}

/// Model info from a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Capability tier within this provider — higher is more capable.
    /// Used by the cost-aware auto-switch to find a "cheaper but capable"
    /// model (a lower tier, same provider + account). Providers with a
    /// single tier leave this at `0`; Claude ranks haiku < sonnet < opus
    /// < fable. Never compare tiers across providers.
    #[serde(default)]
    pub tier: i32,
}

impl ModelInfo {
    /// Whether this model supports extended reasoning ("thinking"). Planning
    /// is gated to thinking models to avoid hallucinated designs. Detection
    /// reads capability tags only: Claude/Grok tag reasoning models with a
    /// `reasoning` capability, Ollama and Kimi report `thinking`, and Cursor
    /// tags its catalog at build time from its own id convention (see
    /// `cursor::model_info`). The old fallback of sniffing "thinking" out of
    /// the model id is retired — each provider owns its tagging.
    pub fn is_thinking(&self) -> bool {
        self.capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("reasoning") || c.eq_ignore_ascii_case("thinking"))
    }

    /// Whether this model is known to accept image input, from capability
    /// tags. `Some(true)` when it advertises vision (`vision`, or Kimi's
    /// `image_in`); `Some(false)` when a capability probe succeeded and
    /// reported no vision — recognizable by Ollama's `/api/show` baseline
    /// tag `completion`, which every probed model carries; `None` when
    /// nothing is known, so callers fall back to the provider-level
    /// `ProviderCapabilities::supports_images_in`.
    pub fn images_in_hint(&self) -> Option<bool> {
        let has = |t: &str| self.capabilities.iter().any(|c| c.eq_ignore_ascii_case(t));
        if has("vision") || has("image_in") {
            Some(true)
        } else if has("completion") {
            Some(false)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(caps: &[&str], id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: id.into(),
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            tier: 0,
        }
    }

    #[test]
    fn is_thinking_reads_capability_tags_only() {
        assert!(model(&["code", "reasoning"], "opus").is_thinking());
        assert!(model(&["thinking"], "qwen3").is_thinking());
        // The id sniff is retired — an untagged "thinking" id is not enough.
        assert!(!model(&[], "claude-opus-4-8-thinking-high").is_thinking());
        assert!(!model(&["code"], "sonnet").is_thinking());
    }

    #[test]
    fn images_in_hint_reads_probe_vocabulary() {
        assert_eq!(model(&["code", "vision"], "m").images_in_hint(), Some(true));
        assert_eq!(model(&["image_in"], "m").images_in_hint(), Some(true));
        // Probe succeeded (Ollama baseline `completion`) without vision.
        assert_eq!(
            model(&["completion", "tools"], "m").images_in_hint(),
            Some(false)
        );
        // Nothing known — caller falls back to the provider-level flag.
        assert_eq!(model(&["code"], "m").images_in_hint(), None);
    }

    #[test]
    fn test_event_kind_mapping() {
        assert_eq!(
            ProviderEvent::Started {
                model: "opus".into(),
                conversation_id: None,
                metadata: serde_json::Value::Null,
            }
            .event_kind(),
            "agent-start"
        );
        assert_eq!(
            ProviderEvent::Text { text: "hi".into() }.event_kind(),
            "agent-text"
        );
        assert_eq!(
            ProviderEvent::Completed {
                conversation_id: None,
                result_meta: serde_json::Value::Null,
            }
            .event_kind(),
            "agent-end"
        );
        assert_eq!(
            ProviderEvent::Crashed {
                reason: "oops".into(),
                error_kind: CrashKind::Unknown,
                exit_code: Some(1),
                stderr: None,
            }
            .event_kind(),
            "agent-end"
        );
    }

    #[test]
    fn test_event_data_serialization() {
        let event = ProviderEvent::Text {
            text: "hello".into(),
        };
        let data = event.event_data();
        assert_eq!(data["text"], "hello");

        let event = ProviderEvent::Crashed {
            reason: "timeout".into(),
            error_kind: CrashKind::Timeout,
            exit_code: Some(137),
            stderr: Some("killed".into()),
        };
        let data = event.event_data();
        assert_eq!(data["status"], "crashed");
        assert_eq!(data["reason"], "timeout");
        assert_eq!(data["exitCode"], 137);
        assert_eq!(data["errorKind"], "timeout");
    }

    #[test]
    fn completed_merges_result_meta_additively() {
        let event = ProviderEvent::Completed {
            conversation_id: Some("conv-1".into()),
            result_meta: serde_json::json!({
                "durationMs": 1234,
                "numTurns": 3,
                "totalCostUsd": 0.42,
                "permissionDenials": [{ "tool_name": "Bash" }],
            }),
        };
        let data = event.event_data();
        assert_eq!(data["status"], "complete");
        assert_eq!(data["conversationId"], "conv-1");
        assert_eq!(data["durationMs"], 1234);
        assert_eq!(data["numTurns"], 3);
        assert_eq!(data["totalCostUsd"], 0.42);
        assert_eq!(data["permissionDenials"][0]["tool_name"], "Bash");
    }

    #[test]
    fn system_event_maps_to_system_kind() {
        let event = ProviderEvent::System {
            text: "Claude CLI compacted the conversation".into(),
            subtype: "compact_boundary".into(),
            detail: serde_json::json!({ "trigger": "auto" }),
        };
        assert_eq!(event.event_kind(), "system");
        let data = event.event_data();
        assert_eq!(data["text"], "Claude CLI compacted the conversation");
        assert_eq!(data["subtype"], "compact_boundary");
        assert_eq!(data["detail"]["trigger"], "auto");
    }

    /// One fixture per provider, taken from what that CLI/API actually
    /// prints, so the taxonomy keeps mapping the real strings.
    #[test]
    fn classify_maps_per_provider_fixtures() {
        let cases: &[(&str, CrashKind)] = &[
            // Claude CLI `result` error on an expired login.
            (
                "Failed to authenticate. API Error: 401 {\"type\":\"error\"}",
                CrashKind::AuthExpired,
            ),
            // Claude CLI on a throttled account.
            (
                "API Error: 429 {\"error\":{\"type\":\"rate_limit_error\"}}",
                CrashKind::RateLimit,
            ),
            // grok: the device-login marker's rewritten message.
            (
                "This Grok account isn't signed in. Open Settings \u{2192} Grok accounts",
                CrashKind::AuthExpired,
            ),
            // kimi: the "No model configured" marker's rewritten message.
            (
                "Kimi Code isn't signed in on this host. Run `kimi login`",
                CrashKind::AuthExpired,
            ),
            // cursor-agent: an unauthenticated non-zero exit's stderr.
            ("Error: Unauthorized (401)", CrashKind::AuthExpired),
            // ollama: the HTTP-status crash reason.
            (
                "Ollama returned HTTP 429 Too Many Requests",
                CrashKind::RateLimit,
            ),
            (
                "Ollama returned HTTP 401 Unauthorized",
                CrashKind::AuthExpired,
            ),
            // The shared turn harness's timeout copy.
            ("grok turn exceeded 600s timeout", CrashKind::Timeout),
            // Every cancel path, whatever emitted it.
            ("interrupted", CrashKind::Interrupted),
            // A plugin provider that reports something opaque.
            ("upstream returned an unexpected body", CrashKind::Unknown),
        ];
        for (text, expected) in cases {
            assert_eq!(CrashKind::classify(text), *expected, "classifying {text:?}");
        }
    }

    #[test]
    fn classify_or_falls_back_only_when_the_text_says_nothing() {
        assert_eq!(
            CrashKind::classify_or("boom", CrashKind::ExitedMidTurn),
            CrashKind::ExitedMidTurn
        );
        assert_eq!(
            CrashKind::classify_or("API Error: 401", CrashKind::ExitedMidTurn),
            CrashKind::AuthExpired
        );
        assert_eq!(
            CrashKind::AuthExpired.or(CrashKind::NoOutput),
            CrashKind::AuthExpired
        );
        assert_eq!(
            CrashKind::Unknown.or(CrashKind::NoOutput),
            CrashKind::NoOutput
        );
    }

    /// A `Crashed` payload written before the taxonomy existed — or by a
    /// plugin provider that never sets it — must still deserialize.
    #[test]
    fn crashed_error_kind_defaults_to_unknown() {
        let event: ProviderEvent = serde_json::from_value(serde_json::json!({
            "kind": "crashed",
            "reason": "plugin exploded",
            "exit_code": null,
            "stderr": null,
        }))
        .expect("legacy Crashed payload deserializes");
        match event {
            ProviderEvent::Crashed { error_kind, .. } => {
                assert_eq!(error_kind, CrashKind::Unknown)
            }
            other => panic!("expected Crashed, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(CrashKind::ExitedMidTurn).unwrap(),
            serde_json::json!("exited_mid_turn")
        );
    }
}
