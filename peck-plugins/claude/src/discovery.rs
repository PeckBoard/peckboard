//! CLI model discovery via the stream-json `initialize` handshake.
//!
//! Port of 0.1.11's `src/provider/claude/mod.rs::probe_cli_models`: the CLI
//! has no `models` subcommand; its catalog rides the control protocol's
//! `initialize` handshake. The probe spawns `claude --print` in duplex
//! stream-json mode (tools and MCP disabled), writes one
//! `control_request{subtype:"initialize"}` line, and reads the matching
//! `control_response`, whose `models` array carries
//! value/displayName/description per model. The host's probe function is
//! one-shot (stdin written then closed) with a 60 s TTL cache, so a broken
//! or slow CLI costs at most one spawn per window.

use serde_json::{Value, json};

use crate::settings;

const REQUEST_ID: &str = "peckboard-model-discovery";

/// Hard cap on one discovery probe: CLI spawn + initialize handshake.
const MODEL_DISCOVERY_TIMEOUT_MS: u64 = 10_000;

/// Ask the CLI for its catalog under the host credentials. `None` on any
/// failure (no CLI, no answer, empty catalog) so the caller falls back to
/// the static seed.
///
/// Scope note: 0.1.11 additionally probed once per stored account, under
/// that account's credential env (`CLAUDE_CONFIG_DIR` / tokens). The
/// plugin's `provider.models` hook cannot obtain account credentials — the
/// `account_env` host fn is turn-scoped and `list_accounts` returns only
/// `{id, name}` — so only the host scope is probed and account variants
/// mirror the host catalog (0.1.11's documented fallback when an account
/// probe failed).
pub fn probe_cli_models(cli: &str) -> Option<Vec<Value>> {
    let request = json!({
        "type": "control_request",
        "request_id": REQUEST_ID,
        "request": { "subtype": "initialize" },
    });
    let stdout = settings::probe_stdout(
        cli,
        &[
            "--print",
            "--input-format=stream-json",
            "--output-format=stream-json",
            "--verbose",
            // Keep startup lean: no MCP servers, no built-in tools.
            "--strict-mcp-config",
            "--tools",
            "",
        ],
        Some(&format!("{request}\n")),
        MODEL_DISCOVERY_TIMEOUT_MS,
    )?;
    let models = stdout
        .lines()
        .find_map(|line| parse_initialize_models(line, REQUEST_ID))?;
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

/// Parse one stream-json stdout line: if it is the successful
/// `control_response` for `request_id`, map its `models` array into seed-
/// shaped catalog entries. `None` for every other line (system frames,
/// other responses) and for malformed or error responses.
pub fn parse_initialize_models(line: &str, request_id: &str) -> Option<Vec<Value>> {
    let json: Value = serde_json::from_str(line.trim()).ok()?;
    if json.get("type")?.as_str()? != "control_response" {
        return None;
    }
    let response = json.get("response")?;
    if response.get("request_id")?.as_str()? != request_id
        || response.get("subtype")?.as_str()? != "success"
    {
        return None;
    }
    let models = response.get("response")?.get("models")?.as_array()?;
    Some(models.iter().filter_map(cli_model_info).collect())
}

/// Map one CLI catalog entry to a seed-shaped model object.
///
/// The CLI's `value` becomes the model id verbatim — it is exactly what
/// `--model=` accepts (aliases like `opus[1m]` included), so it round-trips
/// through spawn untouched. The `default` sentinel is skipped: Peckboard's
/// own Auto entry already covers "let the CLI choose". The display name
/// prefers the description head ("Opus 4.8 with 1M context · Best for…" →
/// "Opus 4.8 with 1M context") because it names the concrete model version,
/// which the bare alias label ("Opus") does not.
fn cli_model_info(entry: &Value) -> Option<Value> {
    let value = entry.get("value")?.as_str()?.trim();
    if value.is_empty() || value == "default" {
        return None;
    }
    let label = entry
        .get("displayName")
        .and_then(|v| v.as_str())
        .unwrap_or(value);
    let description = entry
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let display_name = description
        .split('·')
        .next()
        .map(str::trim)
        .filter(|head| !head.is_empty())
        .unwrap_or(label)
        .to_string();

    let haystack = format!("{value} {label} {description}").to_lowercase();
    let tier = if haystack.contains("fable") || haystack.contains("mythos") {
        4
    } else if haystack.contains("opus") {
        3
    } else if haystack.contains("sonnet") {
        2
    } else if haystack.contains("haiku") {
        1
    } else {
        2
    };

    let flag = |key: &str| entry.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    let mut capabilities = vec!["code".to_string()];
    if flag("supportsAdaptiveThinking") || flag("supportsEffort") {
        capabilities.push("reasoning".into());
    }
    if tier >= 2 {
        capabilities.push("vision".into());
    }

    Some(json!({
        "id": value,
        "display_name": display_name,
        "capabilities": capabilities,
        "tier": tier,
    }))
}

/// Append every pinned-seed entry the probed catalog doesn't already carry.
/// Matched by id only: a CLI family alias (`opus[1m]`) is not the same as a
/// pinned snapshot (`claude-opus-4-8`), and both stay selectable — that is
/// how the pinned Opus snapshots remain pickable after the CLI starts
/// advertising a newer `opus` alias.
pub fn merge_always_offered(mut models: Vec<Value>) -> Vec<Value> {
    let seed = crate::models::seed_models();
    for extra in seed.as_array().cloned().unwrap_or_default() {
        let id = extra.get("id").and_then(|v| v.as_str());
        if !models
            .iter()
            .any(|m| m.get("id").and_then(|v| v.as_str()) == id)
        {
            models.push(extra);
        }
    }
    models
}

/// Bedrock ARNs advertised via the standard `ANTHROPIC_DEFAULT_*_MODEL` env
/// vars become picker entries ("Bedrock: <arn tail>"). Native builds only —
/// WASM has no environment.
pub fn push_bedrock_env_models(models: &mut Vec<Value>) {
    #[cfg(not(target_arch = "wasm32"))]
    for (env_var, tier) in &[
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", 3),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", 2),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", 1),
    ] {
        if let Ok(arn) = std::env::var(env_var) {
            if arn.is_empty()
                || models
                    .iter()
                    .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(arn.as_str()))
            {
                continue;
            }
            models.push(json!({
                "id": arn,
                "display_name": format!("Bedrock: {}", arn.split('/').next_back().unwrap_or(&arn)),
                "capabilities": ["code"],
                "tier": tier,
            }));
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = models;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape captured from a real `claude` initialize handshake (2.1.x).
    const HANDSHAKE_LINE: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"peckboard-model-discovery","response":{"commands":[],"models":[{"value":"default","displayName":"Default","description":"Let the CLI choose"},{"value":"opus[1m]","displayName":"Opus","description":"Opus 4.8 with 1M context · Best for complex work","supportsEffort":true},{"value":"sonnet","displayName":"Sonnet","description":"Sonnet 4.6 · Balanced"},{"value":"haiku","displayName":"Haiku","description":""}]}}}"#;

    #[test]
    fn parses_models_from_a_captured_handshake() {
        let models = parse_initialize_models(HANDSHAKE_LINE, "peckboard-model-discovery").unwrap();
        let ids: Vec<&str> = models
            .iter()
            .map(|m| m.get("id").unwrap().as_str().unwrap())
            .collect();
        // `default` is skipped; aliases pass through verbatim.
        assert_eq!(ids, vec!["opus[1m]", "sonnet", "haiku"]);
        // Display name prefers the description head over the alias label.
        assert_eq!(
            models[0].get("display_name").unwrap().as_str().unwrap(),
            "Opus 4.8 with 1M context"
        );
        // Tier + capabilities derived from the text / flags.
        assert_eq!(models[0].get("tier").unwrap().as_i64(), Some(3));
        assert!(
            models[0]
                .get("capabilities")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "reasoning")
        );
        assert_eq!(models[2].get("tier").unwrap().as_i64(), Some(1));
        // Haiku's empty description falls back to the displayName label.
        assert_eq!(
            models[2].get("display_name").unwrap().as_str().unwrap(),
            "Haiku"
        );
    }

    #[test]
    fn other_lines_and_error_responses_are_ignored() {
        assert!(parse_initialize_models("not json", "x").is_none());
        assert!(
            parse_initialize_models(
                r#"{"type":"system","subtype":"init"}"#,
                "peckboard-model-discovery"
            )
            .is_none()
        );
        assert!(
            parse_initialize_models(
                r#"{"type":"control_response","response":{"subtype":"error","request_id":"peckboard-model-discovery","error":"nope"}}"#,
                "peckboard-model-discovery"
            )
            .is_none()
        );
    }

    #[test]
    fn merge_always_offered_tops_up_pinned_ids() {
        let discovered = vec![serde_json::json!({
            "id": "opus[1m]",
            "display_name": "Opus 4.8 with 1M context",
            "capabilities": ["code"],
            "tier": 3,
        })];
        let merged = merge_always_offered(discovered);
        let ids: Vec<&str> = merged
            .iter()
            .map(|m| m.get("id").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(ids[0], "opus[1m]");
        assert!(ids.contains(&"claude-opus-4-8"));
        assert!(ids.contains(&"claude-haiku-4-5"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn bedrock_env_models_become_picker_entries() {
        // Process-global env: use a value no other test touches.
        unsafe {
            std::env::set_var(
                "ANTHROPIC_DEFAULT_OPUS_MODEL",
                "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-opus-5",
            );
        }
        let mut models = Vec::new();
        push_bedrock_env_models(&mut models);
        unsafe {
            std::env::remove_var("ANTHROPIC_DEFAULT_OPUS_MODEL");
        }
        let entry = models
            .iter()
            .find(|m| {
                m.get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| id.ends_with("anthropic.claude-opus-5"))
            })
            .expect("bedrock entry present");
        assert_eq!(
            entry.get("display_name").unwrap().as_str().unwrap(),
            "Bedrock: anthropic.claude-opus-5"
        );
        assert_eq!(entry.get("tier").unwrap().as_i64(), Some(3));
    }
}
