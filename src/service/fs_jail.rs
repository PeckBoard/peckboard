//! Shared filesystem jail for folder-scoped reads, writes, and listings.
//!
//! A folder is the file-access scope of everything that runs inside it, so
//! every path that comes in over an API has to be proven to stay under that
//! folder's root before it touches disk. The rules are the same everywhere:
//!
//! - the caller's path must be **relative and descending** — `..`, an
//!   absolute path, or a Windows prefix is refused *before* any syscall;
//! - the resolved path is **canonicalized** and re-checked with
//!   `starts_with(root)`, which is what defeats a symlink whose textual
//!   segments all looked in-bounds;
//! - writes canonicalize the **parent** directory, so a symlinked
//!   intermediate can't redirect the write outside the folder;
//! - listings use `file_type()` (lstat), so symlinks are neither followed
//!   nor listed — no cycles, no escape via a linked subtree — and are
//!   bounded by depth/count caps so a huge tree can't blow up a response.
//!
//! `root` must already be canonicalized by the caller (resolve the folder
//! row's path with [`std::fs::canonicalize`]); comparing a non-canonical
//! root against a canonical target would reject legitimate paths on any
//! system where the root itself contains a symlink (e.g. macOS `/tmp`).

use std::path::{Component, Path, PathBuf};

/// How deep a listing walk descends below the folder root.
pub const MAX_DEPTH: usize = 8;
/// Cap on files returned by one listing walk.
pub const MAX_FILES: usize = 20_000;
/// Cap on bytes returned by one jailed read.
pub const MAX_READ_BYTES: usize = 1024 * 1024; // 1 MiB

/// Directories a walk never descends into — hidden dirs (`.git`, …) plus
/// common build/vendor output. Shared so a plugin's view, a worker's
/// codebase map, and a document-review file picker all see the same tree.
pub fn is_ignored_dir(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    matches!(
        name,
        "node_modules"
            | "target"
            | "dist"
            | "build"
            | "vendor"
            | "out"
            | "bin"
            | "obj"
            | "coverage"
            | "__pycache__"
            | "venv"
    )
}

/// Reject anything but plain, descending relative segments *before* the
/// filesystem is touched: no absolute path, no root/prefix, no `..`.
pub fn check_relative(rel: &Path) -> Result<(), String> {
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("path must be relative and within the project folder".to_string());
    }
    Ok(())
}

/// Resolve `rel` under the canonicalized `root` for reading. Returns the
/// canonical path of an existing regular file inside the folder.
pub fn resolve_read(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    check_relative(rel)?;
    let target = root.join(rel);
    // Canonicalize and re-check containment — defeats a symlink that points
    // outside the folder even though every textual segment looked safe.
    let canon = std::fs::canonicalize(&target).map_err(|e| format!("file not found: {e}"))?;
    if !canon.starts_with(root) {
        return Err("path escapes the project folder".to_string());
    }
    let meta = std::fs::metadata(&canon).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".to_string());
    }
    Ok(canon)
}

/// Resolve `rel` under the canonicalized `root` for writing. The parent
/// directory is canonicalized and re-checked, so a symlinked intermediate
/// cannot redirect the write outside the folder. With `create_dirs`, missing
/// in-folder parents are created first.
pub fn resolve_write(root: &Path, rel: &Path, create_dirs: bool) -> Result<PathBuf, String> {
    check_relative(rel)?;
    let Some(name) = rel.file_name() else {
        return Err("path must name a file".to_string());
    };
    let target = root.join(rel);
    let parent = match target.parent() {
        Some(p) => p.to_path_buf(),
        None => return Err("path has no parent directory".to_string()),
    };
    // Materialize the parent dir (only when asked) so we can canonicalize it.
    if !parent.exists() {
        if create_dirs {
            std::fs::create_dir_all(&parent)
                .map_err(|e| format!("could not create parent directories: {e}"))?;
        } else {
            return Err(
                "parent directory does not exist (pass create_dirs to make it)".to_string(),
            );
        }
    }
    let canon_parent =
        std::fs::canonicalize(&parent).map_err(|e| format!("parent path unavailable: {e}"))?;
    if !canon_parent.starts_with(root) {
        return Err("path escapes the project folder".to_string());
    }
    let final_path = canon_parent.join(name);
    // Refuse to clobber a non-file (e.g. a directory) at the target.
    if let Ok(meta) = std::fs::symlink_metadata(&final_path)
        && !meta.is_file()
    {
        return Err("target exists and is not a regular file".to_string());
    }
    Ok(final_path)
}

/// One file found by [`walk_files`]: path relative to the folder root, and
/// its size in bytes.
pub struct WalkedFile {
    pub path: String,
    pub size: u64,
}

/// Recursively list files under `root`, keeping the ones `keep` accepts.
/// Returns `(files, truncated)`; `truncated` is `true` when [`MAX_FILES`]
/// was hit and the listing is partial.
pub fn walk_files(root: &Path, keep: &dyn Fn(&Path) -> bool) -> (Vec<WalkedFile>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    walk_dir(root, root, 0, keep, &mut out, &mut truncated);
    (out, truncated)
}

fn walk_dir(
    dir: &Path,
    root: &Path,
    depth: usize,
    keep: &dyn Fn(&Path) -> bool,
    out: &mut Vec<WalkedFile>,
    truncated: &mut bool,
) {
    if depth > MAX_DEPTH || *truncated {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        if out.len() >= MAX_FILES {
            *truncated = true;
            return;
        }
        // `file_type()` is an lstat: a symlink is reported as a symlink, so
        // it is neither followed nor listed.
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_ignored_dir(&name) {
                continue;
            }
            walk_dir(&path, root, depth + 1, keep, out, truncated);
        } else if file_type.is_file()
            && keep(&path)
            && let Ok(rel) = path.strip_prefix(root)
        {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(WalkedFile {
                path: rel.to_string_lossy().to_string(),
                size,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three escape shapes every jailed route depends on: `..`, an
    /// absolute path, and a symlink whose textual segments look in-bounds.
    #[test]
    fn containment_refuses_traversal_absolute_and_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("ok.md"), "# ok").unwrap();

        assert!(resolve_read(&root, Path::new("ok.md")).is_ok());
        assert!(
            resolve_read(&root, Path::new("../escape.md"))
                .unwrap_err()
                .contains("within the project folder")
        );
        assert!(
            resolve_read(&root, Path::new("/etc/passwd"))
                .unwrap_err()
                .contains("within the project folder")
        );

        #[cfg(unix)]
        {
            let secret = dir.path().parent().unwrap().join("fs_jail_secret.md");
            std::fs::write(&secret, "TOP SECRET").unwrap();
            std::os::unix::fs::symlink(&secret, root.join("link.md")).unwrap();
            assert!(
                resolve_read(&root, Path::new("link.md"))
                    .unwrap_err()
                    .contains("escapes the project folder")
            );
            let _ = std::fs::remove_file(&secret);
        }
    }
}
