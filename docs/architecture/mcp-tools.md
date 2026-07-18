# MCP Tools

Each worker and plain session gets a stdio MCP subprocess spawned alongside the Claude CLI. The subprocess runs the MCP server with a scoped bearer token.

## Worker Tools

Workers (autonomous card executors) have access to:

| Tool | Description |
|------|-------------|
| create_card | File follow-up cards on the project. Inherits caller's workflow by default |
| list_projects | List all projects |
| list_workflows | List available workflows |
| list_cards | List cards with filters (step, workflow, blocked) |
| finish_card | Skip remaining pipeline steps, mark card done |
| wont_do_card | Park card in won't-do column for human triage |
| complete_step | Signal current step is complete, advance to next |
| ask_user | Block card with a question for the human operator |
| write_report | Write a markdown report to the reports folder |
| attach_report_file | Attach a binary/text file to a report folder |
| update_card | Update card fields (subject to edit policy) |
| update_project | Update project fields |
| create_project | Create a new project with initial cards |
| pause_project | Pause a project |
| resume_project | Resume a project |
| delete_card | Delete a card |
| move_card_to_done | Move card to done |
| move_card_to_wont_do | Move card to won't-do |

Workers do NOT get AskUserQuestion (auto-denied by the CLI). They run autonomously.

## Plain Session Tools

Plain sessions (interactive, human at keyboard) have access to:

| Tool | Description |
|------|-------------|
| create_project | Create a new project with initial cards |
| list_projects | List all projects |
| list_workflows | List available workflows |
| create_card | Create cards on any project |
| list_cards | List cards with filters |
| write_report | Write markdown reports |
| attach_report_file | Attach files to reports |
| update_card | Update card fields |
| update_project | Update project fields |
| pause_project | Pause a project |
| resume_project | Resume a project |
| delete_card | Delete a card |
| move_card_to_done | Move card to done |
| move_card_to_wont_do | Move card to won't-do |

Plus AskUserQuestion via `--permission-prompt-tool stdio`.

## Token Scoping

- **Worker tokens:** scoped to `{ sessionId, projectId, role: 'worker' }`. Can only operate on their own project (403 on cross-project calls).
- **Session tokens:** scoped to `{ sessionId, role: 'session' }`. Can target any project by passing `projectId` explicitly.

Tokens are 24-byte hex, stored by SHA-256 hash. Issued on spawn, revoked on teardown.

## MCP Config File

Per-session JSON at `<dataDir>/worker-mcp/<sessionId>.json`. Consumed by the Claude CLI's `--mcp-config` flag. References the MCP server subprocess command and passes the token + base URL via environment variables.

## Report Writing

Reports are written to `<dataDir>/reports/<YYYY-MM-DD folder>/<file>.md`:
- Folder and file names are caller-supplied but sanitized (no path separators, dots, or traversal)
- Today's date is prepended to the folder automatically
- Frontmatter records title, ISO date, sessionId, projectName
- The tool returns sanitized `folder` + `file` names (no absolute paths leaked)

Attachments go through `attach_report_file` as base64-encoded bytes (never file paths, to avoid local-file-disclosure). Extensions are allowlisted, capped at 10 MB raw.

## Side Effects

When a worker calls a tool that creates/references a project or card, the MCP server appends a "chip" system message to the caller's session transcript. This renders as a clickable reference in the chat UI.

## Subagents (`spawn_subagent`)

Any session on any provider can spawn a subagent — a child session that
works a task in the background and has its final message posted back into
the parent automatically. This is the provider-independent counterpart to
Claude's native Task tool (grok / ollama / cursor have no such mechanism).

Flow (`src/subagent.rs`):

1. `spawn_subagent {name, prompt, model?, effort?, system_prompt_name?}`
   creates the child row — `expert_kind = 'subagent'`,
   `parent_session_id` = caller, caller's folder/project/user, model and
   effort inherited unless given — persists the preamble+task as the
   child's first `user` event, and returns `{subagent_session_id}` plus a
   `_dispatch_session` marker.
2. The `mcp` route (which holds the `AppState`) strips the marker and
   fire-and-forgets the child's first turn through
   `ExpertDispatcher::resume_session`.
3. Every provider emits a `ProcessCompletion`; the completion listener in
   `main.rs` calls `subagent::handle_subagent_done`, which claims the
   completion (idempotent via `sessions.subagent_completed_at`), extracts
   the child's final reply (trailing `agent-text` events), and delivers
   `[subagent "<name>" finished] …` to the parent like a user message
   (spawn if idle, queue/inject if running). Crashes deliver the error.

Guards: depth 1 (a subagent may not spawn subagents), max 5 in flight per
parent, blocked for pre-hatcher sessions by the read-only allowlist.
Subagent sessions are ordinary sessions — readable with
`read_worker_session`/`search_sessions`, terminable via session-control.
