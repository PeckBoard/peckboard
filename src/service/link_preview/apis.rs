//! API card enrichment for every non-GitHub provider that has one: other
//! forges (GitLab, Gitea/Forgejo/Codeberg, Bitbucket — GitHub-shaped
//! `details` so the UI renders them alike), package registries, Hacker
//! News, Wikipedia, arXiv, Steam, Stack Exchange, Hugging Face, Poly Haven,
//! and the token-gated trackers (Linear, Jira, Trello, Sentry).
//!
//! Every request goes through the same SSRF-guarded fetch as the rest of
//! the preview service. Tracker tokens come from Peckboard variables (a
//! global env var by that name — plaintext, or encrypted and currently
//! unlocked — else the process environment) and, like the GitHub token, are
//! only spent for an admin's request: anyone else gets the page's own card.
//!
//! `details` conventions the UI understands, beyond the GitHub shape:
//! `type`, `badge` (`{label, tone}` with tone `ok|danger|merged|muted`),
//! `facts` (short strings shown as a meta row), and per-type fields
//! (`package`: `version`, `license`, `downloads`; `paper`: `authors`).

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;
use serde_json::{Value, json};

use super::LinkPreviewService;
use super::github::{GhCard as Card, json_rule};
use super::parse::{clean, decode_entities};
use super::providers::Enrich;
use super::ssrf::{BodyRule, FetchOptions, FetchPolicy, JSON_MAX_BYTES};
use crate::db::Db;

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(|x| match x {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn n(v: &Value, ptr: &str) -> Option<u64> {
    v.pointer(ptr).and_then(Value::as_u64)
}

fn text(v: Option<String>, max: usize) -> Option<String> {
    v.and_then(|t| clean(&t, max))
}

/// `1234567` → `1.2M`.
pub(super) fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => trim_point(n as f64 / 1e3, "k"),
        1_000_000..=999_999_999 => trim_point(n as f64 / 1e6, "M"),
        _ => trim_point(n as f64 / 1e9, "B"),
    }
}

fn trim_point(x: f64, unit: &str) -> String {
    let s = if x >= 100.0 {
        format!("{x:.0}")
    } else {
        format!("{x:.1}")
    };
    format!("{}{unit}", s.trim_end_matches(".0"))
}

fn badge(label: &str, tone: &str) -> Value {
    json!({ "label": label, "tone": tone })
}

/// Tags → text, entities decoded (HN / fediverse post bodies).
pub(super) fn html_text(html: &str, max: usize) -> Option<String> {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<[^>]*>").unwrap());
    static BREAK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>|</p>\s*<p[^>]*>").unwrap());
    let spaced = BREAK.replace_all(html, " ");
    clean(&decode_entities(&TAG.replace_all(&spaced, "")), max)
}

fn gunzip(body: Vec<u8>) -> Vec<u8> {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return body;
    }
    let mut out = Vec::new();
    let limit = (JSON_MAX_BYTES * 8) as u64;
    match flate2::read::GzDecoder::new(&body[..])
        .take(limit)
        .read_to_end(&mut out)
    {
        Ok(_) => out,
        Err(_) => Vec::new(),
    }
}

pub(super) async fn get_json(
    svc: &LinkPreviewService,
    policy: &FetchPolicy,
    url: &str,
    auth: Option<&str>,
    post_json: Option<&str>,
) -> Option<Value> {
    let f = svc
        .guarded_fetch(
            url,
            policy,
            &FetchOptions {
                accept: "application/json",
                auth,
                post_json,
                body: json_rule,
            },
        )
        .await
        .ok()?;
    serde_json::from_slice(&gunzip(f.body)).ok()
}

fn xml_rule(ct: &str) -> BodyRule {
    if ct.ends_with("xml") {
        BodyRule::Read {
            max: JSON_MAX_BYTES,
            truncate: false,
        }
    } else {
        BodyRule::Refuse
    }
}

/// A Peckboard variable (global scope) — plaintext, or encrypted and
/// currently unlocked — else the process environment.
async fn secret(db: &Db, name: &str) -> Option<String> {
    let from_db = match db.get_env_var(name, None).await {
        Ok(Some(var)) if !var.encrypted => var.value,
        Ok(Some(var)) => crate::service::env_vars::unlocked_values_blocking()
            .get(&var.id)
            .cloned(),
        _ => None,
    };
    from_db
        .or_else(|| std::env::var(name).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub(super) async fn enrich(
    svc: &LinkPreviewService,
    db: &Db,
    policy: &FetchPolicy,
    kind: Enrich,
    caps: &BTreeMap<String, String>,
    privileged: bool,
) -> Option<Card> {
    let cap = |k: &str| caps.get(k).map(String::as_str);
    match kind {
        Enrich::Npm => {
            let name = cap("name")?;
            let latest = get_json(
                svc,
                policy,
                &format!(
                    "https://registry.npmjs.org/{}/latest",
                    name.replace('/', "%2F")
                ),
                None,
                None,
            )
            .await?;
            let dl = get_json(
                svc,
                policy,
                &format!("https://api.npmjs.org/downloads/point/last-week/{name}"),
                None,
                None,
            )
            .await;
            Some(npm_card(&latest, dl.as_ref()))
        }
        Enrich::Crates => {
            let v = get_json(
                svc,
                policy,
                &format!("https://crates.io/api/v1/crates/{}", cap("name")?),
                None,
                None,
            )
            .await?;
            Some(crates_card(&v))
        }
        Enrich::Pypi => {
            let v = get_json(
                svc,
                policy,
                &format!("https://pypi.org/pypi/{}/json", cap("name")?),
                None,
                None,
            )
            .await?;
            Some(pypi_card(&v))
        }
        Enrich::HackerNews => {
            let v = get_json(
                svc,
                policy,
                &format!(
                    "https://hacker-news.firebaseio.com/v0/item/{}.json",
                    cap("id")?
                ),
                None,
                None,
            )
            .await?;
            hn_card(&v)
        }
        Enrich::Wikipedia => {
            let title = cap("title")?;
            if title == "." || title == ".." {
                return None;
            }
            // The title is already percent-encoded in the link's path.
            let v = get_json(
                svc,
                policy,
                &format!("https://{}/api/rest_v1/page/summary/{title}", cap("host")?),
                None,
                None,
            )
            .await?;
            wikipedia_card(&v)
        }
        Enrich::Arxiv => {
            let f = svc
                .guarded_fetch(
                    &format!(
                        "https://export.arxiv.org/api/query?id_list={}",
                        urlencoding::encode(cap("id")?)
                    ),
                    policy,
                    &FetchOptions::get("application/atom+xml", xml_rule),
                )
                .await
                .ok()?;
            arxiv_card(&String::from_utf8_lossy(&f.body))
        }
        Enrich::Steam => {
            let app = cap("app")?;
            let v = get_json(
                svc,
                policy,
                &format!("https://store.steampowered.com/api/appdetails?appids={app}"),
                None,
                None,
            )
            .await?;
            let reviews = get_json(
                svc,
                policy,
                &format!(
                    "https://store.steampowered.com/appreviews/{app}?json=1&language=all&purchase_type=all&num_per_page=0"
                ),
                None,
                None,
            )
            .await;
            steam_card(v.get(app)?.get("data")?, reviews.as_ref())
        }
        Enrich::StackExchange => {
            let host = cap("host")?;
            let site = host
                .trim_end_matches(".com")
                .trim_end_matches(".net")
                .trim_end_matches(".stackexchange");
            let v = get_json(
                svc,
                policy,
                &format!(
                    "https://api.stackexchange.com/2.3/questions/{}?site={site}",
                    cap("id")?
                ),
                None,
                None,
            )
            .await?;
            stackexchange_card(v.pointer("/items/0")?)
        }
        Enrich::DockerHub => {
            let ns = cap("ns").unwrap_or("library");
            let name = cap("name")?;
            let v = get_json(
                svc,
                policy,
                &format!("https://hub.docker.com/v2/repositories/{ns}/{name}/"),
                None,
                None,
            )
            .await?;
            Some(dockerhub_card(&v, ns, name))
        }
        Enrich::HuggingFace => {
            let dataset = cap("url")?.contains("/datasets/");
            let v = get_json(
                svc,
                policy,
                &format!(
                    "https://huggingface.co/api/{}/{}/{}",
                    if dataset { "datasets" } else { "models" },
                    cap("owner")?,
                    cap("repo")?
                ),
                None,
                None,
            )
            .await?;
            Some(hf_card(&v, dataset))
        }
        Enrich::Gist => {
            let v = get_json(
                svc,
                policy,
                &format!("https://api.github.com/gists/{}", cap("id")?),
                None,
                None,
            )
            .await?;
            Some(gist_card(&v))
        }
        Enrich::GitlabRepo | Enrich::GitlabMerge | Enrich::GitlabIssue => {
            let project = cap("project")?;
            let base = format!(
                "https://gitlab.com/api/v4/projects/{}",
                urlencoding::encode(project)
            );
            let endpoint = match kind {
                Enrich::GitlabMerge => format!("{base}/merge_requests/{}", cap("num")?),
                Enrich::GitlabIssue => format!("{base}/issues/{}", cap("num")?),
                _ => base,
            };
            let v = get_json(svc, policy, &endpoint, None, None).await?;
            Some(gitlab_card(kind, project, &v))
        }
        Enrich::GiteaRepo | Enrich::GiteaPull | Enrich::GiteaIssue => {
            let (owner, repo) = (cap("owner")?, cap("repo")?);
            let base = format!("https://{}/api/v1/repos/{owner}/{repo}", cap("host")?);
            let endpoint = match kind {
                Enrich::GiteaPull => format!("{base}/pulls/{}", cap("num")?),
                Enrich::GiteaIssue => format!("{base}/issues/{}", cap("num")?),
                _ => base,
            };
            let v = get_json(svc, policy, &endpoint, None, None).await?;
            Some(gitea_card(kind, &format!("{owner}/{repo}"), &v))
        }
        Enrich::BitbucketRepo | Enrich::BitbucketPull => {
            let (owner, repo) = (cap("owner")?, cap("repo")?);
            let base = format!("https://api.bitbucket.org/2.0/repositories/{owner}/{repo}");
            let endpoint = match kind {
                Enrich::BitbucketPull => format!("{base}/pullrequests/{}", cap("num")?),
                _ => base,
            };
            let v = get_json(svc, policy, &endpoint, None, None).await?;
            Some(bitbucket_card(kind, &format!("{owner}/{repo}"), &v))
        }
        Enrich::PolyHaven => {
            let id = cap("id")?;
            let v = get_json(
                svc,
                policy,
                &format!("https://api.polyhaven.com/info/{id}"),
                None,
                None,
            )
            .await?;
            Some(polyhaven_card(&v, id))
        }
        Enrich::Linear if privileged => {
            let key = secret(db, "LINEAR_API_KEY").await?;
            let body = json!({
                "query": "query($id:String!){issue(id:$id){identifier title description priorityLabel state{name type} assignee{name avatarUrl} team{name}}}",
                "variables": { "id": cap("key")? },
            })
            .to_string();
            let v = get_json(
                svc,
                policy,
                "https://api.linear.app/graphql",
                Some(&key),
                Some(&body),
            )
            .await?;
            linear_card(v.pointer("/data/issue")?)
        }
        Enrich::Jira if privileged => {
            let email = secret(db, "JIRA_EMAIL").await?;
            let token = secret(db, "JIRA_TOKEN").await?;
            let auth = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))
            );
            let v = get_json(
                svc,
                policy,
                &format!(
                    "https://{}/rest/api/3/issue/{}?fields=summary,status,assignee,priority,issuetype",
                    cap("host")?,
                    cap("key")?
                ),
                Some(&auth),
                None,
            )
            .await?;
            jira_card(&v)
        }
        Enrich::Trello if privileged => {
            let key = secret(db, "TRELLO_KEY").await?;
            let token = secret(db, "TRELLO_TOKEN").await?;
            let v = get_json(
                svc,
                policy,
                &format!(
                    "https://api.trello.com/1/cards/{}?key={}&token={}&fields=name,desc,closed,due,dueComplete,labels&board=true&board_fields=name&list=true&list_fields=name",
                    cap("card")?,
                    urlencoding::encode(&key),
                    urlencoding::encode(&token)
                ),
                None,
                None,
            )
            .await?;
            trello_card(&v)
        }
        Enrich::Sentry if privileged => {
            let token = secret(db, "SENTRY_TOKEN").await?;
            let org = match cap("org") {
                Some(o) => o.to_string(),
                None => cap("host")?.strip_suffix(".sentry.io")?.to_string(),
            };
            let v = get_json(
                svc,
                policy,
                &format!(
                    "https://sentry.io/api/0/organizations/{org}/issues/{}/",
                    cap("issue")?
                ),
                Some(&format!("Bearer {token}")),
                None,
            )
            .await?;
            sentry_card(&v)
        }
        _ => None,
    }
}

// ── Parsers (pure: fixture-tested) ────────────────────────────────────

pub(super) fn npm_card(latest: &Value, downloads: Option<&Value>) -> Card {
    let dl = downloads.and_then(|d| n(d, "/downloads"));
    Card {
        title: s(latest, "/name"),
        description: text(s(latest, "/description"), 300),
        image: None,
        details: json!({
            "type": "package",
            "registry": "npm",
            "version": s(latest, "/version"),
            "license": s(latest, "/license").or_else(|| s(latest, "/license/type")),
            "downloads": dl,
            "downloads_period": dl.map(|_| "week"),
        }),
    }
}

pub(super) fn crates_card(v: &Value) -> Card {
    let recent = n(v, "/crate/recent_downloads");
    Card {
        title: s(v, "/crate/name"),
        description: text(s(v, "/crate/description"), 300),
        image: None,
        details: json!({
            "type": "package",
            "registry": "crates.io",
            "version": s(v, "/crate/max_stable_version").or_else(|| s(v, "/crate/newest_version")),
            "license": s(v, "/versions/0/license"),
            "downloads": recent.or_else(|| n(v, "/crate/downloads")),
            "downloads_period": if recent.is_some() { Some("90 days") } else { None },
        }),
    }
}

pub(super) fn pypi_card(v: &Value) -> Card {
    let license = s(v, "/info/license_expression")
        .or_else(|| s(v, "/info/license").filter(|l| l.len() <= 40));
    Card {
        title: s(v, "/info/name"),
        description: text(s(v, "/info/summary"), 300),
        image: None,
        details: json!({
            "type": "package",
            "registry": "PyPI",
            "version": s(v, "/info/version"),
            "license": license,
            "author": s(v, "/info/author"),
        }),
    }
}

pub(super) fn hn_card(v: &Value) -> Option<Card> {
    let by = s(v, "/by");
    let mut facts = Vec::new();
    if let Some(p) = n(v, "/score") {
        facts.push(format!("▲ {p} points"));
    }
    if let Some(c) = n(v, "/descendants") {
        facts.push(format!("{c} comments"));
    }
    if let Some(b) = &by {
        facts.push(format!("by {b}"));
    }
    let body = v
        .get("text")
        .and_then(Value::as_str)
        .and_then(|t| html_text(t, 300));
    let title = text(s(v, "/title"), 300).or_else(|| body.clone())?;
    Some(Card {
        title: Some(title),
        description: if s(v, "/title").is_some() {
            body.or_else(|| s(v, "/url"))
        } else {
            None
        },
        image: None,
        details: json!({
            "type": "hn",
            "points": n(v, "/score"),
            "comments": n(v, "/descendants"),
            "author": by,
            "time": n(v, "/time"),
            "link": s(v, "/url"),
            "facts": facts,
        }),
    })
}

pub(super) fn wikipedia_card(v: &Value) -> Option<Card> {
    Some(Card {
        title: text(s(v, "/title"), 300)?.into(),
        description: text(s(v, "/extract"), 600),
        image: s(v, "/thumbnail/source").or_else(|| s(v, "/originalimage/source")),
        details: json!({
            "type": "wiki",
            "summary": s(v, "/description"),
            "facts": s(v, "/description").into_iter().collect::<Vec<_>>(),
        }),
    })
}

pub(super) fn arxiv_card(xml: &str) -> Option<Card> {
    static ENTRY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<entry>(.*?)</entry>").unwrap());
    static TITLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<title>(.*?)</title>").unwrap());
    static SUMMARY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<summary>(.*?)</summary>").unwrap());
    static AUTHOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<author>\s*<name>(.*?)</name>").unwrap());
    static PUBLISHED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<published>([0-9-]{10})").unwrap());
    static CATEGORY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"<arxiv:primary_category[^>]*term="([^"]+)""#).unwrap());
    let entry = ENTRY.captures(xml)?.get(1)?.as_str();
    let title = clean(
        &decode_entities(TITLE.captures(entry)?.get(1)?.as_str()),
        300,
    )?;
    let authors: Vec<String> = AUTHOR
        .captures_iter(entry)
        .filter_map(|c| clean(&decode_entities(&c[1]), 100))
        .take(50)
        .collect();
    let published = PUBLISHED.captures(entry).map(|c| c[1].to_string());
    let category = CATEGORY.captures(entry).map(|c| c[1].to_string());
    let mut facts = Vec::new();
    match authors.len() {
        0 => {}
        1..=3 => facts.push(authors.join(", ")),
        k => facts.push(format!("{} et al. ({k} authors)", authors[0])),
    }
    facts.extend(published.clone());
    facts.extend(category.clone());
    Some(Card {
        title: Some(title),
        description: SUMMARY
            .captures(entry)
            .and_then(|c| clean(&decode_entities(&c[1]), 600)),
        image: None,
        details: json!({
            "type": "paper",
            "authors": authors,
            "published": published,
            "category": category,
            "facts": facts,
        }),
    })
}

pub(super) fn steam_card(data: &Value, reviews: Option<&Value>) -> Option<Card> {
    let price = if data.get("is_free").and_then(Value::as_bool) == Some(true) {
        Some("Free".to_string())
    } else {
        s(data, "/price_overview/final_formatted")
    };
    let review = reviews.and_then(|r| {
        let desc = s(r, "/query_summary/review_score_desc")?;
        let total = n(r, "/query_summary/total_reviews").unwrap_or(0);
        Some(if total > 0 {
            format!("{desc} ({} reviews)", compact(total))
        } else {
            desc
        })
    });
    let release = s(data, "/release_date/date");
    let facts: Vec<String> = [price.clone(), review.clone(), release.clone()]
        .into_iter()
        .flatten()
        .collect();
    Some(Card {
        title: text(s(data, "/name"), 300)?.into(),
        description: s(data, "/short_description").and_then(|d| html_text(&d, 300)),
        image: s(data, "/header_image"),
        details: json!({
            "type": "game",
            "price": price,
            "reviews": review,
            "release": release,
            "facts": facts,
        }),
    })
}

pub(super) fn stackexchange_card(q: &Value) -> Option<Card> {
    let mut facts = Vec::new();
    if let Some(sc) = q.get("score").and_then(Value::as_i64) {
        facts.push(format!("{sc} votes"));
    }
    if let Some(a) = n(q, "/answer_count") {
        facts.push(format!("{a} answers"));
    }
    if let Some(v) = n(q, "/view_count") {
        facts.push(format!("{} views", compact(v)));
    }
    let answered = q.get("is_answered").and_then(Value::as_bool) == Some(true);
    let tags: Vec<String> = q
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .take(5)
                .collect()
        })
        .unwrap_or_default();
    Some(Card {
        title: s(q, "/title")
            .and_then(|t| clean(&decode_entities(&t), 300))?
            .into(),
        description: if tags.is_empty() {
            None
        } else {
            Some(tags.join(" · "))
        },
        image: s(q, "/owner/profile_image"),
        details: json!({
            "type": "question",
            "badge": if answered { badge("Answered", "ok") } else { badge("Open", "muted") },
            "author": s(q, "/owner/display_name").map(|a| decode_entities(&a)),
            "facts": facts,
        }),
    })
}

pub(super) fn dockerhub_card(v: &Value, ns: &str, name: &str) -> Card {
    let pulls = n(v, "/pull_count");
    Card {
        title: Some(if ns == "library" {
            name.to_string()
        } else {
            format!("{ns}/{name}")
        }),
        description: text(s(v, "/description"), 300),
        image: None,
        details: json!({
            "type": "package",
            "registry": "Docker Hub",
            "downloads": pulls,
            "downloads_period": pulls.map(|_| "pulls"),
            "stars": n(v, "/star_count"),
            "updated": s(v, "/last_updated"),
        }),
    }
}

pub(super) fn hf_card(v: &Value, dataset: bool) -> Card {
    let license = v
        .get("tags")
        .and_then(Value::as_array)
        .and_then(|t| {
            t.iter()
                .filter_map(Value::as_str)
                .find_map(|t| t.strip_prefix("license:"))
        })
        .map(str::to_string);
    let mut facts: Vec<String> = Vec::new();
    facts.extend(s(v, "/pipeline_tag"));
    facts.extend(s(v, "/library_name"));
    if let Some(d) = n(v, "/downloads") {
        facts.push(format!("↓ {}", compact(d)));
    }
    if let Some(l) = n(v, "/likes") {
        facts.push(format!("♥ {}", compact(l)));
    }
    facts.extend(license.clone());
    Card {
        title: s(v, "/id"),
        description: text(
            s(v, "/cardData/description").or_else(|| s(v, "/description")),
            300,
        ),
        image: None,
        details: json!({
            "type": if dataset { "dataset" } else { "model" },
            "license": license,
            "facts": facts,
        }),
    }
}

pub(super) fn gist_card(v: &Value) -> Card {
    let files: Vec<String> = v
        .get("files")
        .and_then(Value::as_object)
        .map(|f| f.keys().take(10).cloned().collect())
        .unwrap_or_default();
    let mut facts = vec![format!(
        "{} file{}",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    )];
    facts.extend(s(v, "/owner/login"));
    Card {
        title: files.first().cloned().or_else(|| Some("Gist".into())),
        description: text(s(v, "/description"), 300),
        image: s(v, "/owner/avatar_url"),
        details: json!({ "type": "gist", "files": files, "facts": facts }),
    }
}

pub(super) fn gitlab_card(kind: Enrich, project: &str, v: &Value) -> Card {
    match kind {
        Enrich::GitlabRepo => Card {
            title: s(v, "/name_with_namespace").or_else(|| Some(project.to_string())),
            description: text(s(v, "/description"), 300),
            image: s(v, "/avatar_url").or_else(|| s(v, "/namespace/avatar_url")),
            details: json!({
                "type": "repo",
                "repo": project,
                "stars": v.get("star_count"),
                "forks": v.get("forks_count"),
                "open_issues": v.get("open_issues_count"),
                "default_branch": v.get("default_branch"),
            }),
        },
        _ => {
            let is_mr = kind == Enrich::GitlabMerge;
            let state = s(v, "/state").unwrap_or_default();
            Card {
                title: text(s(v, "/title"), 300),
                description: text(s(v, "/description"), 300),
                image: s(v, "/author/avatar_url"),
                details: json!({
                    "type": if is_mr { "pull" } else { "issue" },
                    "repo": project,
                    "number": v.get("iid"),
                    "state": if state == "opened" { "open" } else { "closed" },
                    "merged": state == "merged",
                    "draft": v.get("draft").and_then(Value::as_bool).unwrap_or(false),
                    "author": s(v, "/author/username"),
                    "comments": v.get("user_notes_count"),
                }),
            }
        }
    }
}

pub(super) fn gitea_card(kind: Enrich, full_name: &str, v: &Value) -> Card {
    match kind {
        Enrich::GiteaRepo => Card {
            title: s(v, "/full_name").or_else(|| Some(full_name.to_string())),
            description: text(s(v, "/description"), 300),
            image: s(v, "/owner/avatar_url"),
            details: json!({
                "type": "repo",
                "repo": full_name,
                "stars": v.get("stars_count"),
                "forks": v.get("forks_count"),
                "open_issues": v.get("open_issues_count"),
                "language": v.get("language"),
            }),
        },
        _ => Card {
            title: text(s(v, "/title"), 300),
            description: text(s(v, "/body"), 300),
            image: s(v, "/user/avatar_url"),
            details: json!({
                "type": if kind == Enrich::GiteaPull { "pull" } else { "issue" },
                "repo": full_name,
                "number": v.get("number"),
                "state": v.get("state"),
                "merged": v.get("merged").and_then(Value::as_bool).unwrap_or(false),
                "draft": v.get("draft").and_then(Value::as_bool).unwrap_or(false),
                "author": s(v, "/user/login"),
                "comments": v.get("comments"),
            }),
        },
    }
}

pub(super) fn bitbucket_card(kind: Enrich, full_name: &str, v: &Value) -> Card {
    if kind == Enrich::BitbucketRepo {
        return Card {
            title: s(v, "/full_name").or_else(|| Some(full_name.to_string())),
            description: text(s(v, "/description"), 300),
            image: s(v, "/links/avatar/href"),
            details: json!({
                "type": "repo",
                "repo": full_name,
                "language": v.get("language"),
            }),
        };
    }
    let state = s(v, "/state").unwrap_or_default();
    Card {
        title: text(s(v, "/title"), 300),
        description: text(s(v, "/description"), 300),
        image: s(v, "/author/links/avatar/href"),
        details: json!({
            "type": "pull",
            "repo": full_name,
            "number": v.get("id"),
            "state": if state == "OPEN" { "open" } else { "closed" },
            "merged": state == "MERGED",
            "author": s(v, "/author/display_name"),
            "comments": v.get("comment_count"),
        }),
    }
}

pub(super) fn polyhaven_card(v: &Value, id: &str) -> Card {
    let kind = match n(v, "/type") {
        Some(0) => "HDRI",
        Some(1) => "Texture",
        Some(2) => "Model",
        _ => "Asset",
    };
    let authors: Vec<String> = v
        .get("authors")
        .and_then(Value::as_object)
        .map(|a| a.keys().cloned().collect())
        .unwrap_or_default();
    let mut facts = vec![kind.to_string()];
    if !authors.is_empty() {
        facts.push(format!("by {}", authors.join(", ")));
    }
    if let Some(d) = n(v, "/download_count") {
        facts.push(format!("↓ {}", compact(d)));
    }
    facts.push("CC0".into());
    Card {
        title: s(v, "/name").or_else(|| Some(id.to_string())),
        description: None,
        image: Some(format!(
            "https://cdn.polyhaven.com/asset_img/thumbs/{id}.png?width=512"
        )),
        details: json!({ "type": "asset", "asset_kind": kind, "facts": facts }),
    }
}

fn ticket(state: Option<String>, tone: &str, facts: Vec<String>) -> Value {
    json!({
        "type": "ticket",
        "badge": state.map(|st| badge(&st, tone)),
        "facts": facts,
    })
}

pub(super) fn linear_card(issue: &Value) -> Option<Card> {
    let tone = match s(issue, "/state/type").as_deref() {
        Some("completed") => "merged",
        Some("canceled") => "muted",
        Some("started") => "ok",
        _ => "muted",
    };
    let facts: Vec<String> = [
        s(issue, "/identifier"),
        s(issue, "/assignee/name"),
        s(issue, "/priorityLabel"),
        s(issue, "/team/name"),
    ]
    .into_iter()
    .flatten()
    .collect();
    Some(Card {
        title: text(s(issue, "/title"), 300)?.into(),
        description: text(s(issue, "/description"), 300),
        image: s(issue, "/assignee/avatarUrl"),
        details: ticket(s(issue, "/state/name"), tone, facts),
    })
}

pub(super) fn jira_card(v: &Value) -> Option<Card> {
    let tone = match s(v, "/fields/status/statusCategory/key").as_deref() {
        Some("done") => "merged",
        Some("indeterminate") => "ok",
        _ => "muted",
    };
    let facts: Vec<String> = [
        s(v, "/key"),
        s(v, "/fields/issuetype/name"),
        s(v, "/fields/assignee/displayName"),
        s(v, "/fields/priority/name"),
    ]
    .into_iter()
    .flatten()
    .collect();
    Some(Card {
        title: text(s(v, "/fields/summary"), 300)?.into(),
        description: None,
        image: None,
        details: ticket(s(v, "/fields/status/name"), tone, facts),
    })
}

pub(super) fn trello_card(v: &Value) -> Option<Card> {
    let closed = v.get("closed").and_then(Value::as_bool) == Some(true);
    let mut facts: Vec<String> = [s(v, "/board/name"), s(v, "/list/name")]
        .into_iter()
        .flatten()
        .collect();
    if let Some(due) = s(v, "/due") {
        facts.push(format!("due {}", due.get(..10).unwrap_or(&due)));
    }
    Some(Card {
        title: text(s(v, "/name"), 300)?.into(),
        description: text(s(v, "/desc"), 300),
        image: None,
        details: ticket(
            Some(if closed { "Archived" } else { "Open" }.into()),
            if closed { "muted" } else { "ok" },
            facts,
        ),
    })
}

pub(super) fn sentry_card(v: &Value) -> Option<Card> {
    let status = s(v, "/status");
    let tone = match status.as_deref() {
        Some("resolved") => "merged",
        Some("ignored") => "muted",
        _ => "danger",
    };
    let mut facts: Vec<String> = [s(v, "/shortId"), s(v, "/level"), s(v, "/project/name")]
        .into_iter()
        .flatten()
        .collect();
    if let Some(c) = s(v, "/count") {
        facts.push(format!("{c} events"));
    }
    if let Some(u) = n(v, "/userCount") {
        facts.push(format!("{u} users"));
    }
    Some(Card {
        title: text(s(v, "/title"), 300)?.into(),
        description: text(s(v, "/culprit"), 300),
        image: None,
        details: ticket(status, tone, facts),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_fixture_parses() {
        let latest = json!({
            "name": "left-pad", "version": "1.3.0",
            "description": "String left pad", "license": "WTFPL"
        });
        let dl = json!({"downloads": 1_234_567, "package": "left-pad"});
        let c = npm_card(&latest, Some(&dl));
        assert_eq!(c.title.as_deref(), Some("left-pad"));
        assert_eq!(c.details["version"], "1.3.0");
        assert_eq!(c.details["license"], "WTFPL");
        assert_eq!(c.details["downloads"], 1_234_567);
        assert_eq!(c.details["type"], "package");
        assert_eq!(compact(1_234_567), "1.2M");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(12_000), "12k");
    }

    #[test]
    fn hacker_news_fixture_parses() {
        let v = json!({
            "by": "dhouston", "descendants": 71, "id": 8863, "score": 111,
            "time": 1175714200, "title": "My YC app: Dropbox",
            "type": "story", "url": "http://www.getdropbox.com/u/2/screencast.html"
        });
        let c = hn_card(&v).unwrap();
        assert_eq!(c.title.as_deref(), Some("My YC app: Dropbox"));
        assert_eq!(c.details["points"], 111);
        assert_eq!(
            c.details["facts"],
            json!(["▲ 111 points", "71 comments", "by dhouston"])
        );
        // A comment has no title: its text becomes the title.
        let comment = json!({"by": "pg", "text": "Nice <i>work</i>&#x27;s", "type": "comment"});
        assert_eq!(
            hn_card(&comment).unwrap().title.as_deref(),
            Some("Nice work's")
        );
    }

    #[test]
    fn wikipedia_fixture_parses() {
        let v = json!({
            "title": "Rust (programming language)",
            "description": "General-purpose programming language",
            "extract": "Rust is a general-purpose programming language.",
            "thumbnail": {"source": "https://upload.wikimedia.org/x.png"}
        });
        let c = wikipedia_card(&v).unwrap();
        assert_eq!(c.title.as_deref(), Some("Rust (programming language)"));
        assert_eq!(
            c.image.as_deref(),
            Some("https://upload.wikimedia.org/x.png")
        );
        assert_eq!(c.details["type"], "wiki");
    }

    #[test]
    fn arxiv_and_forge_fixtures_parse() {
        let xml = r#"<feed><entry><id>x</id><published>2017-06-12T17:57:34Z</published>
            <title>Attention Is All
              You Need</title><summary> The dominant models &amp; more. </summary>
            <author><name>Ashish Vaswani</name></author><author><name>Noam Shazeer</name></author>
            <arxiv:primary_category xmlns:arxiv="x" term="cs.CL" scheme="y"/></entry></feed>"#;
        let c = arxiv_card(xml).unwrap();
        assert_eq!(c.title.as_deref(), Some("Attention Is All You Need"));
        assert_eq!(
            c.description.as_deref(),
            Some("The dominant models & more.")
        );
        assert_eq!(
            c.details["authors"],
            json!(["Ashish Vaswani", "Noam Shazeer"])
        );
        assert_eq!(c.details["category"], "cs.CL");

        let mr =
            json!({"iid": 12, "title": "Fix", "state": "merged", "author": {"username": "dev"}});
        let c = gitlab_card(Enrich::GitlabMerge, "g/p", &mr);
        assert_eq!(c.details["type"], "pull");
        assert_eq!(c.details["merged"], true);
        assert_eq!(c.details["number"], 12);
    }
}
