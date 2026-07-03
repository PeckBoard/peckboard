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

// Provider factory — AI provider abstraction
//
// Providers implement the full agent lifecycle: spawn, send,
// interrupt, kill, cleanup. Each provider translates its native
// output into the unified ProviderEvent stream. Claude CLI is
// the built-in provider; plugins can register additional providers.
