//! The GitHub half of PR-linked reviews: read a pull request's review
//! comments on one file, and reply to one of those threads.
//!
//! Deliberately small. This is not a GitHub client — it is the three calls
//! a linked review makes, over `reqwest`, against a configurable API base so
//! the tests can point it at a local server instead of the internet.
//!
//! Credentials come from the `github-bridge` plugin's settings if it is
//! installed (that is where a PeckBoard already keeps its GitHub token), and
//! otherwise from `GITHUB_TOKEN` in the environment. Nothing here prompts,
//! and nothing here runs unless a review is actually linked to a PR.

use crate::db::Db;

/// The plugin whose stored token we borrow, and its settings keys.
const BRIDGE_PLUGIN: &str = "github-bridge";
const TOKEN_KEY: &str = "github_token";
const API_BASE_KEY: &str = "github_api_base";

pub const DEFAULT_API_BASE: &str = "https://api.github.com";
/// GitHub requires a User-Agent on API requests.
const UA: &str = concat!("peckboard-doc-review/", env!("PECKBOARD_VERSION"));
/// Comment bodies are small; a PR with a runaway thread should not be able
/// to hand us an unbounded response.
const MAX_COMMENTS: usize = 500;

/// Where to talk to GitHub, and as whom.
#[derive(Debug, Clone)]
pub struct GithubConfig {
    pub token: String,
    pub api_base: String,
}

/// The configured credentials, or `None` when this PeckBoard has none — in
/// which case the PR features stay switched off rather than half-working.
///
/// The environment variable is `PECKBOARD_GITHUB_TOKEN`, not the bare
/// `GITHUB_TOKEN`: a token that happens to be exported in the shell that
/// started the server should not quietly switch a feature on.
pub async fn config(db: &Db) -> Option<GithubConfig> {
    let settings = db.list_plugin_settings(BRIDGE_PLUGIN).await.ok();
    let stored = |key: &str| -> Option<String> {
        settings
            .as_ref()?
            .get(key)?
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let token = stored(TOKEN_KEY)
        .or_else(|| std::env::var("PECKBOARD_GITHUB_TOKEN").ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())?;
    let api_base = stored(API_BASE_KEY)
        .or_else(|| std::env::var("PECKBOARD_GITHUB_API_BASE").ok())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
    Some(GithubConfig {
        token,
        api_base: api_base.trim_end_matches('/').to_string(),
    })
}

/// One review comment on a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrComment {
    /// GitHub's comment id — the annotation's `external_id`, and what a
    /// reply is addressed to.
    pub id: i64,
    /// Repo-relative path of the file the comment is on.
    pub path: String,
    /// Line in the file's current state. GitHub nulls this once the comment
    /// goes outdated, which is why `original_line` is kept as the fallback.
    pub line: Option<i32>,
    pub original_line: Option<i32>,
    pub body: String,
    /// Login of whoever wrote it, for the annotation's byline.
    pub author: String,
}

impl PrComment {
    /// The line to anchor at: where the comment lives now, else where it was
    /// written. `None` when GitHub reports neither — a file-level comment,
    /// which has no line to point at.
    pub fn anchor_line(&self) -> Option<i32> {
        self.line.or(self.original_line).filter(|n| *n >= 1)
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn parse_comment(v: &serde_json::Value) -> Option<PrComment> {
    Some(PrComment {
        id: v.get("id")?.as_i64()?,
        path: v.get("path")?.as_str()?.to_string(),
        line: v.get("line").and_then(|l| l.as_i64()).map(|l| l as i32),
        original_line: v
            .get("original_line")
            .and_then(|l| l.as_i64())
            .map(|l| l as i32),
        body: v
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or_default()
            .to_string(),
        author: v
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|l| l.as_str())
            .unwrap_or("someone")
            .to_string(),
    })
}

/// Every review comment on the pull request, oldest first. Paginated at
/// GitHub's maximum page size and capped, so one enormous thread cannot
/// stall a sync.
pub async fn list_review_comments(
    cfg: &GithubConfig,
    owner: &str,
    repo: &str,
    number: i32,
) -> anyhow::Result<Vec<PrComment>> {
    let mut out = Vec::new();
    let mut page = 1;
    loop {
        let url = format!(
            "{}/repos/{owner}/{repo}/pulls/{number}/comments?per_page=100&page={page}",
            cfg.api_base
        );
        let res = client()
            .get(&url)
            .header("User-Agent", UA)
            .header("Accept", "application/vnd.github+json")
            .bearer_auth(&cfg.token)
            .send()
            .await?;
        if !res.status().is_success() {
            anyhow::bail!("GitHub returned {} for {url}", res.status());
        }
        let body: serde_json::Value = res.json().await?;
        let Some(items) = body.as_array() else {
            anyhow::bail!("GitHub returned a non-list of review comments");
        };
        let batch = items.len();
        out.extend(items.iter().filter_map(parse_comment));
        if batch < 100 || out.len() >= MAX_COMMENTS {
            out.truncate(MAX_COMMENTS);
            return Ok(out);
        }
        page += 1;
    }
}

/// Reply on an existing review-comment thread.
pub async fn reply_to_comment(
    cfg: &GithubConfig,
    owner: &str,
    repo: &str,
    number: i32,
    comment_id: i64,
    body: &str,
) -> anyhow::Result<()> {
    let url = format!(
        "{}/repos/{owner}/{repo}/pulls/{number}/comments/{comment_id}/replies",
        cfg.api_base
    );
    let res = client()
        .post(&url)
        .header("User-Agent", UA)
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(&cfg.token)
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("GitHub returned {} for {url}", res.status());
    }
    Ok(())
}

/// The number of the open pull request whose head is `branch`, if there is
/// one. Used to offer a link rather than make the user go find the number.
pub async fn find_open_pr_for_branch(
    cfg: &GithubConfig,
    owner: &str,
    repo: &str,
    branch: &str,
) -> anyhow::Result<Option<i32>> {
    let url = format!(
        "{}/repos/{owner}/{repo}/pulls?state=open&head={owner}:{branch}",
        cfg.api_base
    );
    let res = client()
        .get(&url)
        .header("User-Agent", UA)
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(&cfg.token)
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("GitHub returned {} for {url}", res.status());
    }
    let body: serde_json::Value = res.json().await?;
    Ok(body
        .as_array()
        .and_then(|prs| prs.first())
        .and_then(|pr| pr.get("number"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32))
}

/// `owner/repo` out of a git remote URL, for the two forms git writes:
/// `git@github.com:owner/repo.git` and `https://github.com/owner/repo`.
/// Anything that isn't recognisably a GitHub remote yields `None` — a repo
/// hosted elsewhere simply has no PR to offer.
pub fn parse_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let rest = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://github.com/") {
        rest
    } else {
        return None;
    };
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// The `origin` remote URL recorded in a checkout's `.git/config`, read
/// without shelling out to git (the same no-git-binary rule the repo scan
/// follows). Also handles a linked worktree, whose `.git` is a file naming
/// the real git dir.
pub fn origin_remote(checkout: &std::path::Path) -> Option<String> {
    let git = checkout.join(".git");
    let config = if git.is_dir() {
        git.join("config")
    } else {
        // A worktree's `.git` file: `gitdir: /abs/path/.git/worktrees/<name>`.
        // The config lives on the main repo, two levels up from there.
        let contents = std::fs::read_to_string(&git).ok()?;
        let dir = contents.trim().strip_prefix("gitdir:")?.trim();
        std::path::Path::new(dir).parent()?.parent()?.join("config")
    };
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }
        if in_origin && let Some(rest) = line.strip_prefix("url") {
            return rest
                .trim_start()
                .strip_prefix('=')
                .map(|u| u.trim().to_string());
        }
    }
    None
}

/// The checkout the file at `root/rel` sits in, plus that file's path
/// inside it. Walks up from the file to the nearest `.git` — a directory in
/// a plain clone, a file in a linked worktree — and stops at `root`, which
/// is the folder jail's boundary.
///
/// The path it returns is what GitHub reports as a review comment's `path`,
/// which is the whole point: the two have to be comparable.
pub fn checkout_of(root: &std::path::Path, rel: &str) -> Option<(std::path::PathBuf, String)> {
    let file = root.join(rel);
    let mut dir = file.parent()?.to_path_buf();
    loop {
        if dir.join(".git").symlink_metadata().is_ok() {
            let inside = file.strip_prefix(&dir).ok()?;
            return Some((dir, inside.to_string_lossy().replace('\\', "/")));
        }
        if dir == root {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
    }
}

/// The branch a checkout is on, `refs/heads/` stripped. `None` on a detached
/// head — there is no branch for a pull request to have been opened from.
pub fn head_branch(checkout: &std::path::Path) -> Option<String> {
    let git = checkout.join(".git");
    let head = if git.is_dir() {
        git.join("HEAD")
    } else {
        let contents = std::fs::read_to_string(&git).ok()?;
        let dir = contents.trim().strip_prefix("gitdir:")?.trim();
        std::path::Path::new(dir).join("HEAD")
    };
    std::fs::read_to_string(head)
        .ok()?
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

/// Answer on GitHub for every resolved annotation that came in from a pull
/// request: the reviewer asked there, so the reply belongs there. Whoever
/// resolved it — a review pass or the person reading the rail — the thread
/// hears the same thing.
///
/// Spawned rather than awaited. A slow GitHub is not a reason to hold up an
/// agent's turn or a click, and a reply that fails must not fail the
/// resolution that prompted it: the resolution is the record, the reply is a
/// courtesy. A review with no PR link never reaches the network at all.
pub fn answer_resolutions(
    db: Db,
    review_id: String,
    resolutions: Vec<(String, String, Option<String>)>,
) {
    tokio::spawn(async move {
        let Ok(Some(link)) = db.get_doc_review_pr_link(&review_id).await else {
            return;
        };
        let Some(cfg) = config(&db).await else {
            return;
        };
        let Ok(comments) = db.list_doc_review_comments(&review_id, false).await else {
            return;
        };
        for (comment_id, action, note) in resolutions {
            let Some(comment) = comments.iter().find(|c| c.id == comment_id) else {
                continue;
            };
            if comment.external_kind.as_deref() != Some("github_pr") {
                continue;
            }
            let Some(thread) = comment
                .external_id
                .as_deref()
                .and_then(|id| id.parse::<i64>().ok())
            else {
                continue;
            };
            let body = match note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                Some(note) => format!("**PeckBoard review — {action}.** {note}"),
                None => format!("**PeckBoard review — {action}.**"),
            };
            if let Err(e) =
                reply_to_comment(&cfg, &link.owner, &link.repo, link.number, thread, &body).await
            {
                tracing::warn!("doc review {review_id}: GitHub reply failed: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_parse_in_the_shapes_git_writes_them() {
        let expected = Some(("PeckBoard".to_string(), "peckboard".to_string()));
        assert_eq!(
            parse_remote("git@github.com:PeckBoard/peckboard.git"),
            expected
        );
        assert_eq!(
            parse_remote("https://github.com/PeckBoard/peckboard"),
            expected
        );
        assert_eq!(
            parse_remote("https://github.com/PeckBoard/peckboard.git"),
            expected
        );
        assert_eq!(
            parse_remote("ssh://git@github.com/PeckBoard/peckboard.git"),
            expected
        );
    }

    #[test]
    fn a_remote_that_is_not_github_offers_nothing() {
        assert_eq!(parse_remote("git@gitlab.com:acme/app.git"), None);
        assert_eq!(parse_remote("https://example.com/acme/app"), None);
        assert_eq!(parse_remote(""), None);
        // A path with an extra segment is not `owner/repo`.
        assert_eq!(parse_remote("https://github.com/acme/app/tree/main"), None);
    }

    #[test]
    fn a_comment_anchors_where_it_lives_now_then_where_it_was_written() {
        let base = PrComment {
            id: 1,
            path: "docs/spec.md".into(),
            line: Some(12),
            original_line: Some(9),
            body: "tighten this".into(),
            author: "octocat".into(),
        };
        assert_eq!(base.anchor_line(), Some(12));
        // Outdated: GitHub drops `line` and only `original_line` survives.
        let outdated = PrComment {
            line: None,
            ..base.clone()
        };

        assert_eq!(outdated.anchor_line(), Some(9));
        // A file-level comment has neither, and gets no anchor at all.
        let file_level = PrComment {
            line: None,
            original_line: None,
            ..base
        };
        assert_eq!(file_level.anchor_line(), None);
    }

    #[test]
    fn a_comment_payload_parses_the_fields_the_import_needs() {
        let raw = serde_json::json!({
            "id": 4242,
            "path": "docs/spec.md",
            "line": 7,
            "original_line": 5,
            "body": "this reads two ways",
            "user": { "login": "octocat" },
        });
        let parsed = parse_comment(&raw).expect("a well-formed comment parses");
        assert_eq!(parsed.id, 4242);
        assert_eq!(parsed.path, "docs/spec.md");
        assert_eq!(parsed.anchor_line(), Some(7));
        assert_eq!(parsed.author, "octocat");

        // A payload with no id or no path is not a review comment on a file.
        assert!(parse_comment(&serde_json::json!({ "path": "a.md" })).is_none());
        assert!(parse_comment(&serde_json::json!({ "id": 1 })).is_none());
    }

    #[test]
    fn origin_is_read_from_a_plain_checkout_and_from_a_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::write(
            repo.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:acme/app.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .unwrap();
        assert_eq!(
            origin_remote(&repo).as_deref(),
            Some("git@github.com:acme/app.git")
        );

        // A linked worktree points at the main repo's config through its
        // `.git` file.
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        assert_eq!(
            origin_remote(&tree).as_deref(),
            Some("git@github.com:acme/app.git")
        );
    }

    #[test]
    fn a_checkout_with_no_origin_says_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git/config"),
            "[remote \"upstream\"]\n\turl = git@github.com:acme/app.git\n",
        )
        .unwrap();
        assert_eq!(origin_remote(dir.path()), None);
    }

    #[test]
    fn a_file_resolves_to_its_checkout_and_to_the_path_github_reports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = root.join("repos/app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/spec.md"), "# spec").unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();

        let (checkout, path) = checkout_of(root, "repos/app/docs/spec.md").expect("in a checkout");
        assert_eq!(checkout, repo);
        assert_eq!(path, "docs/spec.md", "repo-relative, not folder-relative");
        assert_eq!(head_branch(&checkout).as_deref(), Some("feature/x"));

        // A file in the folder but outside any repo has no PR to be part of.
        std::fs::create_dir_all(root.join("notes")).unwrap();
        std::fs::write(root.join("notes/todo.md"), "# todo").unwrap();
        assert!(checkout_of(root, "notes/todo.md").is_none());
    }

    #[test]
    fn a_detached_head_offers_no_branch_to_match_a_pull_request_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git/HEAD"),
            "9f1c0de4c0ffee0000000000000000000000beef\n",
        )
        .unwrap();
        assert_eq!(head_branch(dir.path()), None);
    }
}
