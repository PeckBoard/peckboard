//! Static MCP tool definitions — names, descriptions, and JSON Schemas
//! returned to clients via `tools/list`. Kept separate from the registry
//! and handlers because the schema list dominates the file by line count
//! and is otherwise read-only configuration.

use super::context::McpToolDef;

pub(super) fn tool_definitions() -> Vec<McpToolDef> {
    vec![
        McpToolDef {
            name: "complete_step".into(),
            description: "Mark the current workflow step as complete and advance to the next step.".into(),
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
            description: "Mark the card as finished (done). No further steps will run.".into(),
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
                    }
                },
                "required": ["title", "description"],
                "additionalProperties": false
            }),
        },
        McpToolDef {
            name: "list_cards".into(),
            description: "List all cards in a project. Uses current project context if available, or pass project_id explicitly.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_id": { "type": "string", "description": "Project ID (optional if in a worker session with project context)" }
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
            description: "List available workflow definitions.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
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
            description: "Update fields on an existing card.".into(),
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
            description: "Read the event history of another worker session in the same project. Use to understand what another worker did, see their tool calls, and review their work.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "The worker session ID to read" },
                    "last_n": { "type": "integer", "description": "Number of recent events to return (default 50, max 200)" }
                },
                "required": ["session_id"],
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
    ]
}
