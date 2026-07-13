//! Git worktree isolation for card workers.
//!
//! When `project.worktree_isolation` is set and the project folder is a git
//! repository root, each card's worker runs in its own linked worktree under
//! `<folder>/.peckboard/worktrees/<id8>` on branch `card/<id8>`.
//!
//! id8 = first 8 hex chars of the card UUID (no new column — existence check
//! is a path check). All git operations run via `tokio::process::Command`.
//! Any failure falls back to the shared folder and appends a session event.

use std::path::{Path, PathBuf};

use crate::db::Db;

// ── Derivation helpers ────────────────────────────────────────────────────────

/// First 8 hex chars of a card UUID, used for worktree path + branch names.
pub fn card_id8(card_id: &str) -> String {
    card_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect()
}

/// Absolute path of the worktree for a card.
pub fn worktree_path(folder_path: &str, id8: &str) -> PathBuf {
    Path::new(folder_path)
        .join(".peckboard")
        .join("worktrees")
        .join(id8)
}

/// Git branch name for a card's worktree.
pub fn branch_name(id8: &str) -> String {
    format!("card/{id8}")
}

// ── ensure_worktree ───────────────────────────────────────────────────────────

/// Return the working directory for a card's worker.
///
/// If `isolation_on` is false, or the folder is not a git repo root (no
/// `.git`), or any git command fails, returns `folder_path` unchanged and
/// appends a `worktree-downgrade` session event on failure.
///
/// If the worktree already exists, reuses it (idempotent).
pub async fn ensure_worktree(
    folder_path: &str,
    card_id: &str,
    isolation_on: bool,
    session_id: &str,
    db: &Db,
) -> String {
    if !isolation_on {
        return folder_path.to_string();
    }

    // Only operate on git repo roots.
    if !Path::new(folder_path).join(".git").exists() {
        return folder_path.to_string();
    }

    let id8 = card_id8(card_id);
    let wt_path = worktree_path(folder_path, &id8);
    let branch = branch_name(&id8);

    // Reuse existing worktree.
    if wt_path.exists() {
        return wt_path.to_string_lossy().to_string();
    }

    // Append .peckboard/ to .git/info/exclude (idempotent, repo-local).
    append_peckboard_exclude(folder_path).await;

    // Create the worktree: git worktree add <path> -b card/<id8>
    let result = tokio::process::Command::new("git")
        .args([
            "-C",
            folder_path,
            "worktree",
            "add",
            wt_path.to_string_lossy().as_ref(),
            "-b",
            &branch,
        ])
        .output()
        .await;

    match result {
        Ok(out) if out.status.success() => wt_path.to_string_lossy().to_string(),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::warn!(
                card_id,
                "worktree add failed: {stderr}; using shared folder"
            );
            append_downgrade_event(session_id, db, &stderr).await;
            folder_path.to_string()
        }
        Err(e) => {
            tracing::warn!(card_id, "worktree add error: {e}; using shared folder");
            append_downgrade_event(session_id, db, &e.to_string()).await;
            folder_path.to_string()
        }
    }
}

/// Append `.peckboard/` to `.git/info/exclude` if not already present.
async fn append_peckboard_exclude(folder_path: &str) {
    let exclude_path = Path::new(folder_path)
        .join(".git")
        .join("info")
        .join("exclude");
    // Ensure directory exists.
    if let Some(parent) = exclude_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let existing = tokio::fs::read_to_string(&exclude_path)
        .await
        .unwrap_or_default();
    let line = ".peckboard/";
    if !existing.lines().any(|l| l.trim() == line) {
        let to_append = if existing.ends_with('\n') || existing.is_empty() {
            format!("{line}\n")
        } else {
            format!("\n{line}\n")
        };
        let _ = tokio::fs::write(&exclude_path, format!("{existing}{to_append}")).await;
    }
}

async fn append_downgrade_event(session_id: &str, db: &Db, reason: &str) {
    let _ = db
        .append_event(
            session_id,
            "worktree-downgrade",
            serde_json::json!({ "reason": reason }),
        )
        .await;
}

// ── finalize_worktree ─────────────────────────────────────────────────────────

/// Called when a card reaches a terminal step.
///
/// - Worktree missing → noop.
/// - Dirty → leave worktree + append `worktree-done {merged:false, reason:"dirty"}`.
/// - Clean, no conflict → fast-forward merge into the main folder's HEAD branch,
///   remove the worktree, delete the branch, append `{merged:true}`.
/// - Main folder dirty or merge conflict → abort, leave, append `{merged:false,
///   reason:"conflict"}`.
pub async fn finalize_worktree(folder_path: &str, card_id: &str, session_id: &str, db: &Db) {
    let id8 = card_id8(card_id);
    let wt_path = worktree_path(folder_path, &id8);
    let branch = branch_name(&id8);

    if !wt_path.exists() {
        return;
    }

    let wt_str = wt_path.to_string_lossy().to_string();

    // Check worktree dirty.
    if is_dirty(&wt_str).await {
        let _ = db
            .append_event(
                session_id,
                "worktree-done",
                serde_json::json!({ "merged": false, "reason": "dirty", "branch": branch }),
            )
            .await;
        return;
    }

    // Check main folder dirty (treat as conflict to avoid losing user work).
    if is_dirty(folder_path).await {
        let _ = db
            .append_event(
                session_id,
                "worktree-done",
                serde_json::json!({ "merged": false, "reason": "conflict", "branch": branch }),
            )
            .await;
        return;
    }

    // Try fast-forward first, fall back to non-interactive merge.
    let ff = tokio::process::Command::new("git")
        .args(["-C", folder_path, "merge", "--ff-only", &branch])
        .output()
        .await;

    let merged = match ff {
        Ok(out) if out.status.success() => true,
        _ => {
            // Try regular merge.
            let merge = tokio::process::Command::new("git")
                .args(["-C", folder_path, "merge", "--no-edit", &branch])
                .output()
                .await;
            match merge {
                Ok(out) if out.status.success() => true,
                _ => {
                    // Abort and leave the worktree for the user to resolve.
                    let _ = tokio::process::Command::new("git")
                        .args(["-C", folder_path, "merge", "--abort"])
                        .output()
                        .await;
                    let _ = db
                        .append_event(
                            session_id,
                            "worktree-done",
                            serde_json::json!({
                                "merged": false,
                                "reason": "conflict",
                                "branch": branch,
                            }),
                        )
                        .await;
                    return;
                }
            }
        }
    };

    if merged {
        // Remove worktree and delete branch.
        let _ = tokio::process::Command::new("git")
            .args(["-C", folder_path, "worktree", "remove", &wt_str])
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["-C", folder_path, "branch", "-d", &branch])
            .output()
            .await;
        let _ = db
            .append_event(
                session_id,
                "worktree-done",
                serde_json::json!({ "merged": true, "branch": branch }),
            )
            .await;
    }
}

/// Returns true if `git status --porcelain` shows any changes.
async fn is_dirty(repo_path: &str) -> bool {
    match tokio::process::Command::new("git")
        .args(["-C", repo_path, "status", "--porcelain"])
        .output()
        .await
    {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => true, // treat error as dirty to be safe
    }
}

// ── prune_worktrees ───────────────────────────────────────────────────────────

/// Janitor: run `git worktree prune` and remove clean worktrees whose card is
/// terminal or deleted.
///
/// `terminal_id8s` — id8 values whose card is done/wont_do/deleted.
pub async fn prune_worktrees(folder_path: &str, terminal_id8s: &[String]) {
    if !Path::new(folder_path).join(".git").exists() {
        return;
    }

    // git worktree prune cleans up stale administrative files.
    let _ = tokio::process::Command::new("git")
        .args(["-C", folder_path, "worktree", "prune"])
        .output()
        .await;

    for id8 in terminal_id8s {
        let wt_path = worktree_path(folder_path, id8);
        if !wt_path.exists() {
            continue;
        }
        let wt_str = wt_path.to_string_lossy().to_string();
        if !is_dirty(&wt_str).await {
            let _ = tokio::process::Command::new("git")
                .args(["-C", folder_path, "worktree", "remove", &wt_str])
                .output()
                .await;
            let branch = branch_name(id8);
            let _ = tokio::process::Command::new("git")
                .args(["-C", folder_path, "branch", "-d", &branch])
                .output()
                .await;
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_card_id8_derivation() {
        let id = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        assert_eq!(card_id8(id), "a1b2c3d4");
    }

    #[test]
    fn test_card_id8_strips_hyphens() {
        // UUIDs have hyphens which are not hex digits; they should be stripped.
        let id = "a1b2c3d4-xxxx-yyyy-zzzz-ef1234567890";
        // Only hex chars: a1b2c3d4ef1234567890 → first 8 = "a1b2c3d4"
        assert_eq!(card_id8(id), "a1b2c3d4");
    }

    #[test]
    fn test_worktree_path() {
        let path = worktree_path("/home/user/repo", "a1b2c3d4");
        assert_eq!(
            path,
            Path::new("/home/user/repo/.peckboard/worktrees/a1b2c3d4")
        );
    }

    #[test]
    fn test_branch_name() {
        assert_eq!(branch_name("a1b2c3d4"), "card/a1b2c3d4");
    }

    #[tokio::test]
    async fn test_append_peckboard_exclude_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let git_info = dir.path().join(".git").join("info");
        tokio::fs::create_dir_all(&git_info).await.unwrap();
        let exclude = git_info.join("exclude");

        let folder = dir.path().to_string_lossy().to_string();

        // First call: appends the line.
        append_peckboard_exclude(&folder).await;
        let content1 = tokio::fs::read_to_string(&exclude).await.unwrap();
        assert!(content1.contains(".peckboard/"));

        // Second call: idempotent — no duplicate.
        append_peckboard_exclude(&folder).await;
        let content2 = tokio::fs::read_to_string(&exclude).await.unwrap();
        let count = content2
            .lines()
            .filter(|l| l.trim() == ".peckboard/")
            .count();
        assert_eq!(
            count, 1,
            "expected exactly one .peckboard/ line, got {count}"
        );
    }
}
