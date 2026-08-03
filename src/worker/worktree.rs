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
/// A `git` invocation with any inherited repo-pointing environment stripped.
///
/// Every call here names its repo with `-C`, but a stray `GIT_DIR` /
/// `GIT_WORK_TREE` / `GIT_INDEX_FILE` in the environment (git sets these for
/// hook subprocesses) silently retargets the command at a different repo.
fn git_command(args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

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
    // Create the worktree: git worktree add <path> -b card/<id8>
    let result = git_command(&[
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

// ── merge / finalize ──────────────────────────────────────────────────────────

/// Outcome of one merge-and-cleanup attempt on a card's worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// True when the card's branch is merged into the main folder's HEAD.
    pub merged: bool,
    /// `dirty` | `conflict` | `cleanup_failed` | `worktree_missing`; `None` when fully done.
    pub reason: Option<String>,
    /// Git stderr / explanation behind `reason`.
    pub detail: Option<String>,
}

impl MergeOutcome {
    fn done() -> Self {
        Self {
            merged: true,
            reason: None,
            detail: None,
        }
    }

    fn unmerged(reason: &str, detail: impl Into<String>) -> Self {
        Self {
            merged: false,
            reason: Some(reason.to_string()),
            detail: Some(detail.into()),
        }
    }

    /// Merged, but removing the worktree or deleting the branch failed —
    /// the stale worktree needs a retry, so it stays flagged on the card.
    fn cleanup_failed(detail: impl Into<String>) -> Self {
        Self {
            merged: true,
            reason: Some("cleanup_failed".into()),
            detail: Some(detail.into()),
        }
    }

    /// True when nothing is left to do for this card's worktree.
    pub fn is_clean(&self) -> bool {
        self.reason.is_none()
    }
}

/// Called when a card reaches a terminal step. Thin wrapper over
/// [`merge_worktree`] for callers that always have a live session.
pub async fn finalize_worktree(folder_path: &str, card_id: &str, session_id: &str, db: &Db) {
    merge_worktree(folder_path, card_id, Some(session_id), db).await;
}

/// Merge a card's worktree back into the main folder, then clean it up.
///
/// - Worktree missing, nothing pending on the card → noop (`merged: true`), no event.
/// - Worktree missing but the card still carries an unmerged flag and its
///   branch has commits not on HEAD (deleted out from under an unresolved
///   conflict, e.g. by the watchdog) → `reason: "worktree_missing"`, so a
///   retry doesn't falsely report success.
/// - Worktree dirty → leave it, `reason: "dirty"`.
/// - Main folder dirty or merge conflict → abort the merge, leave the
///   worktree, `reason: "conflict"` carrying the git stderr.
/// - Merged but `worktree remove` / `branch -d` failed → `reason:
///   "cleanup_failed"` carrying the git stderr, so a stale worktree is
///   visible instead of rotting silently.
///
/// Every outcome is persisted on the card (`worktree_unmerged_reason` /
/// `worktree_unmerged_detail`) so it survives a restart, and — when
/// `session_id` is set — appended to the transcript as a `worktree-done`
/// event carrying `cardId` / `projectId` so the UI can offer a retry.
pub async fn merge_worktree(
    folder_path: &str,
    card_id: &str,
    session_id: Option<&str>,
    db: &Db,
) -> MergeOutcome {
    let id8 = card_id8(card_id);
    let wt_path = worktree_path(folder_path, &id8);
    let branch = branch_name(&id8);
    let card = db.get_card(card_id).await.ok().flatten();

    // No worktree: nothing to merge in the common case (this runs for every
    // card in non-isolated projects too). But if the card still carries an
    // unmerged flag and its branch has commits not on HEAD, the worktree was
    if !wt_path.exists() {
        let still_unmerged = card
            .as_ref()
            .is_some_and(|c| c.worktree_unmerged_reason.is_some())
            && branch_has_unmerged_commits(folder_path, &branch).await;
        let outcome = if still_unmerged {
            MergeOutcome::unmerged(
                "worktree_missing",
                "the worktree was removed before its commits were merged; the branch \
                 still exists but was not merged into the main folder",
            )
        } else {
            MergeOutcome::done()
        };
        persist_outcome(db, card_id, card.as_ref(), &outcome).await;
        emit_done_event(db, session_id, card.as_ref(), card_id, &branch, &outcome).await;
        return outcome;
    }

    let wt_str = wt_path.to_string_lossy().to_string();

    let outcome = if let Some(status) = dirty_status(&wt_str).await {
        MergeOutcome::unmerged("dirty", status)
    } else if let Some(status) = dirty_status(folder_path).await {
        // Main folder dirty — treat as a conflict to avoid losing user work.
        MergeOutcome::unmerged(
            "conflict",
            format!("the project folder has uncommitted changes:\n{status}"),
        )
    } else {
        match run_merge(folder_path, &branch).await {
            Err(detail) => MergeOutcome::unmerged("conflict", detail),
            Ok(()) => match cleanup_worktree(folder_path, &wt_str, &branch).await {
                Ok(()) => MergeOutcome::done(),
                Err(detail) => {
                    // Never swallowed: logged here and carried into the
                    // `worktree-done` event + the card's persisted state.
                    tracing::warn!(card_id, "worktree cleanup failed: {detail}");
                    MergeOutcome::cleanup_failed(detail)
                }
            },
        }
    };

    persist_outcome(db, card_id, card.as_ref(), &outcome).await;
    emit_done_event(db, session_id, card.as_ref(), card_id, &branch, &outcome).await;
    outcome
}

/// Fast-forward if possible, otherwise a non-interactive merge; a failed
/// merge is aborted so the main folder is left untouched. `Err` carries the
/// git stderr.
async fn run_merge(folder_path: &str, branch: &str) -> Result<(), String> {
    if let Ok(out) = git_command(&["-C", folder_path, "merge", "--ff-only", branch])
        .output()
        .await
        && out.status.success()
    {
        return Ok(());
    }

    let merge = git_command(&["-C", folder_path, "merge", "--no-edit", branch])
        .output()
        .await;
    match merge {
        Ok(out) if out.status.success() => Ok(()),
        other => {
            let detail = match other {
                Ok(out) => git_error(&out),
                Err(e) => e.to_string(),
            };
            // Abort and leave the worktree for the user to resolve.
            let _ = git_command(&["-C", folder_path, "merge", "--abort"])
                .output()
                .await;
            Err(detail)
        }
    }
}

/// Remove the worktree and delete its branch. `Err` carries the git stderr
/// of whichever step failed — callers must surface it, not swallow it.
async fn cleanup_worktree(folder_path: &str, wt_str: &str, branch: &str) -> Result<(), String> {
    run_git(&["-C", folder_path, "worktree", "remove", wt_str]).await?;
    run_git(&["-C", folder_path, "branch", "-d", branch]).await
}

/// Run a git command, mapping a non-zero exit or spawn failure to its stderr.
async fn run_git(args: &[&str]) -> Result<(), String> {
    match git_command(args).output().await {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(git_error(&out)),
        Err(e) => Err(e.to_string()),
    }
}

/// stderr of a failed git invocation, falling back to stdout then the code.
fn git_error(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    format!("git exited with {}", out.status)
}

/// Write the outcome onto the card, skipping the write when nothing changed
/// (the common "no worktree, nothing pending" path).
async fn persist_outcome(
    db: &Db,
    card_id: &str,
    card: Option<&crate::db::models::Card>,
    outcome: &MergeOutcome,
) {
    if let Some(card) = card
        && card.worktree_unmerged_reason.as_deref() == outcome.reason.as_deref()
        && card.worktree_unmerged_detail.as_deref() == outcome.detail.as_deref()
    {
        return;
    }
    if let Err(e) = db
        .update_card(
            card_id,
            crate::db::models::UpdateCard {
                worktree_unmerged_reason: Some(outcome.reason.clone()),
                worktree_unmerged_detail: Some(outcome.detail.clone()),
                ..Default::default()
            },
        )
        .await
    {
        tracing::warn!(card_id, "failed to persist worktree merge state: {e}");
    }
}

async fn emit_done_event(
    db: &Db,
    session_id: Option<&str>,
    card: Option<&crate::db::models::Card>,
    card_id: &str,
    branch: &str,
    outcome: &MergeOutcome,
) {
    let Some(session_id) = session_id else {
        return;
    };
    let mut data = serde_json::json!({
        "merged": outcome.merged,
        "branch": branch,
        "cardId": card_id,
    });
    if let Some(project_id) = card.map(|c| c.project_id.as_str()) {
        data["projectId"] = serde_json::json!(project_id);
    }
    if let Some(reason) = &outcome.reason {
        data["reason"] = serde_json::json!(reason);
    }
    if let Some(detail) = &outcome.detail {
        data["detail"] = serde_json::json!(detail);
    }
    let _ = db.append_event(session_id, "worktree-done", data).await;
}

/// `git status --porcelain` output when the repo has changes, else `None`.
/// A git failure counts as dirty (with the error as the status) to be safe.
async fn dirty_status(repo_path: &str) -> Option<String> {
    match git_command(&["-C", repo_path, "status", "--porcelain"])
        .output()
        .await
    {
        Ok(out) if out.stdout.is_empty() => None,
        Ok(out) => Some(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        Err(e) => Some(format!("git status failed: {e}")),
    }
}

/// Returns true if `git status --porcelain` shows any changes.
async fn is_dirty(repo_path: &str) -> bool {
    dirty_status(repo_path).await.is_some()
}

/// True if `branch` exists and is not fully merged into `folder_path`'s HEAD
/// (i.e. it carries commits HEAD doesn't have). False if the branch is gone
/// (already cleaned up) or fully merged.
async fn branch_has_unmerged_commits(folder_path: &str, branch: &str) -> bool {
    let exists = git_command(&["-C", folder_path, "rev-parse", "--verify", branch])
        .output()
        .await
        .is_ok_and(|out| out.status.success());
    if !exists {
        return false;
    }
    let is_ancestor = git_command(&[
        "-C",
        folder_path,
        "merge-base",
        "--is-ancestor",
        branch,
        "HEAD",
    ])
    .output()
    .await
    .is_ok_and(|out| out.status.success());
    !is_ancestor
}

// ── prune_worktrees ───────────────────────────────────────────────────────────

/// Janitor: run `git worktree prune` and remove clean worktrees whose card is
/// terminal or deleted.
///
/// `terminal_id8s` — id8 values whose card is done/wont_do/deleted.
/// `unmerged_id8s` — id8 values whose card still carries an unresolved
/// `worktree_unmerged_reason` (dirty/conflict/cleanup_failed); their worktree
/// is left for the user to resolve even if it's terminal, so a retry-merge
/// doesn't find the worktree gone and falsely report success.
pub async fn prune_worktrees(
    folder_path: &str,
    terminal_id8s: &[String],
    unmerged_id8s: &[String],
) {
    if !Path::new(folder_path).join(".git").exists() {
        return;
    }

    // git worktree prune cleans up stale administrative files.
    if let Err(e) = run_git(&["-C", folder_path, "worktree", "prune"]).await {
        tracing::warn!(folder_path, "git worktree prune failed: {e}");
    }

    for id8 in terminal_id8s {
        if unmerged_id8s.contains(id8) {
            continue;
        }
        let wt_path = worktree_path(folder_path, id8);
        if !wt_path.exists() {
            continue;
        }
        let wt_str = wt_path.to_string_lossy().to_string();
        if !is_dirty(&wt_str).await {
            let branch = branch_name(id8);
            if let Err(e) = cleanup_worktree(folder_path, &wt_str, &branch).await {
                tracing::warn!(folder_path, id8, "worktree cleanup failed: {e}");
            }
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

    // ── merge_worktree ───────────────────────────────────────────────

    /// Run a git command in `dir`, panicking with its stderr on failure.
    /// Same env scrubbing as [`git_command`] — `cargo test` under a git hook
    /// inherits a `GIT_DIR` that would point these at the wrong repo.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A git repo with one commit, plus a DB holding folder/project/card
    /// rows pointing at it. Returns (repo dir, db, card id).
    async fn repo_with_card() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        git(&path, &["init", "-b", "main"]);
        git(&path, &["config", "user.email", "t@example.com"]);
        git(&path, &["config", "user.name", "Test"]);
        std::fs::write(path.join("README.md"), "hello\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", "init"]);

        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: path.to_string_lossy().to_string(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(crate::db::models::NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: String::new(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: true,
        })
        .await
        .unwrap();
        let card_id = "a1b2c3d4-0000-0000-0000-000000000000".to_string();
        db.create_card(crate::db::models::NewCard {
            id: card_id.clone(),
            project_id: "p1".into(),
            title: "Card".into(),
            description: String::new(),
            step: "backlog".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts,
            system_prompt_name: None,
        })
        .await
        .unwrap();
        (dir, db, card_id)
    }

    /// Committed work merges back, the worktree + branch are gone, and the
    /// card carries no unmerged flag.
    #[tokio::test]
    async fn test_merge_worktree_clean_merges_and_clears_card() {
        let (dir, db, card_id) = repo_with_card().await;
        let folder = dir.path().to_string_lossy().to_string();

        let wt = ensure_worktree(&folder, &card_id, true, "s1", &db).await;
        assert_ne!(wt, folder, "expected an isolated worktree");
        std::fs::write(std::path::Path::new(&wt).join("new.txt"), "work\n").unwrap();
        let wt_path = std::path::PathBuf::from(&wt);
        git(&wt_path, &["add", "."]);
        git(&wt_path, &["commit", "-m", "card work"]);

        let outcome = merge_worktree(&folder, &card_id, None, &db).await;
        assert!(outcome.is_clean(), "{outcome:?}");
        assert!(dir.path().join("new.txt").exists(), "merge did not land");
        assert!(!wt_path.exists(), "worktree not removed");

        let card = db.get_card(&card_id).await.unwrap().unwrap();
        assert_eq!(card.worktree_unmerged_reason, None);
    }

    /// Uncommitted work leaves the worktree in place and the reason is
    /// persisted on the card so it survives a restart.
    #[tokio::test]
    async fn test_merge_worktree_dirty_persists_reason_on_card() {
        let (dir, db, card_id) = repo_with_card().await;
        let folder = dir.path().to_string_lossy().to_string();

        let wt = ensure_worktree(&folder, &card_id, true, "s1", &db).await;
        std::fs::write(std::path::Path::new(&wt).join("scratch.txt"), "wip\n").unwrap();

        let outcome = merge_worktree(&folder, &card_id, None, &db).await;
        assert_eq!(outcome.reason.as_deref(), Some("dirty"));
        assert!(outcome.detail.unwrap().contains("scratch.txt"));
        assert!(std::path::Path::new(&wt).exists(), "worktree was removed");

        let card = db.get_card(&card_id).await.unwrap().unwrap();
        assert_eq!(card.worktree_unmerged_reason.as_deref(), Some("dirty"));

        // Resolving it (commit) and retrying merges and clears the flag.
        let wt_path = std::path::PathBuf::from(&wt);
        git(&wt_path, &["add", "."]);
        git(&wt_path, &["commit", "-m", "resolved"]);
        let retry = merge_worktree(&folder, &card_id, None, &db).await;
        assert!(retry.is_clean(), "{retry:?}");
        let card = db.get_card(&card_id).await.unwrap().unwrap();
        assert_eq!(card.worktree_unmerged_reason, None);
    }

    /// Prune skips a worktree whose card is flagged unmerged even though the
    /// card is terminal, so a conflict left for the user isn't deleted out
    /// from under them.
    #[tokio::test]
    async fn test_prune_worktrees_skips_unmerged_flagged_card() {
        let (dir, db, card_id) = repo_with_card().await;
        let folder = dir.path().to_string_lossy().to_string();
        let id8 = card_id8(&card_id);

        let wt = ensure_worktree(&folder, &card_id, true, "s1", &db).await;
        std::fs::write(std::path::Path::new(&wt).join("new.txt"), "work\n").unwrap();
        let wt_path = std::path::PathBuf::from(&wt);
        git(&wt_path, &["add", "."]);
        git(&wt_path, &["commit", "-m", "card work"]);

        db.update_card(
            &card_id,
            crate::db::models::UpdateCard {
                worktree_unmerged_reason: Some(Some("conflict".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        prune_worktrees(&folder, &[id8.clone()], &[id8.clone()]).await;
        assert!(wt_path.exists(), "flagged worktree was pruned");

        prune_worktrees(&folder, &[id8], &[]).await;
        assert!(
            !wt_path.exists(),
            "worktree should be prunable once unflagged"
        );
    }

    /// A worktree deleted out from under an unresolved conflict (e.g. by the
    /// watchdog before this fix) is reported as still unmerged, not silently
    /// marked done, so retry-merge doesn't lie about the state.
    #[tokio::test]
    async fn test_merge_worktree_reports_missing_when_flagged_and_unmerged() {
        let (dir, db, card_id) = repo_with_card().await;
        let folder = dir.path().to_string_lossy().to_string();

        let wt = ensure_worktree(&folder, &card_id, true, "s1", &db).await;
        std::fs::write(std::path::Path::new(&wt).join("new.txt"), "work\n").unwrap();
        let wt_path = std::path::PathBuf::from(&wt);
        git(&wt_path, &["add", "."]);
        git(&wt_path, &["commit", "-m", "card work"]);

        db.update_card(
            &card_id,
            crate::db::models::UpdateCard {
                worktree_unmerged_reason: Some(Some("conflict".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Simulate the worktree being deleted out from under the flag.
        std::fs::remove_dir_all(&wt_path).unwrap();

        let outcome = merge_worktree(&folder, &card_id, None, &db).await;
        assert!(!outcome.merged, "{outcome:?}");
        assert_eq!(outcome.reason.as_deref(), Some("worktree_missing"));

        let card = db.get_card(&card_id).await.unwrap().unwrap();
        assert_eq!(
            card.worktree_unmerged_reason.as_deref(),
            Some("worktree_missing")
        );
    }
}
