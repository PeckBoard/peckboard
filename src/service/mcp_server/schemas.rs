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

pub(super) fn tool_definitions() -> Vec<McpToolDef> {
    vec![
        McpToolDef {
            name: "complete_step".into(),
            description: "Finish the CURRENT workflow step and hand off to the next worker for the NEXT step. Advances the card by EXACTLY ONE step — it does NOT finish the card. Use this ONLY when there is genuine remaining work for a later step. If you have completed ALL of the card's work, call `finish_card` instead — calling `complete_step` then would leave the card stalled in an early step and block every card that depends on it.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "handoff_context": {
                        "type": "string",
                        "description": "Context to pass to the next step's worker"
                    }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "finish_card".into(),
            description: "Mark the ENTIRE card as done. Moves the card straight to the terminal `done` step from whatever step it is currently on, which unblocks any cards that depend on it. Use this whenever all of the card's work is complete — even if the card is still on an early step like `backlog` or `in_progress`. Do NOT use `complete_step` to finish a card.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "Final summary of what was accomplished"
                    }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "wont_do_card".into(),
            description: "Mark the card as won't-do. Stops all work on this card.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "Reason this card cannot or should not be done"
                    }
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "ask_user".into(),
            description: "Ask the user one or more questions with multiple choice or fill-in-the-blank answers. The UI renders interactive controls. The tool returns when the user submits their answers.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "Array of questions to ask",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "type": "string", "description": "The question text" },
                                "header": { "type": "string", "description": "Category label (e.g. Setup, Input, Configuration)" },
                                "multiSelect": { "type": "boolean", "description": "true for checkboxes (multi), false for radio buttons (single). Default false." },
                                "options": {
                                    "type": "array",
                                    "description": "If provided, renders as multiple choice. Omit for free-form text input.",
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
            description: "Create a new card in a project. Uses current project context if available, or pass project_id explicitly.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Project ID (optional if in a worker session with project context)"
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
                        "description": "Priority (lower = higher priority)"
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
                        "description": "Optional effort level (low, medium, high, xhigh, max)"
                    },
                    "depends_on": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional ids of cards this card depends on. A worker only starts this card once every dependency is 'done'. Dependencies must be existing cards in the same project."
                    },
                    "blocked": {
                        "type": "boolean",
                        "description": "Optional. File the card already blocked so no worker picks it up until a human unblocks it. Defaults to true when `block_reason` is given."
                    },
                    "block_reason": {
                        "type": "string",
                        "description": "Optional reason the card is blocked at creation (e.g. 'needs human triage'). Setting this implies blocked=true unless `blocked` is set explicitly."
                    }
                },
                "required": ["title", "description"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_cards".into(),
            description: "List cards in a project. A project_id is required (or call from a worker session that already has project context); without one this returns no cards rather than every card across PeckBoard. Optionally filter by status. Each card includes a short summary of its description, not the full text.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": { "type": "string", "description": "Project ID to list cards for. Required unless the calling session already has project context. Without it, no cards are returned." },
                    "status": { "type": "string", "description": "Optional workflow step to filter by (e.g. backlog, in_progress, done, wont_do)." }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_card_dependencies".into(),
            description: "List the direct dependencies of a card — the cards it must wait on before a worker will pick it up. Each entry reports whether that prerequisite is 'done'.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": { "type": "string", "description": "ID of the card whose dependencies to list" }
                },
                "required": ["card_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "get_card_dependency_tree".into(),
            description: "Resolve the full transitive dependency tree of a card — its dependencies, their dependencies, and so on. Returns a nested tree plus whether every transitive prerequisite has reached 'done'.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": { "type": "string", "description": "ID of the card to resolve the dependency tree for" }
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
            description: "List available workflow definitions. Each step includes the built-in `instructions` text. If you pass `project_id`, every step that has a project-specific override also includes `project_instructions` — the additional text the project owner appended to the built-in prompt for that (workflow, step) combination.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Optional project id. When supplied, the response merges in any per-step `project_instructions` overrides that project has set."
                    }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "set_workflow_instructions".into(),
            description: "Set (or clear) the additional instructions a project appends to a workflow's per-step prompt. The text is appended below the built-in step instructions a worker receives — both apply. Pass an empty `instructions` string to clear the override.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Project whose workflow instructions to edit (optional if the session already has project context)."
                    },
                    "workflow_id": {
                        "type": "string",
                        "description": "Workflow id (e.g. `fast-develop-software`). Must exist in `list_workflows`."
                    },
                    "step": {
                        "type": "string",
                        "description": "Step name within the workflow (e.g. `in_progress`). Must be a step that runs a worker — terminal steps like `done`/`backlog` are rejected."
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Additional instructions to append to the built-in step prompt. Empty string clears the override."
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
            description: "Attach a file to a report folder. Accepts base64-encoded data with allowlisted extensions and a 10MB size cap.".into(),
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
            description: "Update fields on an existing card. Only the fields you pass are changed; omit a field to leave it as-is.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": {
                        "type": "string",
                        "description": "ID of the card to update"
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
                        "description": "New workflow id for the card (must be a known workflow). Pass `step` alongside it if the card's current step doesn't exist in the new workflow."
                    },
                    "model": {
                        "type": ["string", "null"],
                        "description": "New model override (e.g. claude-opus-4-8), or null to clear it and fall back to the project/host default."
                    },
                    "effort": {
                        "type": ["string", "null"],
                        "description": "New effort level (low, medium, high, xhigh, max), or null to clear it."
                    },
                    "depends_on": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Replace the card's dependency set with these card ids (must be cards in the same project; may not form a cycle). Pass an empty array to clear all dependencies. Omit to leave dependencies unchanged."
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
                        "description": "ID of the project to update"
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
            description: "Register a folder (working directory) for use with projects. Set create_if_missing=true to also create the directory on disk if it doesn't exist.".into(),
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
                        "description": "If true, create the directory on disk when it doesn't exist (default false)"
                    }
                },
                "required": ["name", "path"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "create_project".into(),
            description: "Create a new project in a folder. Provide either folder_id (existing folder) or folder_path (will look up by path, or create a new folder if path doesn't match any).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Project name"
                    },
                    "folder_id": {
                        "type": "string",
                        "description": "Folder ID to create the project in (use this OR folder_path)"
                    },
                    "folder_path": {
                        "type": "string",
                        "description": "Folder path: looks up by path; if no folder matches, registers one (and creates the dir on disk if create_folder_if_missing=true)"
                    },
                    "folder_name": {
                        "type": "string",
                        "description": "Folder display name; used when folder_path is given and a new folder needs to be registered (defaults to the basename of folder_path)"
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
                        "description": "Number of concurrent workers (default 1)"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "pause_project".into(),
            description: "Pause a project, preventing new work from being scheduled.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "ID of the project to pause"
                    }
                },
                "required": ["project_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "resume_project".into(),
            description: "Resume a paused project, allowing work to be scheduled again.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "ID of the project to resume"
                    }
                },
                "required": ["project_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "delete_project".into(),
            description: "Delete a project permanently. This cascades: all cards, worker sessions, and their events are removed.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "ID of the project to delete"
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
                        "description": "ID of the card to delete"
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
                        "description": "ID of the card to mark as done"
                    }
                },
                "required": ["card_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "move_card_to_wont_do".into(),
            description: "Move a card to the won't-do step, optionally with a reason.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "card_id": {
                        "type": "string",
                        "description": "ID of the card to mark as won't-do"
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
            description: "Broadcast a message to all other running workers in the same project. Use this to inform other workers about file changes, shared state updates, or coordination needs. Other workers will receive your message before their next action.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The message to broadcast (e.g. 'Modified src/auth/mod.rs — added JWT validation middleware')"
                    },
                    "files_changed": {
                        "type": "array",
                        "description": "List of file paths that were modified, created, or deleted",
                        "items": { "type": "string" }
                    }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "fetch_url".into(),
            description: "Fetch a URL from peckboard's server (bypasses bot protection that blocks the CLI's WebFetch). Use this when WebFetch returns 403 or is blocked. Returns the page text content.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
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
            description: "List all available AI models across all providers (including plugins). Use to see valid model IDs for card/project configuration.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "share_finding".into(),
            description: "Share a finding, discovery, or insight with all other running workers. This can be anything valuable: research results, data patterns, bugs, architectural decisions, experimental observations, domain knowledge, constraints, or any information that may help other workers. Broadcasts a summary; workers can retrieve full detail and ask follow-up questions.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "Brief summary of the finding (shown to all workers)" },
                    "detail": { "type": "string", "description": "Full detail (available on request via get_finding_details)" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags for categorization" }
                },
                "required": ["summary", "detail"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "get_finding_details".into(),
            description: "Retrieve the full detail of a finding shared by another worker.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "finding_id": { "type": "string", "description": "The finding event ID" }
                },
                "required": ["finding_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "send_worker_message".into(),
            description: "Send a direct message to another worker session. The message is queued and delivered on their next turn. Useful for asking follow-up questions about a finding.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_session_id": { "type": "string", "description": "The session ID of the worker to message" },
                    "message": { "type": "string", "description": "The message to send" }
                },
                "required": ["target_session_id", "message"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_project_reports".into(),
            description: "List all reports written by workers in the same project. Returns report titles, dates, and paths so you can read them.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "read_report".into(),
            description: "Read the full content of a report by its folder and file path.".into(),
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
            description: "Read the recent event history (tail) of another session in the same scope. Use to understand what another worker did, see their tool calls, and review their work. To find specific events (errors, a keyword) without pulling the whole transcript, use search_worker_session instead.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "The session ID to read" },
                    "last_n": { "type": "integer", "description": "Number of recent events to return (default 50, max 200)" }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "upgrade_plugin".into(),
            description: "Install or upgrade a Peckboard plugin from the configured plugin registry, by plugin id (e.g. \"common-tools\"). Downloads the version listed in the registry, verifies its checksum, and swaps it in. If the new version changed the plugin's hook set, it stays pending until an operator re-approves it. Use this to pick up a newer plugin release.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plugin_id": { "type": "string", "description": "The plugin id to install/upgrade (its registry id, e.g. \"common-tools\")." },
                    "repository": { "type": "string", "description": "Optional registry.json URL to restrict the search to a single repository." }
                },
                "required": ["plugin_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "set_session_system_prompt".into(),
            description: "Set (or clear) another session's system prompt. The text you set FULLY REPLACES that session's standing system prompt and takes effect on its next agent run. Pass system_prompt as a string to set it, or omit it / pass null to clear it (reverting to the default prompt). Works for any session you can reach (same folder, or same project for worker tokens).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "The session whose system prompt to edit." },
                    "system_prompt": { "type": "string", "description": "The full system prompt text. Omit or pass null to clear and revert to the default." }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_sessions".into(),
            description: "List every session you can read for debugging — chat, worker, and expert sessions alike. For a chat session this is all sessions in your folder; inside a project it's the project's sessions. Each entry has the session_id, name, kind (chat/worker/expert), and last activity so you can pick one to read or search.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "search_sessions".into(),
            description: "Search any session's event history for debugging WITHOUT reading the whole transcript — grep for a keyword, pull only error/failure events, or filter by event kind. Works for any session (chat, worker, or expert). Omit session_id to search across every session you can read at once (e.g. \"which session hit this error?\"). Returns only matching events, each tagged with its session_id and session_name. At least one of query, errors_only, or kinds is required. Use list_sessions to discover session ids, and read_worker_session for a full tail of one session.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session ID to search. Omit to search all sessions you can read (the current project, or your folder)." },
                    "query": { "type": "string", "description": "Case-insensitive substring to grep for across event text, tool names, inputs, and error messages." },
                    "errors_only": { "type": "boolean", "description": "Return only error/failure events: 'error' events, tool calls that returned an error, and crashed agent runs (default false)." },
                    "kinds": { "type": "array", "items": { "type": "string" }, "description": "Restrict to these event kinds, e.g. [\"agent-tool-end\", \"agent-text\"]." },
                    "limit": { "type": "integer", "description": "Max matching events to return (default 50, max 200)." }
                },
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_worker_sessions".into(),
            description: "List all worker sessions in the same project with their card titles and status. Use to find sessions you can read or message.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_repeating_tasks".into(),
            description: "List repeating tasks in this session's folder. Only available in non-project sessions. Each task has a schedule and a prompt that fires a fresh session on each tick.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "create_repeating_task".into(),
            description: "Create a repeating task in this session's folder. Only available in non-project sessions. Schedule is one of: interval ({\"minutes\": N}), daily ({\"hour\": H, \"minute\": M}), weekly ({\"weekday\": 0-6 (Mon=0), \"hour\": H, \"minute\": M}).".into(),
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
            description: "Edit a repeating task in this session's folder. Only available in non-project sessions. Pass only the fields you want to change.".into(),
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
            description: "Delete a repeating task in this session's folder. Only available in non-project sessions. Previously spawned sessions are preserved (but detached).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        },
    ]
}
