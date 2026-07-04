//! Sandbox gate for Claude CLI tool calls.
//!
//! Two jobs: (1) hard-deny the terminal/shell tool (`Bash` and its
//! `BashOutput` / `KillShell` companions) so all command execution is forced
//! through the approval-gated `run_command` MCP tool; (2) best-effort reject
//! tool calls that touch paths outside the project's allowed directory.
//!
//! The path check is defense-in-depth, not a security boundary — the agent
//! still runs as the user. It catches obvious mistakes (an absolute path that
//! resolves outside the project) while staying permissive for normal work.

/// Check if a tool's input references paths outside the allowed directory.
/// Returns `Some(reason)` if the tool should be denied, `None` if allowed.
pub(super) fn check_path_violation(
    tool_name: &str,
    input: &serde_json::Value,
    allowed_dir: &str,
) -> Option<String> {
    // Terminal/shell tools are hard-disabled: force all command execution
    // through the approval-gated `run_command` MCP tool. Deny unconditionally
    // (before the allowed_dir guard) so the model always gets our guidance.
    if crate::provider::is_terminal_tool(tool_name) {
        return Some(crate::provider::TERMINAL_TOOL_DISABLED_MSG.to_string());
    }

    if allowed_dir.is_empty() {
        return None;
    }

    let allowed = match std::path::Path::new(allowed_dir).canonicalize() {
        Ok(p) => p,
        Err(_) => return None, // Can't resolve allowed dir, skip check
    };

    // Extract file paths from tool input based on tool name
    let paths_to_check: Vec<String> = match tool_name {
        "Read" | "Write" | "Edit" => {
            let mut paths = Vec::new();
            if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            paths
        }
        "Glob" | "Grep" => {
            let mut paths = Vec::new();
            if let Some(p) = input.get("path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            paths
        }
        "NotebookEdit" => {
            let mut paths = Vec::new();
            if let Some(p) = input.get("notebook_path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            paths
        }
        _ => return None, // Unknown tool, allow
    };

    for path_str in &paths_to_check {
        let path = std::path::Path::new(path_str);
        // Resolve relative paths against the allowed directory
        let resolved = if path.is_absolute() {
            match path.canonicalize() {
                Ok(p) => p,
                // File may not exist yet (Write), resolve parent
                Err(_) => {
                    if let Some(parent) = path.parent() {
                        match parent.canonicalize() {
                            Ok(p) => p.join(path.file_name().unwrap_or_default()),
                            Err(_) => path.to_path_buf(),
                        }
                    } else {
                        path.to_path_buf()
                    }
                }
            }
        } else {
            // Relative paths are relative to working dir — should be within allowed
            match std::path::Path::new(allowed_dir).join(path).canonicalize() {
                Ok(p) => p,
                Err(_) => continue, // Can't resolve, allow
            }
        };

        if !resolved.starts_with(&allowed) {
            return Some(format!(
                "Access denied: path '{}' is outside the project folder '{}'",
                path_str, allowed_dir
            ));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bash_is_denied_with_run_command_message() {
        for tool in ["Bash", "BashOutput", "KillShell"] {
            let denied = check_path_violation(tool, &json!({"command": "ls"}), ".");
            assert_eq!(
                denied.as_deref(),
                Some(crate::provider::TERMINAL_TOOL_DISABLED_MSG),
                "{tool} should be denied"
            );
        }
    }

    #[test]
    fn bash_denied_even_when_allowed_dir_empty() {
        // Deny is unconditional — must fire before the allowed_dir guard.
        let denied = check_path_violation("Bash", &json!({"command": "pwd"}), "");
        assert_eq!(
            denied.as_deref(),
            Some(crate::provider::TERMINAL_TOOL_DISABLED_MSG)
        );
    }

    #[test]
    fn non_terminal_tools_are_not_affected() {
        // MCP / file tools inside the allowed dir are allowed.
        for tool in ["read_file", "edit_file", "run_command", "NotebookEdit"] {
            let res = check_path_violation(tool, &json!({"file_path": "."}), ".");
            assert!(res.is_none(), "{tool} should not be denied, got {res:?}");
        }
    }

    #[test]
    fn path_violation_still_denies_outside_read() {
        let denied = check_path_violation("Read", &json!({"file_path": "/etc/passwd"}), ".");
        assert!(denied.is_some());
        assert!(denied.unwrap().contains("outside the project folder"));
    }

    #[test]
    fn path_violation_allows_inside_read() {
        let denied = check_path_violation("Read", &json!({"file_path": "Cargo.toml"}), ".");
        assert!(denied.is_none());
    }
}
