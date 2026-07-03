//! Static MCP tool definitions — names, descriptions, and JSON Schemas
//! returned to clients via `tools/list`. Kept separate from the registry
//! and handlers because the schema list dominates the file by line count
//! and is otherwise read-only configuration.

use super::context::McpToolDef;

/// The canonical names of every MCP tool the Peckboard server exposes, in
/// definition order. Single source of truth: the MCP `tools/list` response and
/// each provider's `--allowedTools` allow-list both derive from this, so adding
/// a tool to [`tool_definitions`] offers it to every session without a
/// per-provider edit.
pub fn tool_names() -> Vec<String> {
    tool_definitions().into_iter().map(|t| t.name).collect()
}
/// Core tools NOT advertised to worker sessions via `tools/list`. Workers
/// never legitimately administer projects, folders, schedules, plugins, or
/// other chat sessions — and every schema dropped here is context that every
/// worker API call stops paying for. This trims ADVERTISEMENT only; the
/// per-handler scope checks (e.g. `ScopedFolderId`) remain the enforcement
/// point if a worker calls a hidden tool by name anyway.
pub fn worker_hidden_tool_names() -> &'static [&'static str] {
    &[
        "list_projects",
        "create_project",
        "update_project",
        "pause_project",
        "resume_project",
        "delete_project",
        "list_workflows",
        "set_workflow_instructions",
        "list_folders",
        "create_folder",
        "list_repeating_tasks",
        "create_repeating_task",
        "update_repeating_task",
        "delete_repeating_task",
        "upgrade_plugin",
        "set_session_system_prompt",
        "list_models",
        "list_sessions",
    ]
}

pub(super) fn tool_definitions() -> Vec<McpToolDef> {
    vec![
        McpToolDef {
            name: "complete_step".into(),
            description: "Finish the CURRENT workflow step and hand off to the next step's worker. Advances the card EXACTLY ONE step — does NOT finish the card. Use ONLY when real work remains for a later step. If ALL the card's work is done, call `finish_card` instead; `complete_step` then would strand the card in an early step and block every dependent card.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "handoff_context": {
                        "type": "string",
                        "description": "Context for the next step's worker"
                    }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "finish_card".into(),
            description: "Mark the ENTIRE card done. Jumps straight to the terminal `done` step from ANY current step (even `backlog`/`in_progress`), unblocking cards that depend on it. Use whenever all the card's work is complete. Do NOT use `complete_step` to finish a card.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "Final summary of what was done"
                    }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "wont_do_card".into(),
            description: "Mark the card won't-do. Stops all work on it.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "Why the card can't or shouldn't be done"
                    }
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "ask_user".into(),
            description: "Ask the user questions (multiple choice or fill-in-the-blank). UI renders interactive controls; returns when the user submits.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "Questions to ask",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "type": "string", "description": "The question text" },
                                "header": { "type": "string", "description": "Category label (e.g. Setup, Input, Configuration)" },
                                "multiSelect": { "type": "boolean", "description": "true = checkboxes (multi), false = radio (single). Default false." },
                                "options": {
                                    "type": "array",
                                    "description": "Provide for multiple choice; omit for free-form text.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string", "description": "Option text" },
                                            "description": { "type": "string", "description": "Help text below the label" }
                                        },
                                        "required": ["label", "description"]
                                    }
                                }
                            },
                            "required": ["question", "header"]
                        }
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "create_card".into(),
            description: "Create a card. Uses current project context, or pass project_id explicitly.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Project ID (optional if session has project context)"
                    },
                    "title": {
                        "type": "string",
                        "description": "Card title"
                    },
                    "description": {
                        "type": "string",
                        "description": "Card description / instructions"
                    },
                    "priority": {
                        "type": "integer",
                        "description": "Priority (lower = higher)"
                    },
                    "workflow": {
                        "type": "string",
                        "description": "Optional workflow override"
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model override (e.g. claude-opus-4-8)"
                    },
                    "effort": {
                        "type": "string",
                        "description": "Effort level: low, medium, high, xhigh, max"
                    },
                    "depends_on": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Card ids this card depends on. Work starts only once every dependency is 'done'. Must be existing same-project cards."
                    },
                    "blocked": {
                        "type": "boolean",
                        "description": "Create the card already blocked; no worker picks it up until a human unblocks. Defaults true when `block_reason` is given."
                    },
                    "block_reason": {
                        "type": "string",
                        "description": "Reason blocked at creation (e.g. 'needs human triage'). Implies blocked=true unless `blocked` is set explicitly."
                    }
                },
                "required": ["title", "description"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_cards".into(),
            description: "List cards in a project. Requires project_id (or worker-session project context) — without it returns NO cards, not every card in PeckBoard. Optional status filter. Cards include a description summary, not full text.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": { "type": "string", "description": "Project ID. Required unless the session has project context; without it, no cards returned." },
                    "status": { "type": "string", "description": "Workflow step filter (e.g. backlog, in_progress, done, wont_do)." }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_card_dependencies".into(),
            description: "List a card's direct dependencies — cards it must wait on before pickup. Each entry reports whether it is 'done'.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": { "type": "string", "description": "Card whose dependencies to list" }
                },
                "required": ["card_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "get_card_dependency_tree".into(),
            description: "Resolve a card's full transitive dependency tree (nested), plus whether every transitive prerequisite is 'done'.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": { "type": "string", "description": "Card to resolve the tree for" }
                },
                "required": ["card_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_projects".into(),
            description: "List all projects.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_workflows".into(),
            description: "List workflow definitions. Each step includes built-in `instructions`. With `project_id`, steps with project overrides also include `project_instructions` — text the project appends to the built-in prompt.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Optional project id; merges in that project's per-step `project_instructions` overrides."
                    }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "set_workflow_instructions".into(),
            description: "Set (or clear) the project-specific instructions appended below a workflow step's built-in prompt (both apply). Empty `instructions` clears the override.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Project to edit (optional if session has project context)."
                    },
                    "workflow_id": {
                        "type": "string",
                        "description": "Workflow id (must exist in `list_workflows`)."
                    },
                    "step": {
                        "type": "string",
                        "description": "Step name (e.g. `in_progress`). Must run a worker — terminal steps (`done`/`backlog`) rejected."
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Text appended to the built-in step prompt. Empty string clears."
                    }
                },
                "required": ["workflow_id", "step", "instructions"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "write_report".into(),
            description: "Write a report or note to the event log for human review.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Report title"
                    },
                    "body": {
                        "type": "string",
                        "description": "Report body (markdown)"
                    }
                },
                "required": ["title", "body"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "attach_report_file".into(),
            description: "Attach a file to a report folder. Base64 data, allowlisted extensions, 10MB cap.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "folder": {
                        "type": "string",
                        "description": "Report folder name (e.g. date string)"
                    },
                    "file": {
                        "type": "string",
                        "description": "File base name (without extension)"
                    },
                    "data": {
                        "type": "string",
                        "description": "Base64-encoded file content"
                    },
                    "extension": {
                        "type": "string",
                        "description": "File extension (e.g. png, pdf, csv, json, txt, md, html, svg, jpg, jpeg, gif, webp)"
                    }
                },
                "required": ["folder", "file", "data", "extension"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "update_card".into(),
            description: "Update card fields. Only passed fields change; omit to leave as-is.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": {
                        "type": "string",
                        "description": "Card ID"
                    },
                    "title": {
                        "type": "string",
                        "description": "New card title"
                    },
                    "description": {
                        "type": "string",
                        "description": "New card description"
                    },
                    "priority": {
                        "type": "integer",
                        "description": "New priority value"
                    },
                    "step": {
                        "type": "string",
                        "description": "New workflow step"
                    },
                    "workflow": {
                        "type": "string",
                        "description": "New workflow id (must be known). Pass `step` too if the current step isn't in the new workflow."
                    },
                    "model": {
                        "type": ["string", "null"],
                        "description": "Model override (e.g. claude-opus-4-8), or null to clear (falls back to project/host default)."
                    },
                    "effort": {
                        "type": ["string", "null"],
                        "description": "Effort (low, medium, high, xhigh, max), or null to clear."
                    },
                    "depends_on": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "REPLACES the dependency set (same-project cards, no cycles). Empty array clears all; omit to leave unchanged."
                    },
                    "blocked": {
                        "type": "boolean",
                        "description": "Whether the card is blocked"
                    },
                    "block_reason": {
                        "type": "string",
                        "description": "Reason the card is blocked"
                    }
                },
                "required": ["card_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "update_project".into(),
            description: "Update fields on an existing project.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Project ID"
                    },
                    "name": {
                        "type": "string",
                        "description": "New project name"
                    },
                    "context": {
                        "type": "string",
                        "description": "New project context"
                    },
                    "worker_count": {
                        "type": "integer",
                        "description": "New worker count"
                    },
                    "status": {
                        "type": "string",
                        "description": "New project status"
                    }
                },
                "required": ["project_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_folders".into(),
            description: "List all folders (working directories) available for projects.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "create_folder".into(),
            description: "Register a folder (working directory) for projects. create_if_missing=true also creates the directory on disk.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Display name for the folder"
                    },
                    "path": {
                        "type": "string",
                        "description": "Absolute filesystem path to the folder"
                    },
                    "create_if_missing": {
                        "type": "boolean",
                        "description": "Create the directory on disk if missing (default false)"
                    }
                },
                "required": ["name", "path"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "create_project".into(),
            description: "Create a project in a folder. Give folder_id (existing) OR folder_path (looked up by path; registers a new folder if none matches).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Project name"
                    },
                    "folder_id": {
                        "type": "string",
                        "description": "Folder ID (use this OR folder_path)"
                    },
                    "folder_path": {
                        "type": "string",
                        "description": "Looked up by path; if none matches, registers a new folder (creates dir on disk if create_folder_if_missing=true)"
                    },
                    "folder_name": {
                        "type": "string",
                        "description": "Display name for a newly registered folder (default: basename of folder_path)"
                    },
                    "create_folder_if_missing": {
                        "type": "boolean",
                        "description": "When folder_path is given: create directory on disk if missing (default false)"
                    },
                    "context": {
                        "type": "string",
                        "description": "Project context / instructions"
                    },
                    "worker_count": {
                        "type": "integer",
                        "description": "Concurrent workers (default 1)"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "pause_project".into(),
            description: "Pause a project; no new work is scheduled.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Project ID"
                    }
                },
                "required": ["project_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "resume_project".into(),
            description: "Resume a paused project; scheduling restarts.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Project ID"
                    }
                },
                "required": ["project_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "delete_project".into(),
            description: "Delete a project PERMANENTLY. Cascades: removes all cards, worker sessions, and their events.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Project ID"
                    }
                },
                "required": ["project_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "delete_card".into(),
            description: "Delete a card permanently.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": {
                        "type": "string",
                        "description": "Card ID"
                    }
                },
                "required": ["card_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "move_card_to_done".into(),
            description: "Move a card to the done step.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": {
                        "type": "string",
                        "description": "Card ID"
                    }
                },
                "required": ["card_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "move_card_to_wont_do".into(),
            description: "Move a card to won't-do, optionally with a reason.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": {
                        "type": "string",
                        "description": "Card ID"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Reason the card won't be done"
                    }
                },
                "required": ["card_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "notify_workers".into(),
            description: "Broadcast to all other running workers in the project (file changes, shared state, coordination). Delivered before their next action.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Message to broadcast (e.g. 'Modified src/auth/mod.rs — added JWT middleware')"
                    },
                    "files_changed": {
                        "type": "array",
                        "description": "File paths modified/created/deleted",
                        "items": { "type": "string" }
                    }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "fetch_url".into(),
            description: "Fetch a URL via peckboard's server — bypasses bot protection; use when WebFetch is 403/blocked. Returns page text.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to fetch"
                    },
                    "max_length": {
                        "type": "integer",
                        "description": "Max response length in chars (default 10000)"
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_models".into(),
            description: "List AI models across all providers (incl. plugins) — the valid model IDs for card/project config.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "share_finding".into(),
            description: "Share a finding with all running workers — anything valuable: research, data patterns, bugs, decisions, constraints. Broadcasts the summary; workers can fetch full detail and ask follow-ups.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "Brief summary (broadcast to all workers)" },
                    "detail": { "type": "string", "description": "Full detail (available on request via get_finding_details)" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags for categorization" }
                },
                "required": ["summary", "detail"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "get_finding_details".into(),
            description: "Get the full detail of a finding shared by another worker.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "finding_id": { "type": "string", "description": "Finding event ID" }
                },
                "required": ["finding_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "send_worker_message".into(),
            description: "Direct-message another worker session; queued, delivered on their next turn. Good for finding follow-ups.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_session_id": { "type": "string", "description": "Target worker's session ID" },
                    "message": { "type": "string", "description": "Message text" }
                },
                "required": ["target_session_id", "message"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_project_reports".into(),
            description: "List reports written by workers in this project (titles, dates, paths).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "read_report".into(),
            description: "Read a report's full content by folder and file.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "folder": { "type": "string", "description": "Report folder (e.g. 2026-06-07)" },
                    "file": { "type": "string", "description": "Report filename (e.g. my-report.md)" }
                },
                "required": ["folder", "file"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "read_worker_session".into(),
            description: "Read the recent event tail of another same-scope session — see what a worker did and its tool calls. For specific events (errors, a keyword) without the whole transcript, use search_sessions.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session ID to read" },
                    "last_n": { "type": "integer", "description": "Recent events to return (default 50, max 200)" }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "upgrade_plugin".into(),
            description: "Install/upgrade a Peckboard plugin from the registry by id (e.g. \"common-tools\"): downloads the registry version, verifies checksum, swaps it in. If the hook set changed, stays pending until an operator re-approves.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plugin_id": { "type": "string", "description": "Registry plugin id (e.g. \"common-tools\")." },
                    "repository": { "type": "string", "description": "Optional registry.json URL to limit to one repository." }
                },
                "required": ["plugin_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "set_session_system_prompt".into(),
            description: "Set (or clear) another session's system prompt. FULLY REPLACES the standing prompt; takes effect on its next agent run. Omit / pass null to clear to the default. Works on any reachable session (same folder, or same project for worker tokens).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session whose prompt to edit." },
                    "system_prompt": { "type": "string", "description": "Full prompt text. Omit or pass null to clear to the default." }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_sessions".into(),
            description: "List every readable session (chat/worker/expert) — folder-wide for chat sessions, project-wide inside a project. Entries: session_id, name, kind, last activity.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "search_sessions".into(),
            description: "Search session event history WITHOUT reading whole transcripts — keyword grep, errors-only, or event-kind filter. Any session kind; omit session_id to search all readable sessions at once. Returns matching events tagged with session_id/session_name. At least one of query, errors_only, kinds required. list_sessions finds ids; read_worker_session gives a full tail.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session to search. Omit to search all readable sessions." },
                    "query": { "type": "string", "description": "Case-insensitive substring over event text, tool names, inputs, error messages." },
                    "errors_only": { "type": "boolean", "description": "Only error/failure events: 'error' events, failed tool calls, crashed runs (default false)." },
                    "kinds": { "type": "array", "items": { "type": "string" }, "description": "Restrict to these event kinds, e.g. [\"agent-tool-end\", \"agent-text\"]." },
                    "limit": { "type": "integer", "description": "Max matches (default 50, max 200)." }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_worker_sessions".into(),
            description: "List project worker sessions with card titles and status — find sessions to read or message.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_repeating_tasks".into(),
            description: "List repeating tasks in this session's folder (non-project sessions only). Each has a schedule + prompt that fires a fresh session per tick.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "create_repeating_task".into(),
            description: "Create a repeating task in this session's folder (non-project sessions only). Schedule: interval ({\"minutes\": N}), daily ({\"hour\": H, \"minute\": M}), weekly ({\"weekday\": 0-6 Mon=0, \"hour\": H, \"minute\": M}).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Display name" },
                    "description": { "type": "string", "description": "Informational description (optional)" },
                    "prompt": { "type": "string", "description": "Prompt sent to the new session on each run" },
                    "schedule_kind": { "type": "string", "enum": ["interval", "daily", "weekly"] },
                    "schedule_value": { "type": "object", "description": "Schedule parameters per kind" },
                    "model": { "type": "string", "description": "Override model id (optional)" },
                    "effort": { "type": "string", "description": "Override effort level (optional)" },
                    "enabled": { "type": "boolean", "description": "Whether the schedule fires (default true)" }
                },
                "required": ["name", "prompt", "schedule_kind", "schedule_value"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "update_repeating_task".into(),
            description: "Edit a repeating task (non-project sessions only). Pass only fields to change.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "name": { "type": "string" },
                    "description": { "type": "string" },
                    "prompt": { "type": "string" },
                    "schedule_kind": { "type": "string", "enum": ["interval", "daily", "weekly"] },
                    "schedule_value": { "type": "object" },
                    "model": { "type": ["string", "null"] },
                    "effort": { "type": ["string", "null"] },
                    "enabled": { "type": "boolean" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "delete_repeating_task".into(),
            description: "Delete a repeating task (non-project sessions only). Spawned sessions are preserved but detached.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "math".into(),
            description: "Evaluate an arithmetic expression; returns the number. Supports + - * / %, ^/** (power), parentheses, unary minus, pi/e/tau, and abs, sqrt, sin, cos, tan, asin, acos, atan, ln, log/log10, log2, exp, floor, ceil, round, sign, min, max, pow.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "expression": {
                                                "type": "string",
                                                "description": "Expression, e.g. \"sqrt(2) * (3 + 4)^2\"."
                                }
                },
                "required": [
                                "expression"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "search_web".into(),
            description: "Search the public web (DuckDuckGo); returns top results as {title, url, snippet}. Use when you lack a URL, then fetch_web/parse_web a result url. Ranked results only — does NOT fetch pages.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "query": {
                                                "type": "string",
                                                "description": "Search query."
                                },
                                "max_results": {
                                                "type": "integer",
                                                "description": "Result count (default 10, max 25)."
                                }
                },
                "required": [
                                "query"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "fetch_web".into(),
            description: "Fetch a public http(s) URL (GET/HEAD) and STORE the body — returns a compact reference + short preview, not the whole page. Private/loopback hosts blocked; redirects not followed (Location surfaced as redirect_location). Read parts via web_get_part with the reference.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "url": {
                                                "type": "string",
                                                "description": "Absolute http(s) URL to fetch."
                                },
                                "method": {
                                                "type": "string",
                                                "enum": [
                                                                "GET",
                                                                "HEAD"
                                                ],
                                                "description": "HTTP method (default GET)."
                                },
                                "headers": {
                                                "type": "object",
                                                "description": "Optional extra request headers (string→string).",
                                                "additionalProperties": {
                                                                "type": "string"
                                                }
                                }
                },
                "required": [
                                "url"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "web_get_part".into(),
            description: "Read part of a page stored by fetch_web/parse_web, by reference. Modes: 'info' (url, status, content_type, title, length, line_count), 'lines' (start_line + line_count), 'slice' (offset + length chars), 'search' (query → matching line numbers + snippets).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "reference": {
                                                "type": "string",
                                                "description": "Reference id returned by fetch_web/parse_web."
                                },
                                "mode": {
                                                "type": "string",
                                                "enum": [
                                                                "info",
                                                                "lines",
                                                                "slice",
                                                                "search"
                                                ],
                                                "description": "What to return (default info)."
                                },
                                "start_line": {
                                                "type": "integer",
                                                "description": "lines mode: 1-based first line."
                                },
                                "line_count": {
                                                "type": "integer",
                                                "description": "lines mode: how many lines (default 100)."
                                },
                                "offset": {
                                                "type": "integer",
                                                "description": "slice mode: 0-based character offset."
                                },
                                "length": {
                                                "type": "integer",
                                                "description": "slice mode: number of characters (default 4000)."
                                },
                                "query": {
                                                "type": "string",
                                                "description": "search mode: substring to find."
                                },
                                "case_insensitive": {
                                                "type": "boolean",
                                                "description": "search mode: case-insensitive (default true)."
                                },
                                "max_matches": {
                                                "type": "integer",
                                                "description": "search mode: cap on matches (default 30)."
                                }
                },
                "required": [
                                "reference"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "parse_web".into(),
            description: "Convert HTML to readable text; extract title, headings, links. Pass `url` (fetched now) or `reference` from a prior fetch_web. Returns structure inline + a text_reference readable via web_get_part.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "url": {
                                                "type": "string",
                                                "description": "Absolute http(s) URL to fetch and parse."
                                },
                                "reference": {
                                                "type": "string",
                                                "description": "Reference of an already-fetched page to parse instead of fetching."
                                }
                },
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "search_files".into(),
            description: "Search project-folder file contents for a literal string or regex; returns matching paths, line numbers, line text. Build/vendor/hidden dirs skipped.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "query": {
                                                "type": "string",
                                                "description": "Text or regex to search for."
                                },
                                "regex": {
                                                "type": "boolean",
                                                "description": "Treat query as regex (default false = literal)."
                                },
                                "case_insensitive": {
                                                "type": "boolean",
                                                "description": "Case-insensitive match (default false)."
                                },
                                "path_contains": {
                                                "type": "string",
                                                "description": "Only search files whose path contains this substring."
                                },
                                "max_results": {
                                                "type": "integer",
                                                "description": "Cap on returned matches (default 200)."
                                }
                },
                "required": [
                                "query"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "list_files".into(),
            description: "List project-folder files (relative path + byte size); optional path-substring filter.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "path_contains": {
                                                "type": "string",
                                                "description": "Only list files whose path contains this substring."
                                },
                                "max": {
                                                "type": "integer",
                                                "description": "Cap on returned files (default 1000)."
                                }
                },
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "read_file".into(),
            description: "Read a UTF-8 file in the project folder. Targeted reads ENFORCED: whole-file only ≤400 lines, windows (start_line + line_count) ≤600 lines — larger reads are rejected; locate the range first with file_outline / read_symbol / search_files. Returns the content `hash` — keep it for edit_file's original_hash. Path project-relative, inside the folder.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "path": {
                                                "type": "string",
                                                "description": "Project-relative file path."
                                },
                                "start_line": {
                                                "type": "integer",
                                                "description": "1-based first line of an optional window."
                                },
                                "line_count": {
                                                "type": "integer",
                                                "description": "Number of lines to return from start_line (default 200, max 600)."
                                }
                },
                "required": [
                                "path"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "write_file".into(),
            description: "Write (or append) a UTF-8 file in the project folder. For CREATING files or full rewrites — to modify existing, prefer edit_file (targeted, hash-guarded). Overwrites by default; append=true appends. Parent dirs created unless create_dirs=false. Returns `hash` for edit_file. Path project-relative, inside the folder.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "path": {
                                                "type": "string",
                                                "description": "Project-relative file path to write."
                                },
                                "content": {
                                                "type": "string",
                                                "description": "The full text to write (or to append when append=true)."
                                },
                                "append": {
                                                "type": "boolean",
                                                "description": "Append to the file instead of overwriting (default false)."
                                },
                                "create_dirs": {
                                                "type": "boolean",
                                                "description": "Create missing parent directories (default true)."
                                }
                },
                "required": [
                                "path",
                                "content"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "edit_file".into(),
            description: "Modify an existing UTF-8 file with targeted insert/update/delete ops instead of rewriting. Hash-guarded: pass the `hash` from read_file/file_outline/read_symbol/write_file/a prior edit_file as original_hash — on mismatch the edit is rejected and the current hash reported (re-read and retry). Line/column numbers are 1-based and address the file BEFORE this call's edits (ranges must not overlap). Omit columns for whole-line ops; give columns for within-line edits (chars; end_column exclusive). Returns the new `hash` for your next edit.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "path": {
                                                "type": "string",
                                                "description": "Project-relative path of an existing file."
                                },
                                "original_hash": {
                                                "type": "string",
                                                "description": "Content hash as last read; proves you're editing the expected version."
                                },
                                "edits": {
                                                "type": "array",
                                                "description": "Ops to apply, each addressed against the original file.",
                                                "items": {
                                                                "type": "object",
                                                                "properties": {
                                                                                "op": {
                                                                                                "type": "string",
                                                                                                "enum": [
                                                                                                                "insert",
                                                                                                                "update",
                                                                                                                "delete"
                                                                                                ],
                                                                                                "description": "What to do."
                                                                                },
                                                                                "line": {
                                                                                                "type": "integer",
                                                                                                "description": "insert: target line. Without `column`, `text` becomes new line(s) BEFORE this line (total_lines+1 appends at EOF). With `column`, inserted inside the line at that character position."
                                                                                },
                                                                                "column": {
                                                                                                "type": "integer",
                                                                                                "description": "insert only: 1-based character position within `line`."
                                                                                },
                                                                                "start_line": {
                                                                                                "type": "integer",
                                                                                                "description": "update/delete: first line of the range."
                                                                                },
                                                                                "start_column": {
                                                                                                "type": "integer",
                                                                                                "description": "update/delete: optional 1-based start character (inclusive). Provide both columns or neither."
                                                                                },
                                                                                "end_line": {
                                                                                                "type": "integer",
                                                                                                "description": "update/delete: last line of the range (inclusive in whole-line mode)."
                                                                                },
                                                                                "end_column": {
                                                                                                "type": "integer",
                                                                                                "description": "update/delete: optional 1-based end character (exclusive)."
                                                                                },
                                                                                "text": {
                                                                                                "type": "string",
                                                                                                "description": "insert/update: the new text. Whole-line mode replaces whole lines; multi-line ok."
                                                                                }
                                                                },
                                                                "required": [
                                                                                "op"
                                                                ],
                                                                "additionalProperties": false
                                                }
                                }
                },
                "required": [
                                "path",
                                "original_hash",
                                "edits"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "file_outline".into(),
            description: "Deterministically parse a source file (no AI) and list its symbols — functions, classes, methods, types — with kind, enclosing parent, 1-based start/end lines, signature, plus the file's `hash`. Supports Rust, TS/JS, Python, Go, Java/Kotlin, C/C++ (by extension). Use FIRST to locate what you need, then read_symbol or a read_file window — don't read whole files.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "path": {
                                                "type": "string",
                                                "description": "Project-relative path of the source file."
                                },
                                "name_contains": {
                                                "type": "string",
                                                "description": "Only list symbols whose name contains this substring (case-insensitive)."
                                },
                                "max": {
                                                "type": "integer",
                                                "description": "Cap on returned symbols (default 200, max 1000)."
                                }
                },
                "required": [
                                "path"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "read_symbol".into(),
            description: "Read one named function/class/method from a file (languages as in file_outline); returns content, start/end lines, and the file's `hash` — ready for edit_file. Exact-name match; disambiguate duplicates with `kind` and/or `parent` (enclosing class/impl, or Go receiver type).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "path": {
                                                "type": "string",
                                                "description": "Project-relative path of the source file."
                                },
                                "name": {
                                                "type": "string",
                                                "description": "Exact symbol name, e.g. \"handle_invoke\" or \"Repo\"."
                                },
                                "kind": {
                                                "type": "string",
                                                "description": "Optional filter, e.g. fn/function/method/class/struct/impl/trait."
                                },
                                "parent": {
                                                "type": "string",
                                                "description": "Optional filter: name of the enclosing container (class, impl, module) or a Go receiver type."
                                }
                },
                "required": [
                                "path",
                                "name"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "git".into(),
            description: "Run a READ-ONLY git subcommand in the project folder; returns exit_code, stdout, stderr. Allowed: status, log, diff, show, branch, blame, ls-files, ls-tree, rev-parse, rev-list, describe, shortlog, tag, remote, for-each-ref, cat-file, name-rev, symbolic-ref, whatchanged, reflog. Mutating commands rejected.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "subcommand": {
                                                "type": "string",
                                                "description": "Subcommand, e.g. \"log\" or \"diff\"."
                                },
                                "args": {
                                                "type": "array",
                                                "items": {
                                                                "type": "string"
                                                },
                                                "description": "argv after the subcommand, e.g. [\"--oneline\", \"-n\", \"10\"]."
                                },
                                "timeout_secs": {
                                                "type": "integer",
                                                "description": "Optional timeout (default 120, max 600)."
                                }
                },
                "required": [
                                "subcommand"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "run_command".into(),
            description: "Run an arbitrary command in the project folder, gated by USER APPROVAL. Allowlisted or 'always'-approved programs run immediately; otherwise returns status 'awaiting_approval' while the user picks Approve once / Approve always / Deny — their answer resumes the session, then re-call run_command with the SAME command. Args are argv (no shell); cwd = project folder; output capped and timed out.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "command": {
                                                "type": "string",
                                                "description": "Bare executable name (no path, no shell), e.g. \"rg\"."
                                },
                                "args": {
                                                "type": "array",
                                                "items": {
                                                                "type": "string"
                                                },
                                                "description": "argv after the command, e.g. [\"-n\", \"TODO\", \"src\"]."
                                },
                                "timeout_secs": {
                                                "type": "integer",
                                                "description": "Optional timeout (default 120, max 600)."
                                }
                },
                "required": [
                                "command"
                ],
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "run_tests".into(),
            description: "Run the project's test suite; returns exit_code, passed, stdout, stderr. runner=auto (default) detects from markers: Cargo.toml→cargo, package.json→npm, go.mod→go, pyproject/pytest→pytest, pom.xml→maven, build.gradle→gradle, Gemfile→rspec, composer.json→phpunit. Pass `args` for extras (e.g. a test filter).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "runner": {
                                                "type": "string",
                                                "enum": [
                                                                "auto",
                                                                "cargo",
                                                                "npm",
                                                                "pnpm",
                                                                "yarn",
                                                                "pytest",
                                                                "go",
                                                                "gradle",
                                                                "maven",
                                                                "rspec",
                                                                "phpunit",
                                                                "dotnet"
                                                ],
                                                "description": "Which runner to use (default auto)."
                                },
                                "args": {
                                                "type": "array",
                                                "items": {
                                                                "type": "string"
                                                },
                                                "description": "Extra arguments forwarded to the runner."
                                },
                                "timeout_secs": {
                                                "type": "integer",
                                                "description": "Optional timeout (default 300, max 600)."
                                }
                },
                "additionalProperties": false
}),
        },
        McpToolDef {
            name: "browser_open".into(),
            description: "Open URL in the managed headless browser for web testing. Returns page_id + compressed page outline (~91% smaller than DOM; elements carry ref=eN handles for browser_act). First call may take a minute (server + browser spin-up).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to open." },
                    "name": { "type": "string", "description": "Short page label." }
                },
                "required": ["url"]
            }),
        },
        McpToolDef {
            name: "browser_outline".into(),
            description: "Compressed structure of an open page (list-folded, ref=eN handles). Call after actions that change the page. Prefer browser_find for locating specific content.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "description": "From browser_open." }
                },
                "required": ["page_id"]
            }),
        },
        McpToolDef {
            name: "browser_find".into(),
            description: "Regex-search the page snapshot (ripgrep) instead of reading the whole outline — returns matching lines with their ref=eN handles.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "description": "From browser_open." },
                    "pattern": { "type": "string", "description": "Regex (alternatives OK: `login|sign in`)." },
                    "ignore_case": { "type": "boolean", "description": "Default true." },
                    "line_limit": { "type": "integer", "description": "Max result lines (default 50, max 100)." }
                },
                "required": ["page_id", "pattern"]
            }),
        },
        McpToolDef {
            name: "browser_act".into(),
            description: "Interact with an open page. action: click|type|fill|select|hover|press_key|navigate|back|forward|scroll_top|scroll_bottom|wait_selector|wait_ms|dialog. Element actions take `ref` (eN from outline/find); type/fill/select/press_key/navigate/wait_selector put their argument in `text`. Set outline=true to get the fresh page structure in the same call.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "description": "From browser_open." },
                    "action": { "type": "string", "description": "One of the actions above." },
                    "ref": { "type": "string", "description": "Element handle, e.g. e12." },
                    "text": { "type": "string", "description": "Text/value/key/url/selector for the action." },
                    "timeout_ms": { "type": "integer", "description": "wait_ms duration (max 30000)." },
                    "accept": { "type": "boolean", "description": "dialog: accept or dismiss (default true)." },
                    "outline": { "type": "boolean", "description": "Also return the post-action outline (default false; costs ~2k tokens)." }
                },
                "required": ["page_id", "action"]
            }),
        },
        McpToolDef {
            name: "browser_screenshot".into(),
            description: "Screenshot an open page (PNG, returned as image). Viewport by default; full_page=true for the whole page.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "description": "From browser_open." },
                    "full_page": { "type": "boolean", "description": "Default false." }
                },
                "required": ["page_id"]
            }),
        },
        McpToolDef {
            name: "browser_close".into(),
            description: "Close an open browser page when done testing.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "description": "From browser_open." }
                },
                "required": ["page_id"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_hidden_tools_exist_and_spare_worker_essentials() {
        let names = tool_names();
        for hidden in worker_hidden_tool_names() {
            assert!(
                names.iter().any(|n| n == hidden),
                "hidden tool {hidden} is not a core tool"
            );
        }
        for essential in [
            "complete_step",
            "finish_card",
            "wont_do_card",
            "ask_user",
            "list_cards",
            "create_card",
            "share_finding",
            "write_report",
            "read_report",
            "list_project_reports",
            "send_worker_message",
            "list_worker_sessions",
            "read_worker_session",
            "read_file",
            "edit_file",
            "file_outline",
            "read_symbol",
            "search_files",
            "run_command",
            "run_tests",
            "search_sessions",
        ] {
            assert!(
                !worker_hidden_tool_names().contains(&essential),
                "essential worker tool {essential} must stay advertised"
            );
        }
    }
}
