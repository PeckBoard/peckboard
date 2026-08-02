pub mod agent;
pub mod claude;
pub mod cursor;
pub mod grok;
pub mod kimi;
pub mod manager;
pub mod message;
pub mod mock;
pub mod ollama;
pub mod plugin_provider;
pub mod registry;
pub mod stream;
pub mod turn;

/// Shared "working style" rules appended to (or, for full-replace providers,
/// used as) the system prompt of every agent provider's sessions. Single
/// source of truth so Claude, grok, ollama, and cursor all ship the same
/// guidance. The leading newline lets it be appended directly onto a prompt.
pub const WORKING_STYLE: &str = "\n# Working style\n\n- For non-trivial work \u{2014} more than a small, well-scoped edit \u{2014} propose a plan first with the `propose_plan` tool and get the user's sign-off before implementing, rather than diving straight into code. Skip this only for trivial or explicitly-specified changes.\n- Prefer the code tools \u{2014} `file_outline`, `read_symbol`, `search_files`, `read_file`, `edit_file` \u{2014} and the search tool to navigate and edit code. NEVER use `grep` or `sed` \u{2014} not in shell commands, not in scripts, not via subagents; use `search_files` (ripgrep-backed) and the code tools instead.\n- NEVER use the terminal or shell tools (Bash and similar). To run a command, use the `run_command` tool instead \u{2014} chat commands are approval-gated so the user stays in control; worker sessions run commands directly, always scoped to the project folder. Use `run_tests` for test suites and `git` for git operations. This applies to subagents too: a subagent must NEVER use the terminal or shell \u{2014} state that explicitly in its prompt.\n- For broad context gathering \u{2014} locating files, mapping unfamiliar code, surveying a repo \u{2014} delegate to a subagent on a cheaper tier or low effort and have it return a distilled summary. Keep the expensive main model for reasoning, decisions, and edits; don't burn its context on exploratory reads a cheap model can do.\n- A subagent does NOT inherit this system prompt. On Claude, Peckboard injects the standing provider rules \u{2014} never use the terminal/shell, use the code tools not `grep`/`sed`, stay inside the project folder \u{2014} into every subagent automatically (SubagentStart hook); on other providers you must restate them in the subagent's prompt yourself. Either way, give every subagent a system prompt matched to its task: look at the work you are delegating, call `list_system_prompts`, and fold the most fitting one (e.g. `research` to investigate, `review` for code review, `debug` to hunt a defect) into the subagent's instructions. You decide, per subagent, which prompt fits the work.\n- Split large tasks: spawn multiple subagents and divide the work between them \u{2014} independent parts run in parallel \u{2014} instead of grinding through everything in one loop. The `spawn_subagent` tool works on every provider: the child session runs in the background and its final message is posted back to you automatically when it finishes.\n- Never verify UI or UX with unit tests alone. Use the Playwright MCP browser tools (or the built-in `browser_*` tools) to open the app, navigate to the affected page, and confirm the change renders and behaves correctly before calling it done.\n- Model routing: UI/UX work always gets the best available model \u{2014} never a lower tier. Complex backend work gets the second-highest. The third-highest is only for incredibly simple tasks, and never for UI. Never assign the lowest model.\n- Keep answers short and to the point. Minimize output \u{2014} don't over-explain or add detail the user didn't ask for.\n- Be critical of the user's direction. When a choice looks suboptimal or wrong, say so and advise or push back with a better option before acting \u{2014} don't just comply.\n- For UI or frontend changes, when a field corresponds to a database-backed type, enum, object, list, priority list, version, or any other non-freeform option: never render it as a plain text input. Use a searchable dropdown (combobox) if the option set can be large; use a plain dropdown (`<select>`) if the set is small and predefined (e.g. a known enum). Only use a text input for genuinely freeform user-authored content.\n";

/// Error returned to the model when it attempts the terminal/shell tool
/// (Claude's `Bash`, and its `BashOutput` / `KillShell` companions). The
/// terminal is disabled in Peckboard: all command execution must go through
/// the approval-gated `run_command` MCP tool. Shared so every provider that
/// intercepts a runnable terminal tool denies with identical copy.
pub const TERMINAL_TOOL_DISABLED_MSG: &str = "The terminal/Bash tool is disabled in Peckboard. Use the `run_command` tool instead to run shell commands.";

/// The terminal/shell tool and its companions, by the names the Claude CLI
/// uses (`Bash` runs a command; `BashOutput` / `KillShell` manage background
/// shells). Single source of truth so every provider denies the same set.
pub fn is_terminal_tool(name: &str) -> bool {
    matches!(name, "Bash" | "BashOutput" | "KillShell")
}

/// Caveman output-style blocks for interactive sessions, keyed by the
/// `caveman_mode` app setting (`off` | `lite` | `full`). Adapted from the
/// caveman skill (github.com/JuliusBrussee/caveman): compress the STYLE,
/// never the substance — code, identifiers, and error strings stay exact.
/// Applied as a system-prompt suffix at the dispatch chokepoint for
/// non-worker sessions; workers carry their own copy in the worker prompt.
pub fn caveman_style(level: &str) -> Option<&'static str> {
    match level {
        "lite" => Some(CAVEMAN_LITE),
        "full" => Some(CAVEMAN_FULL),
        _ => None,
    }
}

const CAVEMAN_LITE: &str = "\n# Output style \u{2014} terse\n\nNo filler, no pleasantries, no hedging, no tool-call narration. Keep articles and full sentences \u{2014} professional but tight. Code, commands, identifiers, and error strings stay exact. Plain full clarity for security warnings, destructive or irreversible actions, and order-sensitive multi-step sequences.\n";

const CAVEMAN_FULL: &str = "\n# Output style \u{2014} caveman\n\nSpeak terse like smart caveman. All technical substance stay; only fluff die. Active EVERY response \u{2014} no drift back to verbose.\n- Drop articles, filler (just/really/basically), pleasantries, hedging. Fragments OK. Short synonyms.\n- No tool-call narration, no decorative tables or emoji, no raw log dumps \u{2014} quote shortest decisive line.\n- Code, commands, identifiers, file paths, error strings: EXACT, never abbreviated. Standard acronyms OK (DB/API/HTTP); invent none.\n- Keep the user's language \u{2014} compress style, not language.\n- Plain, full-sentence clarity returns for: security warnings, destructive or irreversible actions, and ordered multi-step sequences where fragments risk misread. Then caveman resume.\n";
/// Model ids that mean "let PeckBoard choose": empty, the legacy
/// "default", or the explicit "auto".
pub fn is_auto_model(id: &str) -> bool {
    let id = id.trim();
    id.is_empty() || id.eq_ignore_ascii_case("default") || id.eq_ignore_ascii_case("auto")
}

/// Auto mode: pick the best Claude model for the task from its resolved
/// effort. Effort is the app's own "how hard is this" signal, so routing on
/// it is deterministic and costs zero tokens (no classifier call). With no
/// effort at all, workers get Sonnet (their effort defaults to medium) and
/// chats get Opus (interactive quality expectations).
///
/// Returns CLI aliases (`haiku`/`sonnet`/`opus`/`fable`), not pinned model
/// ids: the CLI resolves an alias to its newest model of that tier, so
/// auto-routed sessions track CLI updates without a PeckBoard release.
pub fn auto_model(effort: Option<&str>, is_worker: bool) -> &'static str {
    match effort {
        Some("low") => "haiku",
        Some("medium") => "sonnet",
        Some("high") => "opus",
        Some("xhigh") | Some("max") => "fable",
        _ => {
            if is_worker {
                "sonnet"
            } else {
                "opus"
            }
        }
    }
}

/// One provider's usability info for auto-model resolution: id, best-effort
/// auth status (see `AgentProvider::auth_configured`), and its model
/// catalog with tier metadata.
pub struct AutoCandidate {
    pub provider_id: String,
    pub auth: Option<bool>,
    pub models: Vec<stream::ModelInfo>,
}

/// Maps effort (plus worker/chat role, mirroring `auto_model`'s own
/// fallback) onto a provider-agnostic "how capable a tier do we want"
/// rank. Not comparable across providers by absolute value — only used to
/// pick the highest tier within one provider's own tier range that is
/// <= this rank, clamping down when the provider doesn't go that high.
fn auto_effort_rank(effort: Option<&str>, is_worker: bool) -> i32 {
    match effort {
        Some("low") => 0,
        Some("medium") => 1,
        Some("high") => 2,
        Some("xhigh") | Some("max") => 3,
        _ => {
            if is_worker {
                1
            } else {
                2
            }
        }
    }
}

/// Provider-aware auto-model resolution. Prefers Claude (unchanged
/// `auto_model` alias behaviour) when it's usable; otherwise picks the
/// highest-tier model from the best usable non-Claude provider, ranking
/// credentialed providers (`auth == Some(true)`) ahead of providers with no
/// auth signal (local providers like ollama, `auth == None`) ahead of
/// providers known to lack auth (`auth == Some(false)`, excluded entirely).
/// Ties between equally-ranked providers break on provider id for
/// determinism. Returns an error naming Settings → Providers & Accounts
/// when nothing usable exists, rather than defaulting silently.
pub fn resolve_auto_model(
    candidates: &[AutoCandidate],
    effort: Option<&str>,
    is_worker: bool,
) -> anyhow::Result<String> {
    let usable = |c: &AutoCandidate| c.auth != Some(false) && !c.models.is_empty();

    if candidates
        .iter()
        .any(|c| c.provider_id == "claude" && usable(c))
    {
        return Ok(auto_model(effort, is_worker).to_string());
    }

    let mut ranked: Vec<&AutoCandidate> = candidates.iter().filter(|c| usable(c)).collect();
    ranked.sort_by(|a, b| {
        let a_credentialed = a.auth == Some(true);
        let b_credentialed = b.auth == Some(true);
        b_credentialed
            .cmp(&a_credentialed)
            .then_with(|| a.provider_id.cmp(&b.provider_id))
    });

    let chosen = ranked.into_iter().next().ok_or_else(|| {
        anyhow::anyhow!(
            "Auto mode has no usable AI provider: add credentials or a local provider in \
             Settings \u{2192} Providers & Accounts."
        )
    })?;

    let target_rank = auto_effort_rank(effort, is_worker);
    let mut tiers: Vec<i32> = chosen.models.iter().map(|m| m.tier).collect();
    tiers.sort_unstable();
    tiers.dedup();
    let tier = tiers
        .iter()
        .rev()
        .find(|&&t| t <= target_rank)
        .copied()
        .or_else(|| tiers.first().copied())
        .unwrap_or(0);

    let model = chosen
        .models
        .iter()
        .find(|m| m.tier == tier)
        .expect("tier was selected from chosen.models");

    Ok(format!("{}:{}", chosen.provider_id, model.id))
}
#[cfg(test)]
mod auto_tests {
    use super::*;

    #[test]
    fn auto_model_routes_by_effort_then_role() {
        assert!(is_auto_model("") && is_auto_model("default") && is_auto_model("Auto"));
        assert!(!is_auto_model("claude-opus-4-8"));
        assert!(!is_auto_model("opus"));
        assert_eq!(auto_model(Some("low"), true), "haiku");
        assert_eq!(auto_model(Some("medium"), false), "sonnet");
        assert_eq!(auto_model(Some("high"), true), "opus");
        assert_eq!(auto_model(Some("xhigh"), false), "fable");
        assert_eq!(auto_model(Some("max"), true), "fable");
        assert_eq!(auto_model(None, true), "sonnet");
        assert_eq!(auto_model(None, false), "opus");
        // Junk effort falls back by role rather than panicking.
        assert_eq!(auto_model(Some("very high"), false), "opus");
    }

    fn model(id: &str, tier: i32) -> stream::ModelInfo {
        stream::ModelInfo {
            id: id.into(),
            display_name: id.into(),
            capabilities: vec![],
            tier,
        }
    }

    fn claude_models() -> Vec<stream::ModelInfo> {
        vec![
            model("claude-haiku-4-5", 1),
            model("claude-sonnet-4-6", 2),
            model("claude-opus-4-8", 3),
            model("claude-fable-5", 4),
        ]
    }

    #[test]
    fn resolve_auto_model_prefers_claude_when_usable() {
        let candidates = vec![
            AutoCandidate {
                provider_id: "claude".into(),
                auth: Some(true),
                models: claude_models(),
            },
            AutoCandidate {
                provider_id: "ollama".into(),
                auth: None,
                models: vec![model("llama3", 0)],
            },
        ];
        // Identical to plain `auto_model` — no regression when Claude works.
        assert_eq!(
            resolve_auto_model(&candidates, Some("high"), false).unwrap(),
            auto_model(Some("high"), false)
        );
        assert_eq!(
            resolve_auto_model(&candidates, None, true).unwrap(),
            auto_model(None, true)
        );
    }

    #[test]
    fn resolve_auto_model_falls_back_to_ollama_when_claude_unusable() {
        let candidates = vec![
            AutoCandidate {
                provider_id: "claude".into(),
                auth: Some(false),
                models: claude_models(),
            },
            AutoCandidate {
                provider_id: "ollama".into(),
                auth: None,
                models: vec![model("llama3", 0)],
            },
        ];
        assert_eq!(
            resolve_auto_model(&candidates, Some("high"), false).unwrap(),
            "ollama:llama3"
        );
    }

    #[test]
    fn resolve_auto_model_prefers_credentialed_provider_over_local() {
        let candidates = vec![
            AutoCandidate {
                provider_id: "ollama".into(),
                auth: None,
                models: vec![model("llama3", 0)],
            },
            AutoCandidate {
                provider_id: "grok".into(),
                auth: Some(true),
                models: vec![model("grok-4", 0)],
            },
        ];
        assert_eq!(
            resolve_auto_model(&candidates, None, false).unwrap(),
            "grok:grok-4"
        );
    }

    #[test]
    fn resolve_auto_model_picks_tier_by_effort_within_chosen_provider() {
        let candidates = vec![AutoCandidate {
            provider_id: "grok".into(),
            auth: Some(true),
            models: vec![
                model("grok-mini", 0),
                model("grok-4", 1),
                model("grok-max", 2),
            ],
        }];
        assert_eq!(
            resolve_auto_model(&candidates, Some("low"), false).unwrap(),
            "grok:grok-mini"
        );
        assert_eq!(
            resolve_auto_model(&candidates, Some("high"), false).unwrap(),
            "grok:grok-max"
        );
        // No effort, chat role ranks 2 — clamps to the top tier available.
        assert_eq!(
            resolve_auto_model(&candidates, None, false).unwrap(),
            "grok:grok-max"
        );
    }

    #[test]
    fn resolve_auto_model_errors_when_nothing_usable() {
        let candidates = vec![
            AutoCandidate {
                provider_id: "claude".into(),
                auth: Some(false),
                models: claude_models(),
            },
            AutoCandidate {
                provider_id: "ollama".into(),
                auth: Some(false),
                models: vec![],
            },
        ];
        let err = resolve_auto_model(&candidates, None, false).unwrap_err();
        assert!(err.to_string().contains("Settings"));
        assert!(err.to_string().contains("Providers & Accounts"));
    }
}

// Provider factory — AI provider abstraction
//
// Providers implement the full agent lifecycle: spawn, send,
// interrupt, kill, cleanup. Each provider translates its native
// output into the unified ProviderEvent stream. Claude CLI is
// the built-in provider; plugins can register additional providers.
