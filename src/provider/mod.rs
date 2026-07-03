pub mod agent;
pub mod claude;
pub mod cursor;
pub mod grok;
pub mod manager;
pub mod message;
pub mod mock;
pub mod ollama;
pub mod registry;
pub mod stream;

/// Shared "working style" rules appended to (or, for full-replace providers,
/// used as) the system prompt of every agent provider's sessions. Single
/// source of truth so Claude, grok, ollama, and cursor all ship the same
/// guidance. The leading newline lets it be appended directly onto a prompt.
pub const WORKING_STYLE: &str = "\n# Working style\n\n- Prefer the code tools \u{2014} `file_outline`, `read_symbol`, `search_files`, `read_file`, `edit_file` \u{2014} and the search tool to navigate and edit code. NEVER use `grep` or `sed` \u{2014} not in shell commands, not in scripts, not via subagents; use `search_files` (ripgrep-backed) and the code tools instead.\n- NEVER use the terminal or shell tools (Bash and similar). To run a command, use the `run_command` tool instead \u{2014} it is approval-gated so the user stays in control. Use `run_tests` for test suites and `git` for git operations.\n- Keep answers short and to the point. Minimize output \u{2014} don't over-explain or add detail the user didn't ask for.\n- Be critical of the user's direction. When a choice looks suboptimal or wrong, say so and advise or push back with a better option before acting \u{2014} don't just comply.\n";

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
pub fn auto_model(effort: Option<&str>, is_worker: bool) -> &'static str {
    match effort {
        Some("low") => "claude-haiku-4-5",
        Some("medium") => "claude-sonnet-4-6",
        Some("high") => "claude-opus-4-8",
        Some("xhigh") | Some("max") => "claude-fable-5",
        _ => {
            if is_worker {
                "claude-sonnet-4-6"
            } else {
                "claude-opus-4-8"
            }
        }
    }
}

#[cfg(test)]
mod auto_tests {
    use super::*;

    #[test]
    fn auto_model_routes_by_effort_then_role() {
        assert!(is_auto_model("") && is_auto_model("default") && is_auto_model("Auto"));
        assert!(!is_auto_model("claude-opus-4-8"));
        assert_eq!(auto_model(Some("low"), true), "claude-haiku-4-5");
        assert_eq!(auto_model(Some("medium"), false), "claude-sonnet-4-6");
        assert_eq!(auto_model(Some("high"), true), "claude-opus-4-8");
        assert_eq!(auto_model(Some("xhigh"), false), "claude-fable-5");
        assert_eq!(auto_model(Some("max"), true), "claude-fable-5");
        assert_eq!(auto_model(None, true), "claude-sonnet-4-6");
        assert_eq!(auto_model(None, false), "claude-opus-4-8");
        // Junk effort falls back by role rather than panicking.
        assert_eq!(auto_model(Some("very high"), false), "claude-opus-4-8");
    }
}

// Provider factory — AI provider abstraction
//
// Providers implement the full agent lifecycle: spawn, send,
// interrupt, kill, cleanup. Each provider translates its native
// output into the unified ProviderEvent stream. Claude CLI is
// the built-in provider; plugins can register additional providers.
