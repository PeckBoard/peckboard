//! Host-side existence check for a stdio MCP server's `command`, plus
//! human-readable install hints for the common runners. Backs
//! `POST /api/settings/mcp-servers/check-command`, which the add/edit
//! server modal calls so a missing binary is flagged before the first
//! dispatch fails.

use std::path::{Path, PathBuf};

pub struct CommandCheck {
    pub found: bool,
    pub resolved_path: Option<PathBuf>,
}

/// Locate `command` the way `spawn` will: a bare name is searched on PATH,
/// anything containing a separator is checked as a filesystem path.
pub fn check_command(command: &str) -> CommandCheck {
    let cmd = command.trim();
    if cmd.is_empty() {
        return CommandCheck {
            found: false,
            resolved_path: None,
        };
    }
    if cmd.contains(std::path::MAIN_SEPARATOR) || cmd.contains('/') {
        let p = Path::new(cmd);
        return CommandCheck {
            found: is_executable(p),
            resolved_path: is_executable(p).then(|| p.to_path_buf()),
        };
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(cmd);
        if is_executable(&candidate) {
            return CommandCheck {
                found: true,
                resolved_path: Some(candidate),
            };
        }
    }
    CommandCheck {
        found: false,
        resolved_path: None,
    }
}

fn is_executable(p: &Path) -> bool {
    let Ok(md) = std::fs::metadata(p) else {
        return false;
    };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Human install steps for a missing command. Registry entries can carry
/// their own `install` steps which the UI shows first; these are the
/// built-in fallbacks for the runners that dominate the MCP ecosystem.
pub fn install_hints(command: &str) -> Vec<String> {
    // `npx` on PATH really means "install Node.js" — hint at the toolchain,
    // not the shim. Match on the bare program name even when a path is given.
    let base = Path::new(command.trim())
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let hints: &[&str] = match base.as_str() {
        "npx" | "node" | "npm" => &[
            "Install Node.js (bundles npx and npm): https://nodejs.org/en/download",
            "Debian/Ubuntu: sudo apt-get install -y nodejs npm",
        ],
        "uvx" | "uv" => &[
            "Install uv (bundles uvx): curl -LsSf https://astral.sh/uv/install.sh | sh",
            "Docs: https://docs.astral.sh/uv/getting-started/installation/",
        ],
        "docker" => &[
            "Install Docker Engine: https://docs.docker.com/engine/install/",
            "Debian/Ubuntu: sudo apt-get install -y docker.io",
        ],
        "python" | "python3" | "pip" | "pip3" | "pipx" => &[
            "Install Python 3: https://www.python.org/downloads/",
            "Debian/Ubuntu: sudo apt-get install -y python3 python3-pip",
        ],
        "deno" => &["Install Deno: curl -fsSL https://deno.land/install.sh | sh"],
        "bun" | "bunx" => &["Install Bun: curl -fsSL https://bun.sh/install | bash"],
        "" => &[],
        _ => {
            return vec![format!(
                "Install `{}` so it is on the Peckboard host's PATH.",
                base
            )];
        }
    };
    hints.iter().map(|s| s.to_string()).collect()
}

/// Working-directory suggestion for a one-off install session:
/// `~/peckboard-installs/<command>`. Falls back to the data-dir-relative
/// spot only when no home directory is resolvable.
pub fn suggested_install_folder(command: &str, data_dir: &Path) -> PathBuf {
    let base = Path::new(command.trim())
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = if slug.is_empty() {
        "binary".to_string()
    } else {
        slug
    };
    let root = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.to_path_buf());
    root.join("peckboard-installs").join(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_real_binary_and_misses_a_fake_one() {
        // `sh` exists on every unix host the server runs on.
        #[cfg(unix)]
        {
            let hit = check_command("sh");
            assert!(hit.found);
            assert!(hit.resolved_path.is_some());
        }
        let miss = check_command("definitely-not-a-real-binary-xyz");
        assert!(!miss.found);
        assert!(miss.resolved_path.is_none());
        assert!(!check_command("").found);
        assert!(!check_command("   ").found);
    }

    #[cfg(unix)]
    #[test]
    fn path_form_requires_the_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!check_command(file.to_str().unwrap()).found);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_command(file.to_str().unwrap()).found);
    }

    #[test]
    fn hints_cover_known_runners_and_fall_back() {
        assert!(install_hints("npx")[0].contains("Node.js"));
        assert!(install_hints("uvx")[0].contains("uv"));
        assert!(install_hints("/usr/bin/docker")[0].contains("Docker"));
        let generic = install_hints("weird-tool");
        assert_eq!(generic.len(), 1);
        assert!(generic[0].contains("weird-tool"));
    }

    #[test]
    fn suggested_folder_is_sanitized() {
        let p = suggested_install_folder("../we ird/np x", Path::new("/tmp/data"));
        let leaf = p.file_name().unwrap().to_str().unwrap();
        assert_eq!(leaf, "np-x");
        assert!(p.to_str().unwrap().contains("peckboard-installs"));
    }
}
