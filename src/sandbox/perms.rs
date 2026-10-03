//! Startup permission hardening for the data dir (idempotent).
//!
//! The data dir used to be `0775` with a world-readable DB (`0644`) and
//! `0664` MCP token files. Other local users must not read any of it:
//! the dir becomes `0700` and every secret/DB file `0600`. The process
//! umask is deliberately left alone — agent children inherit it, and a
//! `077` umask would make every file they create in a project private.

use std::path::Path;

/// Files in the data dir that hold secrets or the DB.
const SECRET_FILES: &[&str] = &[
    "jwt_secret",
    "mfa_vault_key",
    "ssh_vault_key",
    "remote_access_key",
    "vapid_keys.json",
    "peckboard.db",
    "peckboard.db-wal",
    "peckboard.db-shm",
];

/// Directories in the data dir that hold credentials or key material.
const PRIVATE_DIRS: &[&str] = &["certs", "worker-mcp", "backups"];

/// Tighten the data dir: `0700` on the dir and the private subdirs (plus
/// every `*-accounts` dir), `0600` on secret files and anything in `certs/`,
/// and on any `jwt_secret*` leftover (e.g. a pre-rotation copy). Best effort:
/// a failure is logged, never fatal.
pub fn harden_data_dir(data_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &Path, mode: u32| {
            if let Ok(md) = std::fs::symlink_metadata(p)
                && !md.file_type().is_symlink()
                && md.permissions().mode() & 0o777 != mode
                && let Err(e) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
            {
                tracing::warn!("could not chmod {} to {mode:o}: {e}", p.display());
            }
        };
        set(data_dir, 0o700);
        for f in SECRET_FILES {
            set(&data_dir.join(f), 0o600);
        }
        for d in PRIVATE_DIRS {
            set(&data_dir.join(d), 0o700);
        }
        if let Ok(rd) = std::fs::read_dir(data_dir.join("certs")) {
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_file()) {
                    set(&e.path(), 0o600);
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(data_dir) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                let Ok(t) = e.file_type() else { continue };
                if t.is_dir() && name.ends_with("-accounts") {
                    set(&e.path(), 0o700);
                } else if t.is_file() && name.starts_with("jwt_secret") {
                    set(&e.path(), 0o600);
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = data_dir;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn tightens_the_data_dir_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o775)).unwrap();
        for f in [
            "peckboard.db",
            "jwt_secret",
            "jwt_secret.pre-rotation-2026-10-01",
        ] {
            std::fs::write(d.join(f), "x").unwrap();
            std::fs::set_permissions(d.join(f), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        std::fs::create_dir_all(d.join("claude-accounts/a")).unwrap();
        std::fs::create_dir_all(d.join("projects")).unwrap();
        std::fs::set_permissions(d.join("projects"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        harden_data_dir(d);
        harden_data_dir(d);

        assert_eq!(mode(d), 0o700);
        assert_eq!(mode(&d.join("peckboard.db")), 0o600);
        assert_eq!(mode(&d.join("jwt_secret")), 0o600);
        assert_eq!(mode(&d.join("jwt_secret.pre-rotation-2026-10-01")), 0o600);
        assert_eq!(mode(&d.join("claude-accounts")), 0o700);
        // Unrelated dirs keep their mode (the 0700 parent already shields them).
        assert_eq!(mode(&d.join("projects")), 0o755);
    }
}
