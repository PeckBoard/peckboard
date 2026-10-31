//! GitHub card enrichment: repo / PR / issue / commit / Actions run details
//! from the REST API, through the same SSRF-guarded fetch as every other
//! preview request. Credentials are the ones `service::github_pr` already
//! uses (the `github-bridge` plugin's token, else `PECKBOARD_GITHUB_TOKEN`),
//! and only an admin's request may spend them — anyone else (and every
//! request when none are configured) gets the public, unauthenticated API.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::LinkPreviewService;
use super::parse::clean;
use super::providers::Enrich;
use super::ssrf::{BodyRule, FetchOptions, FetchPolicy, JSON_MAX_BYTES};
use crate::db::Db;
use crate::service::github_pr::{self, GithubConfig};

pub(super) struct GhCard {
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub details: Value,
}

pub(super) fn json_rule(ct: &str) -> BodyRule {
    if ct == "application/json" || ct.ends_with("+json") || ct == "text/json" {
        BodyRule::Read {
            max: JSON_MAX_BYTES,
            truncate: false,
        }
    } else {
        BodyRule::Refuse
    }
}

pub(super) async fn enrich(
    svc: &LinkPreviewService,
    db: &Db,
    policy: &FetchPolicy,
    kind: Enrich,
    caps: &BTreeMap<String, String>,
    privileged: bool,
) -> Option<GhCard> {
    let cfg = github_pr::config(db).await;
    let (api, token) = credentials(cfg.as_ref(), privileged);
    enrich_with(svc, policy, api, token, kind, caps).await
}

/// API base + token for a preview request. The configured token is the
/// operator's: it can read private repos, so a non-admin must never get a
/// card built with it — they get the public API's view instead.
fn credentials(cfg: Option<&GithubConfig>, privileged: bool) -> (&str, Option<&str>) {
    match cfg {
        Some(c) if privileged => (c.api_base.as_str(), Some(c.token.as_str())),
        _ => (github_pr::DEFAULT_API_BASE, None),
    }
}

pub(super) async fn enrich_with(
    svc: &LinkPreviewService,
    policy: &FetchPolicy,
    api_base: &str,
    token: Option<&str>,
    kind: Enrich,
    caps: &BTreeMap<String, String>,
) -> Option<GhCard> {
    let owner = caps.get("owner")?;
    let repo = caps.get("repo")?.trim_end_matches(".git");
    let base = format!("{}/repos/{owner}/{repo}", api_base.trim_end_matches('/'));
    let endpoint = match kind {
        Enrich::GithubRepo => base,
        Enrich::GithubPull => format!("{base}/pulls/{}", caps.get("num")?),
        Enrich::GithubIssue => format!("{base}/issues/{}", caps.get("num")?),
        Enrich::GithubCommit => format!("{base}/commits/{}", caps.get("sha")?),
        Enrich::GithubRun => format!("{base}/actions/runs/{}", caps.get("run")?),
        _ => return None,
    };
    let auth = token.map(|t| format!("Bearer {t}"));
    let opts = FetchOptions {
        accept: "application/vnd.github+json",
        auth: auth.as_deref(),
        post_json: None,
        body: json_rule,
    };
    let fetched = svc.guarded_fetch(&endpoint, policy, &opts).await.ok()?;
    let v: Value = serde_json::from_slice(&fetched.body).ok()?;
    let mut card = card_from(kind, &format!("{owner}/{repo}"), &v);
    if kind == Enrich::GithubRun
        && let Some(jobs_url) = v.get("jobs_url").and_then(Value::as_str)
        && jobs_url.starts_with(api_base.trim_end_matches('/'))
        && let Ok(f) = svc.guarded_fetch(jobs_url, policy, &opts).await
        && let Ok(jobs) = serde_json::from_slice::<Value>(&f.body)
    {
        apply_jobs(&mut card.details, &jobs);
    }
    Some(card)
}

/// Per-job status of an Actions run: `jobs` (name/status/conclusion) and a
/// `checks` summary like `3/4 passed`.
fn apply_jobs(details: &mut Value, jobs: &Value) {
    let Some(list) = jobs.get("jobs").and_then(Value::as_array) else {
        return;
    };
    let summary: Vec<Value> = list
        .iter()
        .take(20)
        .map(|j| {
            json!({
                "name": j.get("name"),
                "status": j.get("status"),
                "conclusion": j.get("conclusion"),
            })
        })
        .collect();
    let passed = list
        .iter()
        .filter(|j| j.get("conclusion").and_then(Value::as_str) == Some("success"))
        .count();
    details["checks"] = json!(format!("{passed}/{} passed", list.len()));
    details["jobs"] = Value::Array(summary);
}

fn s(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for k in path {
        cur = cur.get(k)?;
    }
    cur.as_str().map(str::to_string)
}

fn text(v: Option<String>, max: usize) -> Option<String> {
    v.and_then(|t| clean(&t, max))
}

fn card_from(kind: Enrich, full_name: &str, v: &Value) -> GhCard {
    match kind {
        Enrich::GithubRepo => GhCard {
            title: s(v, &["full_name"]).or_else(|| Some(full_name.to_string())),
            description: text(s(v, &["description"]), 500),
            image: s(v, &["owner", "avatar_url"]),
            details: json!({
                "type": "repo",
                "repo": full_name,
                "stars": v.get("stargazers_count"),
                "forks": v.get("forks_count"),
                "open_issues": v.get("open_issues_count"),
                "language": v.get("language"),
                "default_branch": v.get("default_branch"),
                "archived": v.get("archived"),
            }),
        },
        Enrich::GithubPull | Enrich::GithubIssue => {
            let is_pull = kind == Enrich::GithubPull;
            let labels: Vec<Value> = v
                .get("labels")
                .and_then(|l| l.as_array())
                .map(|a| a.iter().filter_map(|l| l.get("name").cloned()).collect())
                .unwrap_or_default();
            let mut details = json!({
                "type": if is_pull { "pull" } else { "issue" },
                "repo": full_name,
                "number": v.get("number"),
                "state": v.get("state"),
                "author": s(v, &["user", "login"]),
                "comments": v.get("comments"),
                "labels": labels,
            });
            if is_pull {
                for k in [
                    "merged",
                    "draft",
                    "commits",
                    "additions",
                    "deletions",
                    "changed_files",
                ] {
                    details[k] = v.get(k).cloned().unwrap_or(Value::Null);
                }
            } else {
                details["state_reason"] = v.get("state_reason").cloned().unwrap_or(Value::Null);
            }
            GhCard {
                title: text(s(v, &["title"]), 300),
                description: text(s(v, &["body"]), 300),
                image: s(v, &["user", "avatar_url"]),
                details,
            }
        }
        Enrich::GithubCommit => {
            let msg = s(v, &["commit", "message"]).unwrap_or_default();
            let (head, rest) = msg.split_once('\n').unwrap_or((&msg, ""));
            let sha = s(v, &["sha"]).unwrap_or_default();
            GhCard {
                title: text(Some(head.to_string()), 300),
                description: text(Some(rest.to_string()), 300),
                image: s(v, &["author", "avatar_url"]),
                details: json!({
                    "type": "commit",
                    "repo": full_name,
                    "sha": sha.get(..7).unwrap_or(&sha),
                    "author": s(v, &["commit", "author", "name"]),
                    "login": s(v, &["author", "login"]),
                    "date": s(v, &["commit", "author", "date"]),
                    "additions": v.pointer("/stats/additions"),
                    "deletions": v.pointer("/stats/deletions"),
                }),
            }
        }
        _ => {
            // Actions run.
            let name = s(v, &["name"]);
            GhCard {
                title: text(s(v, &["display_title"]).or_else(|| name.clone()), 300),
                description: text(name, 300),
                image: s(v, &["actor", "avatar_url"]),
                details: json!({
                    "type": "run",
                    "repo": full_name,
                    "run_number": v.get("run_number"),
                    "status": v.get("status"),
                    "conclusion": v.get("conclusion"),
                    "event": v.get("event"),
                    "head_branch": v.get("head_branch"),
                    "workflow": v.get("name"),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_admins_spend_the_configured_token() {
        let cfg = GithubConfig {
            token: "secret".into(),
            api_base: "https://ghe.example/api/v3".into(),
        };
        assert_eq!(
            credentials(Some(&cfg), true),
            ("https://ghe.example/api/v3", Some("secret"))
        );
        assert_eq!(
            credentials(Some(&cfg), false),
            (github_pr::DEFAULT_API_BASE, None)
        );
        assert_eq!(credentials(None, true), (github_pr::DEFAULT_API_BASE, None));
    }
}
