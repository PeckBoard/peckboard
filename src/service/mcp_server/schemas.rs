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
        "search_sessions",
    ]
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
        McpToolDef {
            name: "math".into(),
            description: "Evaluate an arithmetic expression and return the numeric result. Supports + - * / %, ^/** (power), parentheses, unary minus, the constants pi/e/tau, and functions: abs, sqrt, sin, cos, tan, asin, acos, atan, ln, log/log10, log2, exp, floor, ceil, round, sign, min, max, pow.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "expression": {
                                                "type": "string",
                                                "description": "The expression to evaluate, e.g. \"sqrt(2) * (3 + 4)^2\"."
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
            description: "Search the public web (via DuckDuckGo) and return the top results as a list of {title, url, snippet}. Use this to find pages when you don't already have a URL; then call fetch_web or parse_web on a result url to read it. Returns ranked results only — it does not fetch the linked pages.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "query": {
                                                "type": "string",
                                                "description": "What to search for, e.g. \"rust async runtime comparison\"."
                                },
                                "max_results": {
                                                "type": "integer",
                                                "description": "How many results to return (default 10, max 25)."
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
            description: "Fetch a public http(s) URL (GET or HEAD) and STORE the body, returning a compact reference plus a short preview instead of the whole page. Private/loopback hosts are blocked; redirects are not followed (the Location is surfaced as redirect_location so you can re-fetch it). Use web_get_part with the returned reference to read specific lines, a slice, or search matches.".into(),
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
            description: "Read a portion of a page previously stored by fetch_web or parse_web, addressed by its reference. Modes: 'info' (metadata: url, status, content_type, title, length, line_count), 'lines' (start_line + line_count), 'slice' (offset + length in characters), 'search' (find a query string, returns matching line numbers + snippets).".into(),
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
            description: "Convert an HTML page to readable text and extract its title, headings, and links. Pass either a `url` (fetched now) or a `reference` from a prior fetch_web. Returns the structure inline plus a text_reference you can read in full with web_get_part.".into(),
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
            description: "Search the contents of files in the current project folder for a literal string or regex, returning matching file paths, line numbers, and line text. Scoped to the caller's project folder (build/vendor/hidden dirs are skipped by the host).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "query": {
                                                "type": "string",
                                                "description": "Text or regex to search for."
                                },
                                "regex": {
                                                "type": "boolean",
                                                "description": "Treat query as a regular expression (default false = literal)."
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
            description: "List files in the current project folder (relative path + byte size). Optionally filter by a path substring. Scoped to the caller's folder.".into(),
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
            description: "Read a UTF-8 text file from the current project folder. AVOID reading whole files: for source code, prefer file_outline + read_symbol to find and read just the relevant function/class; otherwise pass a line window (start_line + line_count). Returns the whole file's content `hash` — keep it to pass as edit_file's original_hash. Path must be project-relative and stay within the folder.".into(),
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
                                                "description": "Number of lines to return from start_line (default 200)."
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
            description: "Write (or append to) a UTF-8 text file in the current project folder. Use this to CREATE new files or fully rewrite one — to modify an existing file, prefer edit_file (targeted, hash-guarded edits without resending the content). Overwrites by default; set append=true to append. Missing parent directories are created unless create_dirs=false. Returns the written content's `hash` for use with edit_file. Path must be project-relative and stay within the folder.".into(),
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
            description: "Modify an existing UTF-8 text file with targeted insert/update/delete operations instead of rewriting it. Guarded by optimistic concurrency: pass the `hash` you got from read_file / file_outline / read_symbol / write_file / a previous edit_file as original_hash — if the file on disk no longer matches, the edit is rejected and the current hash is reported so you can re-read and retry. All line/column numbers are 1-based and refer to the file BEFORE any of this call's edits (ranges must not overlap). Omit columns to operate on whole lines; provide columns for within-line edits (columns count characters; end_column is exclusive). Returns the new `hash` — use it as original_hash for your next edit of this file.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "path": {
                                                "type": "string",
                                                "description": "Project-relative path of an existing file."
                                },
                                "original_hash": {
                                                "type": "string",
                                                "description": "The file's content hash as last read; proves you are editing the version you think you are."
                                },
                                "edits": {
                                                "type": "array",
                                                "description": "The operations to apply, each addressed against the original file.",
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
                                                                                                "description": "insert: target line. Without `column`, `text` is inserted as new line(s) BEFORE this line (use total_lines+1 to append at end of file). With `column`, `text` is inserted inside this line at that character position."
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
                                                                                                "description": "insert/update: the new text. In whole-line mode it replaces the whole lines; multi-line strings are fine."
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
            description: "Parse a source file deterministically (no AI) and list its symbols — functions, classes, methods, types, etc. — with their kind, enclosing parent, 1-based start/end lines, and signature, plus the file's content `hash`. Supports Rust, TypeScript/JavaScript, Python, Go, Java/Kotlin, and C/C++ (by file extension). Use this FIRST to find the part of a file you need, then read_symbol (or read_file with a line window) — don't read whole files.".into(),
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
            description: "Read just the source of a named function/class/method from a file (languages as in file_outline), returning its content with start/end lines and the file's `hash` — ready to use with edit_file. Matches the exact symbol name; if several symbols share it, disambiguate with `kind` and/or `parent` (the enclosing class/impl, or a Go method's receiver type).".into(),
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
            description: "Run a READ-ONLY git subcommand in the current project folder and return its output (exit_code, stdout, stderr). Permitted subcommands: status, log, diff, show, branch, blame, ls-files, ls-tree, rev-parse, rev-list, describe, shortlog, tag, remote, for-each-ref, cat-file, name-rev, symbolic-ref, whatchanged, reflog. Mutating commands are rejected.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                                "subcommand": {
                                                "type": "string",
                                                "description": "The git subcommand, e.g. \"log\" or \"diff\"."
                                },
                                "args": {
                                                "type": "array",
                                                "items": {
                                                                "type": "string"
                                                },
                                                "description": "Additional argv passed after the subcommand, e.g. [\"--oneline\", \"-n\", \"10\"]."
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
            description: "Run an arbitrary command in the current project folder, gated by USER APPROVAL. If the program is on the operator allowlist or was previously approved 'always', it runs immediately. Otherwise this asks the user to approve (Approve once / Approve always / Deny) and returns status 'awaiting_approval' — the user's answer resumes the session, then you re-call run_command with the SAME command to proceed. Args are passed as an argv array (no shell); cwd is the project folder; output is capped and timed out.".into(),
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
                                                "description": "Arguments passed after the command, e.g. [\"-n\", \"TODO\", \"src\"]."
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
            description: "Run the project's test suite in the current folder and return the result (exit_code, passed, stdout, stderr). With runner=auto (default) the runner is detected from project markers (Cargo.toml→cargo, package.json→npm, go.mod→go, pyproject/pytest→pytest, pom.xml→maven, build.gradle→gradle, Gemfile→rspec, composer.json→phpunit). Pass `args` to forward extra arguments (e.g. a test name filter).".into(),
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
        ] {
            assert!(
                !worker_hidden_tool_names().contains(&essential),
                "essential worker tool {essential} must stay advertised"
            );
        }
    }
}
