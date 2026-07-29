//! Discover git repositories inside a workspace folder, and each repo's
//! worktrees — the data behind `GET /api/repos` (the review wizard's
//! repo → worktree → file cascade).
//!
//! Pure directory reading, no `git` binary: a repo is a directory whose
//! `.git` is itself a directory; its linked worktrees are enumerated from
//! `.git/worktrees/<name>/gitdir`, the same metadata `git worktree list`
//! reads. Staying off the binary keeps the scan cheap, working on hosts
//! without git on PATH, and able to surface `.peckboard/worktrees/<id8>`
//! trees whose git state is broken — which is exactly when someone most
//! wants to read what's in them.
//!
//! Jail rule: a linked worktree registered in git metadata may live
//! anywhere on disk, but every path served here must stay inside the
//! workspace folder — the markdown listing/read routes are folder-jailed,
//! so an out-of-folder worktree would be a dead entry at best and a jail
//! probe at worst. Such worktrees are silently dropped.

use std::path::{Path, PathBuf};

use crate::service::fs_jail;

/// How deep the repo scan descends below the folder root. Deliberately
/// shallower than [`fs_jail::MAX_DEPTH`] — every level multiplies directory
/// reads — but deep enough for the layouts people actually keep: a folder
/// holding `clients/acme/repo` needs more than the three levels a
/// repo-sits-near-the-top rule assumes.
pub const MAX_SCAN_DEPTH: usize = 8;
/// Cap on directories visited by one scan, so a pathological tree cannot
/// blow up the request.
const MAX_SCAN_DIRS: usize = 10_000;

/// One git repository found under a workspace folder.
pub struct RepoEntry {
    /// Folder-relative path of the repo root, `""` when the folder root
    /// itself is the repo. Forward slashes.
    pub path: String,
    /// The repo's directory name (folder callers substitute the folder
    /// name when `path` is empty).
    pub name: String,
    pub worktrees: Vec<WorktreeEntry>,
}

/// One checkout of a repo: the main working tree or a linked worktree.
pub struct WorktreeEntry {
    /// Folder-relative path of the working tree. `""` = folder root.
    pub path: String,
    /// Branch name (`refs/heads/` stripped), or a short commit id when the
    /// tree is detached, or `card/<id8>` for an unregistered card tree.
    pub branch: String,
    /// `true` for the repo's main working tree.
    pub main: bool,
}

/// Scan `folder_root` (already canonicalized) for git repos and their
/// worktrees. Repos are not descended into, so a repo nested inside
/// another repo's tree (a vendored checkout) is not listed.
pub fn scan_repos(folder_root: &Path) -> Vec<RepoEntry> {
    let mut repos = Vec::new();
    let mut visited = 0usize;
    scan_dir(folder_root, folder_root, 0, &mut visited, &mut repos);
    repos.sort_by(|a, b| a.path.cmp(&b.path));
    repos
}

fn scan_dir(dir: &Path, root: &Path, depth: usize, visited: &mut usize, out: &mut Vec<RepoEntry>) {
    if depth > MAX_SCAN_DEPTH || *visited >= MAX_SCAN_DIRS {
        return;
    }
    *visited += 1;
    // `symlink_metadata`: a `.git` symlink must not count as a repo, same
    // no-follow rule as every jailed walk.
    let git = dir.join(".git");
    if git.symlink_metadata().is_ok_and(|m| m.file_type().is_dir()) {
        out.push(read_repo(dir, root));
        return; // a repo's insides are its own tree — don't scan deeper
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue; // lstat: symlinked dirs are neither followed nor listed
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if fs_jail::is_ignored_dir(&name) {
            continue;
        }
        scan_dir(&entry.path(), root, depth + 1, visited, out);
    }
}

/// Build the [`RepoEntry`] for a repo rooted at `dir`.
fn read_repo(dir: &Path, root: &Path) -> RepoEntry {
    let rel = rel_path(dir, root);
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.clone());
    let mut worktrees = vec![WorktreeEntry {
        path: rel.clone(),
        branch: head_branch(&dir.join(".git/HEAD")),
        main: true,
    }];
    linked_worktrees(dir, root, &mut worktrees);
    peckboard_worktrees(dir, root, &mut worktrees);
    // Main tree first, then the linked trees in path order.
    worktrees[1..].sort_by(|a, b| a.path.cmp(&b.path));
    RepoEntry {
        path: rel,
        name,
        worktrees,
    }
}

/// Worktrees registered in `.git/worktrees/<name>/gitdir`. Each `gitdir`
/// file holds the absolute path of the tree's `.git` file; the tree itself
/// is its parent. Kept only when it canonicalizes to inside the folder.
fn linked_worktrees(repo: &Path, root: &Path, out: &mut Vec<WorktreeEntry>) {
    let Ok(rd) = std::fs::read_dir(repo.join(".git/worktrees")) else {
        return;
    };
    for entry in rd.flatten() {
        let meta_dir = entry.path();
        let Ok(gitdir) = std::fs::read_to_string(meta_dir.join("gitdir")) else {
            continue;
        };
        let gitdir = PathBuf::from(gitdir.trim());
        // `<tree>/.git` → `<tree>`; a gitdir not shaped that way is not a
        // worktree registration we can serve.
        let Some(tree) = gitdir.parent() else {
            continue;
        };
        // Canonicalize + containment: a pruned tree fails the first check,
        // an out-of-folder tree the second. Both are dropped, not errors.
        let Ok(canon) = std::fs::canonicalize(tree) else {
            continue;
        };
        if !canon.starts_with(root) || !canon.is_dir() {
            continue;
        }
        out.push(WorktreeEntry {
            path: rel_path(&canon, root),
            branch: head_branch(&meta_dir.join("HEAD")),
            main: false,
        });
    }
}

/// The fixed `<repo>/.peckboard/worktrees/<id8>` layout, for card trees
/// whose git registration is gone (pruned metadata, broken repo state).
/// Anything already found via git metadata is skipped.
fn peckboard_worktrees(repo: &Path, root: &Path, out: &mut Vec<WorktreeEntry>) {
    let Ok(rd) = std::fs::read_dir(repo.join(".peckboard/worktrees")) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_worktree_id8(&name) || !entry.path().is_dir() {
            continue;
        }
        let Ok(canon) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if !canon.starts_with(root) {
            continue; // a symlinked card dir must not walk elsewhere
        }
        let rel = rel_path(&canon, root);
        if out.iter().any(|w| w.path == rel) {
            continue;
        }
        out.push(WorktreeEntry {
            path: rel,
            branch: format!("card/{name}"),
            main: false,
        });
    }
}

/// 8 lowercase hex chars — the only dir-name shape
/// [`crate::worker::worktree::card_id8`] mints.
pub fn is_worktree_id8(s: &str) -> bool {
    s.len() == 8
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Folder-relative path with forward slashes; `""` for the root itself.
fn rel_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// Branch behind a HEAD file: `ref: refs/heads/<branch>` → `<branch>`;
/// a detached HEAD (bare commit id) → its first 8 chars.
fn head_branch(head: &Path) -> String {
    let Ok(raw) = std::fs::read_to_string(head) else {
        return "unknown".to_string();
    };
    let raw = raw.trim();
    match raw.strip_prefix("ref: refs/heads/") {
        Some(branch) => branch.to_string(),
        None => raw.chars().take(8).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay down a fake repo: `.git` dir with a HEAD on `branch`.
    fn mk_repo(dir: &Path, branch: &str) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    }

    /// Register a linked worktree the way git does: metadata dir with a
    /// `gitdir` file naming `<tree>/.git`, plus the tree itself.
    fn mk_linked(repo: &Path, name: &str, tree: &Path, branch: &str) {
        std::fs::create_dir_all(tree).unwrap();
        let meta = repo.join(".git/worktrees").join(name);
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta.join("gitdir"), format!("{}/.git\n", tree.display())).unwrap();
        std::fs::write(meta.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    }

    #[test]
    fn finds_nested_repos_and_their_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();

        // Two repos in subfolders; plain docs dir; ignored dir with a repo.
        mk_repo(&root.join("repos/app"), "main");
        mk_repo(&root.join("tools/cli"), "trunk");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        mk_repo(&root.join("node_modules/dep"), "main");

        // A registered card worktree inside the folder…
        mk_linked(
            &root.join("repos/app"),
            "abcd1234",
            &root.join("repos/app/.peckboard/worktrees/abcd1234"),
            "card/abcd1234",
        );
        // …one outside the folder (must be dropped)…
        let outside = tempfile::tempdir().unwrap();
        mk_linked(
            &root.join("repos/app"),
            "escape",
            &std::fs::canonicalize(outside.path()).unwrap().join("wt"),
            "card/escape",
        );
        // …and an unregistered card tree (broken git state) that only the
        // fixed-layout fallback can see.
        std::fs::create_dir_all(root.join("tools/cli/.peckboard/worktrees/0a1b2c3d")).unwrap();

        let repos = scan_repos(&root);
        let paths: Vec<&str> = repos.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["repos/app", "tools/cli"], "{paths:?}");

        let app = &repos[0];
        assert_eq!(app.name, "app");
        let wts: Vec<(&str, &str, bool)> = app
            .worktrees
            .iter()
            .map(|w| (w.path.as_str(), w.branch.as_str(), w.main))
            .collect();
        assert_eq!(
            wts,
            vec![
                ("repos/app", "main", true),
                (
                    "repos/app/.peckboard/worktrees/abcd1234",
                    "card/abcd1234",
                    false
                ),
            ],
            "escaping worktree dropped"
        );

        let cli = &repos[1];
        assert_eq!(cli.worktrees[0].branch, "trunk");
        assert_eq!(
            cli.worktrees[1].path,
            "tools/cli/.peckboard/worktrees/0a1b2c3d"
        );
        assert_eq!(cli.worktrees[1].branch, "card/0a1b2c3d");
    }

    #[test]
    fn root_repo_detached_head_and_no_descent_into_repos() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "0123456789abcdef0123\n").unwrap();
        // A repo nested inside the root repo's tree stays invisible.
        mk_repo(&root.join("vendor-checkout/inner"), "main");

        let repos = scan_repos(&root);
        assert_eq!(repos.len(), 1, "root repo only");
        assert_eq!(repos[0].path, "");
        assert_eq!(repos[0].worktrees[0].path, "");
        assert!(repos[0].worktrees[0].main);
        assert_eq!(repos[0].worktrees[0].branch, "01234567");
    }
}
