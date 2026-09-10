# Codex CLI — Spawn Argv and JSONL

Captured 2026-09-09. **Live `codex` is not on this machine** (`which codex` miss; `~/.local/bin` has `claude`/`grok`/`cursor-agent`/`agent`, no `codex`; no `~/.codex`). Did **not** install. Flags, JSONL, models, auth strings below come from `openai/codex` `main` (Rust CLI + TypeScript SDK) plus https://learn.chatgpt.com/docs/codex/cli .

Official install (Linux/macOS):

```bash
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Re-probe after install: `codex exec --help`, `codex --help`, `codex debug models --bundled`, then a live `--json` turn in a **tmp git repo** (never `~/.peckboard`, never this working tree).

Synthetic fixture: `fixtures/hello.jsonl` (official event shapes; not a live capture).

## Default Binary Path

- Spawn name: `codex` (PATH).
- Standalone installer symlink: `$HOME/.local/bin/codex` (`CODEX_INSTALL_DIR` override).
- Real binary: `$CODEX_HOME/packages/standalone/current/codex` or `.../current/bin/codex` (`CODEX_HOME` default `$HOME/.codex`).
- npm global: `$prefix/bin/codex` (`npm i -g @openai/codex`).

Peckboard fallback dirs (same as grok/cursor): `~/.local/bin`, `~/.npm-global/bin`, `~/.bun/bin`, `/usr/local/bin`.

Setting: `cli_path` (bare name or absolute).

## Spawn Argv — First Turn

Per-turn process, like grok/cursor — not a long-lived duplex child.
```text
codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check \
    [-c model_reasoning_effort=low|medium|high|xhigh] \
    [-m MODEL] \
    [--image PATH]... \
    [-C WORKDIR] \
    PROMPT
```
```

Capture command from the card (tmp git repo, unattended):

```bash
codex exec --json --sandbox workspace-write --skip-git-repo-check --ephemeral \
    -c approval_policy=never \
    "Reply with the single word OK."
```

**Do not pass `--ephemeral` for Peckboard sessions.** It skips rollout persistence; `codex exec resume <thread_id>` then has nothing to resume. Use `--ephemeral` only for one-shot probes.

`--skip-git-repo-check` is required if cwd is not a git repo. Codex otherwise refuses to run. Prefer a git workspace (folder clone) over the skip flag.

`--json` (alias `--experimental-json`) puts JSONL on **stdout**. Human progress stays on **stderr**. Without `--json`, stdout is the final agent message only.

Default exec sandbox is **read-only**. Unattended writes need `--sandbox workspace-write`. Values: `read-only` | `workspace-write` | `danger-full-access`.

`-c approval_policy=never` means "never ASK" — **not** "auto-approve". Any call codex decides needs approval is hard-errored (`MCP tool call requires approval, but approval policy is never`), which breaks every MCP tool call from an unattended session. Other values: `untrusted` | `on-request` | `never`. (`on-failure` deprecated.) There is no per-server pre-approval key: `mcp_servers.<name>.tool_approval` / `.trusted` / `.auto_approve` and `mcp_tool_approval` are all rejected by `--strict-config`. Peckboard therefore passes `--dangerously-bypass-approvals-and-sandbox` instead of `--sandbox` + `approval_policy=never` — see `mod.rs::build_cli_args`. The two are mutually exclusive.
`-c approval_policy=never` means "never ASK" — **not** "auto-approve". Any call codex decides needs approval is hard-errored (`MCP tool call requires approval, but approval policy is never`), which breaks every MCP tool call from an unattended session, read-only ones included. Other values: `untrusted` | `on-request` | `never`. (`on-failure` deprecated.) There is no narrower, per-server pre-approval key — probed under `--strict-config` on codex-cli 0.153.4, all rejected as unknown fields: `mcp_servers.<name>.{tool_approval,trusted,auto_approve,approval_policy,auto_approve_tools,always_allow}`, `mcp_tool_approval`, `mcp_approval_policy`, `trusted_mcp_servers`, `tools.auto_approve_mcp`, `tools.mcp.*`, `experimental_auto_approve_mcp`. Peckboard therefore passes `--dangerously-bypass-approvals-and-sandbox` instead of `--sandbox` + `approval_policy=never` — see `mod.rs::build_cli_args`. The two parse together, but the flag forces `danger-full-access`, so `--sandbox` alongside it is dead config.
`--full-auto` is deprecated (warning). Prefer `--sandbox workspace-write`.

`--cd` / `-C DIR` — workspace root. Also `--add-dir DIR` for extra writable roots.

ChatGPT login lives in `$CODEX_HOME/auth.json` (`codex login --device-auth`). Peckboard's in-app path is Settings → Codex Accounts. Env `CODEX_API_KEY` is not injected.

## Resume Argv

`thread_id` comes from the first JSONL line: `{"type":"thread.started","thread_id":"..."}`.

```text
codex exec resume <thread_id> --json \
    -c approval_policy=never \
    [-c sandbox_mode=workspace-write] \
    [-c model_reasoning_effort=low|medium|high|xhigh] \
    [-m MODEL] \
    [--image PATH]... \
    PROMPT
```

Also valid: flags **before** `resume` inherit onto the subcommand:
```text
codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check \
    resume <thread_id> PROMPT
```

This before-`resume` form is what Peckboard builds.
```

`--json`, `--skip-git-repo-check`, `--ephemeral`, `-c`/`--config`, `-m`/`--model` are **global** on `codex exec`. `--sandbox` is **not** global — `codex exec resume ID --sandbox workspace-write` is invalid. On resume, pass `--sandbox` before `resume`, or `-c sandbox_mode=workspace-write` after.

`--image` / `-i` is on the `resume` (and `fork`) subcommand itself; repeatable; comma-separated lists ok.

`codex exec resume --last [PROMPT]` resumes the newest session for the cwd. `--all` disables cwd filter. Peckboard should pass the captured `thread_id`, not `--last`.

`--last` without a separate prompt treats the positional as the prompt, not a session id.

## Model Flag

`-m` / `--model STRING` — override for this run. Global on exec + resume.

Probe: `codex debug models` (refreshes remote catalog) or `codex debug models --bundled` (binary-shipped JSON only).

Bundled picker-visible slugs (`codex-rs/models-manager/models.json`, 2026-09-09):

| slug            | display       | efforts                         |
| --------------- | ------------- | ------------------------------- |
| `gpt-6-astra`   | GPT-6-Astra   | low medium high xhigh max ultra |
| `gpt-5.6-sol`   | GPT-5.6-Sol   | low medium high xhigh max ultra |
| `gpt-5.6-terra` | GPT-5.6-Terra | low medium high xhigh max ultra |
| `gpt-5.6-luna`  | GPT-5.6-Luna  | low medium high xhigh max       |
| `gpt-5.5`       | GPT-5.5       | low medium high xhigh           |
| `gpt-5.2`       | GPT-5.2       | low medium high xhigh           |

Hidden (still in catalog): `gpt-5.4`, `gpt-5.4-mini`, `gpt-daybreak-blue-latest`, `gpt-daybreak-red-latest`, `codex-auto-review`.

Peckboard model ids: `codex:<slug>` (e.g. `codex:gpt-5.6-terra`). Default picker: `gpt-6-astra` (priority 1).

Live `codex debug models` may add/hide slugs. Re-run after install.

## Effort Override

```text
-c model_reasoning_effort=low|medium|high|xhigh
```

Config key `model_reasoning_effort`. TOML-parsed; bare `low` is not valid TOML so the CLI stores it as the string `low`.

Wire values from `ReasoningEffort`: `none` `minimal` `low` `medium` `high` `xhigh` `max` `ultra` `persistent` plus unknown customs. Peckboard effort picker maps to `low|medium|high|xhigh`. `max`/`ultra` exist on GPT-6 / 5.6 but are extra.

Plan-mode-only: `-c plan_mode_reasoning_effort=...` (also accepts `none`).

## Image Flag

```text
--image PATH
-i PATH
--image a.png,b.png
```

Repeatable. Attaches to the **first** message of that invocation (new thread or the resume prompt). Images are a user-turn modality (`input_modalities` includes `image` on current catalog models).

## JSONL Event Types

Source of truth: `codex-rs/exec/src/exec_events.rs` (mirrored in `sdk/typescript/src/events.ts` + `items.ts`). One JSON object per stdout line. Tag field: `type`.

### Top-level events

```jsonl
{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}
{"type":"turn.started"}
{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"cache_write_input_tokens":0,"output_tokens":122,"reasoning_output_tokens":0}}
{"type":"turn.failed","error":{"message":"model response stream ended unexpectedly"}}
{"type":"error","message":"Not logged in"}
```

- `thread.started` — always first, including on resume. `thread_id` is the resume handle.
- `turn.started` — empty object besides `type`.
- `turn.completed.usage` fields: `input_tokens`, `cached_input_tokens`, `cache_write_input_tokens` (default 0), `output_tokens`, `reasoning_output_tokens`.
- `turn.failed.error.message` — fatal turn error (exit 1).
- `error.message` — stream-level error. Transient `"Reconnecting... 1/5"` is non-fatal (turn continues). Treat other `error` lines as fatal.

No timestamps on these events. Unlike Claude, items are complete blocks — no token deltas on `agent_message`.

### Item events

`item.started` / `item.updated` / `item.completed` each wrap `item` with `id` + `type` + type-specific fields. Same `id` across the lifecycle.

Item `type` values (`snake_case`):

| type                | started | updated | completed | payload                                                      |
| ------------------- | ------- | ------- | --------- | ------------------------------------------------------------ |
| `agent_message`     | rare    | no      | yes       | `text`                                                       |
| `reasoning`         | rare    | no      | yes       | `text`                                                       |
| `command_execution` | yes     | no      | yes       | `command`, `aggregated_output`, `exit_code?`, `status`       |
| `file_change`       | no      | no      | yes       | `changes[{path,kind}]`, `status`                             |
| `mcp_tool_call`     | yes     | no      | yes       | `server`, `tool`, `arguments`, `result?`, `error?`, `status` |
| `collab_tool_call`  | yes     | maybe   | yes       | Rust-only; not in TS SDK yet                                 |
| `web_search`        | no      | no      | yes       | `query` (+ `action` in Rust)                                 |
| `todo_list`         | yes     | yes     | yes       | `items[{text,completed}]`                                    |
| `error`             | no      | no      | yes       | `message` (non-fatal warning)                                |

`command_execution.status`: `in_progress` | `completed` | `failed` | `declined`.
`file_change.changes[].kind`: `add` | `delete` | `update`.
`file_change.status`: `in_progress` | `completed` | `failed`.
`mcp_tool_call.status`: `in_progress` | `completed` | `failed`.

Examples:

```jsonl
{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"**Scanning docs**"}}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"docs\nsrc\n","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_2","type":"file_change","changes":[{"path":"README.md","kind":"update"}],"status":"completed"}}
{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"OK"}}
{"type":"item.started","item":{"id":"item_4","type":"todo_list","items":[{"text":"Scan docs","completed":false}]}}
{"type":"item.updated","item":{"id":"item_4","type":"todo_list","items":[{"text":"Scan docs","completed":true}]}}
{"type":"item.completed","item":{"id":"item_5","type":"mcp_tool_call","server":"docs","tool":"search","arguments":{"q":"exec"},"status":"completed"}}
{"type":"item.completed","item":{"id":"item_6","type":"web_search","query":"codex exec --json"}}
{"type":"item.completed","item":{"id":"item_7","type":"error","message":"command output truncated"}}
```

Parser map:

- `agent_message.text` → assistant message (last one is the final answer).
- `reasoning.text` → thinking/reasoning.
- `command_execution` → tool start/end (shell).
- `file_change` → file edits.
- `mcp_tool_call` → MCP tool.
- `todo_list` → plan/todo.
- `turn.completed` → `Completed` + usage.
- `turn.failed` / `error` → `Crashed`.

## Stderr Auth Markers

No live stderr (binary missing). Markers below from CLI source + `codex doctor` / `codex login status` reports. `CrashKind::classify` already matches most of them (`401`, `unauthorized`, `login`, `authentication`, `credential`, `not signed in`).

| Source                      | Marker (substring)                                 | Notes                                                                                |
| --------------------------- | -------------------------------------------------- | ------------------------------------------------------------------------------------ |
| `codex login status` stderr | `Not logged in`                                    | exit 1 when no `auth.json` / env key                                                 |
| `codex doctor --summary`    | `no Codex credentials were found`                  | follow-on: `Run codex login or provide an API key through a supported auth env var.` |
| JSONL stdout                | `{"type":"error","message":"..."}`                 | classify `message`                                                                   |
| JSONL stdout                | `{"type":"turn.failed","error":{"message":"..."}}` | classify nested message                                                              |
| HTTP                        | `401` / `Unauthorized`                             | ChatGPT backend or API key                                                           |
| HTTP                        | `Missing bearer or basic authentication`           | API-key path with no header                                                          |

Suggested `STDERR_MARKERS` (abort + `CrashKind::AuthExpired`):

- `Not logged in`
- `no Codex credentials were found`
- `Unauthorized (401)`
- `401 Unauthorized`

Auth stores:

- ChatGPT OAuth (Peckboard in-app path): `$CODEX_HOME/auth.json` via `codex login --device-auth`. Settings → Codex Accounts spawns that command per account.
- Host Default: `codex login` / `codex login --device-auth` writing `~/.codex/auth.json`.
- API key (`CODEX_API_KEY`) is not offered in Peckboard; the CLI still accepts it if the host env has it.
- Status: `codex login status` → `Logged in using ChatGPT` / `Logged in using an API key - …` / `Not logged in`.

## `codex --help` / `codex exec --help` (from source, not live)

`codex exec` (alias `codex e`):

- `--json` / `--experimental-json`
- `--output-last-message` / `-o FILE`
- `--output-schema FILE`
- `--skip-git-repo-check`
- `--ephemeral`
- `--ignore-user-config`
- `--ignore-rules`
- `--color always|never|auto`
- `--thread-source SOURCE`
- `--strict-config`
- subcommands: `resume`, `fork`, `review`
- prompt positional (or `-` for stdin)

Shared (exec + TUI):

- `--image` / `-i`
- `--model` / `-m`
- `--sandbox` / `-s` `read-only|workspace-write|danger-full-access`
- `--profile` / `-p`
- `--oss`, `--local-provider`
- `--cd` / `-C`
- `--add-dir`
- `--worktree`
- `--approve-for-me` (alias `--not-so-yolo`)
- `--dangerously-bypass-approvals-and-sandbox` (alias `--yolo`)
- `--dangerously-bypass-hook-trust`
- `-c` / `--config` `key=value` (global)

Other commands Peckboard may probe: `codex debug models [--bundled]`, `codex login status`, `codex doctor`.

## Implementer Notes

- One process per turn. Capture `thread_id` from `thread.started`. Next user message: `codex exec resume <thread_id> --json …`.
- `supports_mid_stream_injection() == false` (same as grok/cursor).
- `--dangerously-bypass-approvals-and-sandbox` is what Peckboard passes: it is the only switch that clears codex's MCP approval path, and it drops codex's sandbox with it (the session's folder scope is then the only boundary on agent-run commands).
- Workspace: session folder (git) via `-C` / spawn cwd. Never `~/.peckboard`.
- Model discovery: `codex debug models --bundled` (JSON stdout, no auth). Live catalog needs auth.
- Fixture `fixtures/hello.jsonl` is **synthetic**. Last two lines (`error` + `turn.failed`) exist so the parser card covers auth/fail shapes; a happy-path parser should stop at `turn.completed`.
