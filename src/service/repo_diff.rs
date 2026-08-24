//! Working-tree diff for a repo found by [`crate::service::repo_scan`] —
//! the data behind `GET /api/repos/diff` (the folder repo browser's
//! per-repo diff viewer).
//!
//! Shells out to `git` (unlike the scan, which stays on pure directory
//! reads): a correct rename-aware, staged-plus-unstaged diff is exactly
//! what the binary is for, and this route only runs when a user opens a
//! repo's diff view. Callers are responsible for the jail — the tree path
//! handed in must already be validated against the scan results.

use std::path::Path;

use serde::Serialize;

/// Hard cap on rendered diff lines per file; the frontend viewer marks the
/// file truncated past this. Matches the ~400-line cap the session
/// `file-diff` events use.
const MAX_FILE_DIFF_LINES: usize = 400;
/// Cap on files reported per repo, so a pathological tree cannot blow up
/// the response.
const MAX_FILES: usize = 200;
/// Untracked files larger than this are listed but not rendered.
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024;

/// One changed file in a repo's working tree (staged + unstaged vs HEAD,
/// plus untracked files rendered as all-added).
#[derive(Serialize)]
pub struct FileDiff {
    pub path: String,
    /// `added` | `deleted` | `renamed` | `modified` | `untracked`
    pub status: String,
    /// Unified diff body (no `diff --git` header line), possibly empty for
    /// binary or oversized files.
    pub diff: String,
    pub added: usize,
    pub removed: usize,
    pub truncated: bool,
}

/// A repo working tree's full dirty state.
#[derive(Serialize)]
pub struct TreeDiff {
    /// Branch name, or a short commit id when detached, or `unborn` before
    /// the first commit.
    pub branch: String,
    pub files: Vec<FileDiff>,
    /// True when the file list itself was cut at [`MAX_FILES`].
    pub truncated: bool,
}

/// Diff `tree` (an absolute, canonicalized working-tree path) against HEAD.
pub async fn working_tree_diff(tree: &Path) -> Result<TreeDiff, String> {
    let branch = current_branch(tree).await;
    let has_head = git(tree, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .await
        .is_ok();

    let mut files = if has_head {
        let raw = git(
            tree,
            &[
                "diff",
                "HEAD",
                "--no-color",
                "--no-ext-diff",
                "--find-renames",
            ],
        )
        .await?;
        parse_unified(&raw)
    } else {
        Vec::new()
    };

    // Untracked files (and, pre-first-commit, everything) rendered as
    // all-added so a fresh repo still shows its contents.
    for path in untracked_files(tree, has_head).await? {
        if files.len() >= MAX_FILES {
            break;
        }
        files.push(untracked_diff(tree, &path));
    }

    let truncated = files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    Ok(TreeDiff {
        branch,
        files,
        truncated,
    })
}

/// Run one read-only git command in `tree`; stdout on success.
async fn git(tree: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(tree)
        .args(args)
        // Same env scrub as `worker::worktree::git_command`: an inherited
        // GIT_DIR would silently retarget every command.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    match cmd.output().await {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Err(e) => Err(format!("git failed to start: {e}")),
    }
}

/// Branch name for the tree's HEAD; short commit id when detached.
async fn current_branch(tree: &Path) -> String {
    match git(tree, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
        Ok(name) if name.trim() == "HEAD" => git(tree, &["rev-parse", "--short", "HEAD"])
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "detached".to_string()),
        Ok(name) => name.trim().to_string(),
        // rev-parse fails on an unborn branch; the HEAD file still names it.
        Err(_) => "unborn".to_string(),
    }
}

/// Paths git does not track yet. Pre-first-commit (`!has_head`) this is the
/// whole tree, because `diff HEAD` had nothing to report.
async fn untracked_files(tree: &Path, has_head: bool) -> Result<Vec<String>, String> {
    let raw = git(tree, &["status", "--porcelain", "--untracked-files=all"]).await?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some((code, path)) = line.split_at_checked(3) else {
            continue;
        };
        let untracked = code.starts_with("??");
        // Unborn HEAD: staged adds ("A ") are invisible to `diff HEAD` too.
        if untracked || (!has_head && code.starts_with('A')) {
            // `status --porcelain` quotes paths with special chars; keep
            // the raw form — it still names the file for the viewer.
            out.push(path.trim_matches('"').to_string());
        }
    }
    Ok(out)
}

/// Render an untracked file as an all-added diff; binary and oversized
/// files are listed with an empty body.
fn untracked_diff(tree: &Path, rel: &str) -> FileDiff {
    let abs = tree.join(rel);
    let size = abs.metadata().map(|m| m.len()).unwrap_or(0);
    let content = if size <= MAX_UNTRACKED_BYTES {
        std::fs::read(&abs).ok()
    } else {
        None
    };
    let text = content
        .filter(|b| !b.contains(&0))
        .and_then(|b| String::from_utf8(b).ok());
    let Some(text) = text else {
        return FileDiff {
            path: rel.to_string(),
            status: "untracked".to_string(),
            diff: String::new(),
            added: 0,
            removed: 0,
            truncated: size > MAX_UNTRACKED_BYTES,
        };
    };
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(MAX_FILE_DIFF_LINES);
    let mut diff = format!("@@ -0,0 +1,{} @@\n", lines.len());
    for line in &lines[..shown] {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    FileDiff {
        path: rel.to_string(),
        status: "untracked".to_string(),
        diff,
        added: lines.len(),
        removed: 0,
        truncated: shown < lines.len(),
    }
}

/// Split one `git diff` stream into per-file entries. Only the shapes git
/// itself emits are handled; anything unrecognized folds into the current
/// file's body untouched.
fn parse_unified(raw: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(FileDiff {
                path: path_from_header(rest),
                status: "modified".to_string(),
                diff: String::new(),
                added: 0,
                removed: 0,
                truncated: false,
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        // Header lines refine status/path; body lines accumulate.
        if line.starts_with("new file mode") {
            file.status = "added".to_string();
        } else if line.starts_with("deleted file mode") {
            file.status = "deleted".to_string();
        } else if let Some(renamed) = line.strip_prefix("rename to ") {
            file.status = "renamed".to_string();
            file.path = renamed.to_string();
        } else if let Some(p) = line.strip_prefix("+++ b/") {
            file.path = p.to_string();
        } else if line.starts_with("index ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity index")
            || line.starts_with("rename from ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            // header noise the viewer doesn't render
        } else {
            if line.starts_with('+') {
                file.added += 1;
            } else if line.starts_with('-') {
                file.removed += 1;
            }
            if file.truncated {
                continue;
            }
            if file.diff.lines().count() >= MAX_FILE_DIFF_LINES {
                file.truncated = true;
                continue;
            }
            file.diff.push_str(line);
            file.diff.push('\n');
        }
    }
    files
}

/// `a/<path> b/<path>` → `<path>`. Quoted forms fall back to the raw text —
/// the `+++ b/` line that follows overrides with the exact path anyway.
fn path_from_header(rest: &str) -> String {
    match rest.strip_prefix("a/").and_then(|r| r.split_once(" b/")) {
        Some((a, _)) => a.to_string(),
        None => rest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_file_diff_with_statuses() {
        let raw = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,2 +1,2 @@
-old line
+new line
 context
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 333..000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
index 000..444
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1 @@
+hi
";
        let files = parse_unified(raw);
        let brief: Vec<(&str, &str, usize, usize)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.status.as_str(), f.added, f.removed))
            .collect();
        assert_eq!(
            brief,
            vec![
                ("src/main.rs", "modified", 1, 1),
                ("gone.txt", "deleted", 0, 1),
                ("new.rs", "renamed", 0, 0),
                ("fresh.txt", "added", 1, 0),
            ]
        );
        assert!(files[0].diff.contains("@@ -1,2 +1,2 @@"));
        assert!(files[0].diff.contains("-old line"));
        assert!(!files[0].diff.contains("index 111"));
    }

    #[test]
    fn truncates_oversized_file_diffs() {
        let mut raw =
            String::from("diff --git a/big b/big\n--- a/big\n+++ b/big\n@@ -0,0 +1,999 @@\n");
        for i in 0..999 {
            raw.push_str(&format!("+line {i}\n"));
        }
        let files = parse_unified(&raw);
        assert_eq!(files.len(), 1);
        assert!(files[0].truncated);
        assert_eq!(files[0].added, 999, "counts keep going past the cap");
        assert!(files[0].diff.lines().count() <= MAX_FILE_DIFF_LINES);
    }

    #[tokio::test]
    async fn diffs_a_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env_remove("GIT_DIR")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {:?}", out);
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        std::fs::write(root.join("a.txt"), "one\nTWO\n").unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();

        let diff = working_tree_diff(&root).await.unwrap();
        assert_eq!(diff.branch, "main");
        let brief: Vec<(&str, &str)> = diff
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status.as_str()))
            .collect();
        assert_eq!(brief, vec![("a.txt", "modified"), ("new.txt", "untracked")]);
        assert!(diff.files[0].diff.contains("+TWO"));
        assert!(diff.files[1].diff.contains("+fresh"));
    }

    #[tokio::test]
    async fn unborn_repo_lists_everything_as_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q", "-b", "main"])
            .output()
            .unwrap();
        assert!(out.status.success());
        std::fs::write(root.join("seed.txt"), "hello\n").unwrap();

        let diff = working_tree_diff(&root).await.unwrap();
        assert_eq!(diff.branch, "unborn");
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "seed.txt");
        assert_eq!(diff.files[0].added, 1);
    }
}
