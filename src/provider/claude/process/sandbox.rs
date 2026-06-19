//! Best-effort sandbox: reject Claude CLI tool calls that touch paths
//! outside the project's allowed directory.
//!
//! This is a defense-in-depth layer, not a security boundary — the agent
//! still runs as the user. The intent is to catch obvious mistakes
//! (`cd /etc`, an absolute path that resolves outside the project) before
//! they execute, while staying permissive enough not to block normal work.

/// Check if a tool's input references paths outside the allowed directory.
/// Returns `Some(reason)` if the tool should be denied, `None` if allowed.
pub(super) fn check_path_violation(
    tool_name: &str,
    input: &serde_json::Value,
    allowed_dir: &str,
) -> Option<String> {
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
        "Bash" => {
            // For Bash, check the command for obvious path references
            // This is a best-effort check — can't fully parse shell commands
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                // Check for cd to outside directory
                let suspicious_patterns = ["cd /", "cd ~/", "cd ..", "rm -rf /", "cat /etc"];
                for pattern in &suspicious_patterns {
                    if cmd.contains(pattern) {
                        // Try to extract the target path from cd commands
                        if cmd.starts_with("cd ") {
                            let target = cmd
                                .trim_start_matches("cd ")
                                .split_whitespace()
                                .next()
                                .unwrap_or("");
                            if !target.is_empty() {
                                let target_path = if target.starts_with('/') {
                                    std::path::PathBuf::from(target)
                                } else {
                                    std::path::Path::new(allowed_dir).join(target)
                                };
                                if let Ok(resolved) = target_path.canonicalize() {
                                    if !resolved.starts_with(&allowed) {
                                        return Some(format!(
                                            "Access denied: path '{}' is outside the project folder '{}'",
                                            target, allowed_dir
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            return None; // Bash commands are complex; allow unless clearly violating
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
