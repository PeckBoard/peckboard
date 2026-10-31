//! Fediverse posts, instance-agnostic.
//!
//! A link whose path looks like a post on some fediverse software (Mastodon
//! `/@user/123`, Misskey `/notes/abc`, Lemmy `/post/1`, PeerTube `/w/…`, …)
//! makes the server probe that host's `/.well-known/nodeinfo` (cached 24 h,
//! through the SSRF guard) to learn which software it runs, then read the
//! post from that software's JSON API. The result is a rich `post` card —
//! author, avatar, text, media thumbnails — never an iframe: the CSP can't
//! list arbitrary instances. A PeerTube video with a direct file plays
//! through the media proxy instead.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use reqwest::Url;
use serde_json::{Value, json};

use super::LinkPreviewService;
use super::apis::{get_json, html_text};
use super::cache::TtlLru;
use super::github::GhCard as Card;
use super::parse::clean;
use super::ssrf::FetchPolicy;

const NODEINFO_TTL: Duration = Duration::from_secs(24 * 3600);
const NODEINFO_ERR_TTL: Duration = Duration::from_secs(3600);
const MAX_THUMBS: usize = 4;

/// What a post-shaped path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PostRef {
    /// Mastodon-API status (Mastodon, Pleroma/Akkoma, Pixelfed, …).
    Status(String),
    /// Misskey-family note.
    Note(String),
    LemmyPost(String),
    LemmyComment(String),
    /// Kbin/Mbin thread — their API needs OAuth, so the page's own card.
    Entry,
    PeerTubeVideo(String),
}

static PATTERNS: LazyLock<Vec<(Regex, fn(String) -> PostRef)>> = LazyLock::new(|| {
    let r = |p: &str| Regex::new(p).unwrap();
    vec![
        (
            r(r"^/@[A-Za-z0-9_.-]{1,64}(?:@[A-Za-z0-9.-]{1,253})?/(?P<id>[0-9]{1,24})/?$"),
            PostRef::Status as fn(String) -> PostRef,
        ),
        (
            r(r"^/users/[A-Za-z0-9_.-]{1,64}/statuses/(?P<id>[0-9]{1,24})/?$"),
            PostRef::Status,
        ),
        (r(r"^/notice/(?P<id>[A-Za-z0-9]{1,40})/?$"), PostRef::Status),
        (
            r(r"^/p/[A-Za-z0-9_.-]{1,64}/(?P<id>[0-9]{1,24})/?$"),
            PostRef::Status,
        ),
        (r(r"^/i/web/post/(?P<id>[0-9]{1,24})/?$"), PostRef::Status),
        (r(r"^/notes/(?P<id>[A-Za-z0-9]{6,32})/?$"), PostRef::Note),
        (r(r"^/post/(?P<id>[0-9]{1,12})/?$"), PostRef::LemmyPost),
        (
            r(r"^/comment/(?P<id>[0-9]{1,12})/?$"),
            PostRef::LemmyComment,
        ),
        (
            r(r"^/m/[A-Za-z0-9_.@-]{1,100}/t/(?P<id>[0-9]{1,12})(?:/.*)?$"),
            |_| PostRef::Entry,
        ),
        (
            r(r"^/w/(?P<id>[1-9A-HJ-NP-Za-km-z]{16,24})/?$"),
            PostRef::PeerTubeVideo,
        ),
        (
            r(r"^/videos/watch/(?P<id>[0-9a-f-]{36})/?$"),
            PostRef::PeerTubeVideo,
        ),
    ]
});

/// The post a URL's path names, if it looks like one. Cheap: no network.
pub(super) fn post_ref(url: &Url) -> Option<PostRef> {
    let path = url.path();
    PATTERNS.iter().find_map(|(re, make)| {
        re.captures(path).map(|c| {
            make(
                c.name("id")
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default(),
            )
        })
    })
}

/// Which API family a nodeinfo `software.name` speaks.
fn family(software: &str) -> Option<&'static str> {
    Some(match software {
        "mastodon" | "hometown" | "glitchsoc" | "pleroma" | "akkoma" | "pixelfed"
        | "gotosocial" | "friendica" => "mastodon",
        "misskey" | "sharkey" | "firefish" | "iceshrimp" | "foundkey" | "cherrypick"
        | "calckey" | "catodon" => "misskey",
        "lemmy" => "lemmy",
        "kbin" | "mbin" => "kbin",
        "peertube" => "peertube",
        _ => return None,
    })
}

/// A software name from a nodeinfo 2.x document.
pub(super) fn software_of(nodeinfo: &Value) -> Option<String> {
    let name = nodeinfo.pointer("/software/name")?.as_str()?;
    let name = name.trim().to_ascii_lowercase();
    (!name.is_empty() && name.len() <= 64).then_some(name)
}

/// The nodeinfo 2.x document URL from `/.well-known/nodeinfo`.
pub(super) fn nodeinfo_href(wellknown: &Value) -> Option<String> {
    wellknown
        .get("links")?
        .as_array()?
        .iter()
        .filter(|l| {
            l.get("rel")
                .and_then(Value::as_str)
                .is_some_and(|r| r.starts_with("http://nodeinfo.diaspora.software/ns/schema/2"))
        })
        .filter_map(|l| l.get("href")?.as_str().map(str::to_string))
        .next_back()
}

fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

static NODEINFO: LazyLock<Mutex<TtlLru<Option<String>>>> =
    LazyLock::new(|| Mutex::new(TtlLru::new(2000, usize::MAX)));

/// The software `url`'s host runs, per its nodeinfo (cached per origin).
async fn software(svc: &LinkPreviewService, policy: &FetchPolicy, url: &Url) -> Option<String> {
    let key = origin(url);
    if let Some(hit) = NODEINFO.lock().unwrap().get(&key) {
        return hit;
    }
    let found = async {
        let wk = get_json(
            svc,
            policy,
            &format!("{key}/.well-known/nodeinfo"),
            None,
            None,
        )
        .await?;
        let href = nodeinfo_href(&wk)?;
        let doc = get_json(svc, policy, &href, None, None).await?;
        software_of(&doc)
    }
    .await;
    let ttl = if found.is_some() {
        NODEINFO_TTL
    } else {
        NODEINFO_ERR_TTL
    };
    NODEINFO.lock().unwrap().insert(key, found.clone(), ttl, 1);
    found
}

/// A fediverse preview: the post card, or a video file to play.
pub(super) struct FediPost {
    /// The software (nodeinfo name), used as the preview's provider.
    pub software: String,
    pub card: Card,
    /// `(file url, mime)` of a directly playable video.
    pub video: Option<(String, String)>,
}

/// Probe + fetch. `None` falls back to scraping the page.
pub(super) async fn lookup(
    svc: &LinkPreviewService,
    policy: &FetchPolicy,
    url: &Url,
    post: &PostRef,
) -> Option<FediPost> {
    let software = software(svc, policy, url).await?;
    let fam = family(&software)?;
    let base = origin(url);
    let host = url.host_str().unwrap_or_default().to_string();
    let card = match (fam, post) {
        ("mastodon", PostRef::Status(id)) => {
            let v = get_json(
                svc,
                policy,
                &format!("{base}/api/v1/statuses/{id}"),
                None,
                None,
            )
            .await?;
            mastodon_card(&v, &host)?
        }
        ("misskey", PostRef::Note(id)) => {
            let body = json!({ "noteId": id }).to_string();
            let v = get_json(
                svc,
                policy,
                &format!("{base}/api/notes/show"),
                None,
                Some(&body),
            )
            .await?;
            misskey_card(&v, &host)?
        }
        ("lemmy", PostRef::LemmyPost(id)) => {
            let v = get_json(
                svc,
                policy,
                &format!("{base}/api/v3/post?id={id}"),
                None,
                None,
            )
            .await?;
            lemmy_card(v.get("post_view")?, &host, false)?
        }
        ("lemmy", PostRef::LemmyComment(id)) => {
            let v = get_json(
                svc,
                policy,
                &format!("{base}/api/v3/comment?id={id}"),
                None,
                None,
            )
            .await?;
            lemmy_card(v.get("comment_view")?, &host, true)?
        }
        ("peertube", PostRef::PeerTubeVideo(id)) => {
            let v = get_json(
                svc,
                policy,
                &format!("{base}/api/v1/videos/{id}"),
                None,
                None,
            )
            .await?;
            let (card, video) = peertube_card(&v, &base)?;
            return Some(FediPost {
                software,
                card,
                video,
            });
        }
        _ => return None,
    };
    Some(FediPost {
        software,
        card,
        video: None,
    })
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn n(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(Value::as_i64)
}

#[allow(clippy::too_many_arguments)]
fn post_details(
    software_kind: &str,
    author: Option<String>,
    handle: String,
    avatar: Option<String>,
    text: Option<String>,
    thumbs: Vec<String>,
    created_at: Option<String>,
    counts: Value,
    warning: Option<String>,
) -> Value {
    json!({
        "type": "post",
        "software": software_kind,
        "author": author,
        "handle": handle,
        "avatar": avatar,
        "text": text,
        "thumbs": thumbs,
        "created_at": created_at,
        "counts": counts,
        "warning": warning,
    })
}

fn full_handle(acct: &str, host: &str) -> String {
    if acct.contains('@') {
        format!("@{acct}")
    } else {
        format!("@{acct}@{host}")
    }
}

pub(super) fn mastodon_card(v: &Value, host: &str) -> Option<Card> {
    let v = match v.get("reblog") {
        Some(r) if r.is_object() => r,
        _ => v,
    };
    let acct = s(v, "/account/acct")?;
    let author = s(v, "/account/display_name").or_else(|| s(v, "/account/username"));
    let text = s(v, "/content").and_then(|c| html_text(&c, 500));
    let thumbs: Vec<String> = v
        .get("media_attachments")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| s(m, "/preview_url").or_else(|| s(m, "/url")))
                .take(MAX_THUMBS)
                .collect()
        })
        .unwrap_or_default();
    Some(Card {
        title: author.clone().or_else(|| Some(full_handle(&acct, host))),
        description: text.clone(),
        image: None,
        details: post_details(
            "mastodon",
            author,
            full_handle(&acct, host),
            s(v, "/account/avatar_static").or_else(|| s(v, "/account/avatar")),
            text,
            thumbs,
            s(v, "/created_at"),
            json!({
                "replies": n(v, "/replies_count"),
                "boosts": n(v, "/reblogs_count"),
                "likes": n(v, "/favourites_count"),
            }),
            s(v, "/spoiler_text"),
        ),
    })
}

pub(super) fn misskey_card(v: &Value, host: &str) -> Option<Card> {
    let username = s(v, "/user/username")?;
    let acct = match s(v, "/user/host") {
        Some(h) => format!("{username}@{h}"),
        None => username.clone(),
    };
    let author = s(v, "/user/name").or(Some(username));
    let text = s(v, "/text").and_then(|t| clean(&t, 500));
    let thumbs: Vec<String> = v
        .get("files")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|f| s(f, "/type").is_some_and(|t| t.starts_with("image/")))
                .filter_map(|f| s(f, "/thumbnailUrl").or_else(|| s(f, "/url")))
                .take(MAX_THUMBS)
                .collect()
        })
        .unwrap_or_default();
    let reactions: i64 = v
        .get("reactions")
        .and_then(Value::as_object)
        .map(|r| r.values().filter_map(Value::as_i64).sum())
        .unwrap_or(0);
    Some(Card {
        title: author.clone(),
        description: text.clone(),
        image: None,
        details: post_details(
            "misskey",
            author,
            full_handle(&acct, host),
            s(v, "/user/avatarUrl"),
            text,
            thumbs,
            s(v, "/createdAt"),
            json!({
                "replies": n(v, "/repliesCount"),
                "boosts": n(v, "/renoteCount"),
                "likes": reactions,
            }),
            s(v, "/cw"),
        ),
    })
}

pub(super) fn lemmy_card(view: &Value, host: &str, comment: bool) -> Option<Card> {
    let name = s(view, "/creator/name")?;
    let author = s(view, "/creator/display_name").or(Some(name.clone()));
    let community = s(view, "/community/name");
    let (title, body) = if comment {
        (s(view, "/post/name"), s(view, "/comment/content"))
    } else {
        (s(view, "/post/name"), s(view, "/post/body"))
    };
    let text = body.and_then(|b| clean(&b, 500));
    let thumbs: Vec<String> = s(view, "/post/thumbnail_url").into_iter().collect();
    let mut details = post_details(
        "lemmy",
        author,
        full_handle(&name, host),
        s(view, "/creator/avatar"),
        text.clone(),
        if comment { Vec::new() } else { thumbs },
        s(view, "/comment/published").or_else(|| s(view, "/post/published")),
        json!({
            "score": n(view, "/counts/score"),
            "replies": n(view, "/counts/comments").or_else(|| n(view, "/counts/child_count")),
        }),
        None,
    );
    details["community"] = json!(community);
    details["link"] = json!(s(view, "/post/url"));
    Some(Card {
        title: title.and_then(|t| clean(&t, 300)),
        description: text,
        image: None,
        details,
    })
}

/// `(card, playable mp4 file)`: the largest mp4 at ≤ 720p, else the
/// smallest one there is.
pub(super) fn peertube_card(v: &Value, base: &str) -> Option<(Card, Option<(String, String)>)> {
    let abs = |p: Option<String>| {
        p.map(|p| {
            if p.starts_with("http") {
                p
            } else {
                format!("{base}{p}")
            }
        })
    };
    let mut files: Vec<(i64, String)> = Vec::new();
    let mut collect = |list: Option<&Value>| {
        for f in list.and_then(Value::as_array).into_iter().flatten() {
            if let Some(u) = s(f, "/fileUrl").filter(|u| u.ends_with(".mp4")) {
                files.push((n(f, "/resolution/id").unwrap_or(0), u));
            }
        }
    };
    collect(v.get("files"));
    for pl in v
        .get("streamingPlaylists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        collect(pl.get("files"));
    }
    files.sort_by_key(|(res, _)| *res);
    let pick = files
        .iter()
        .rev()
        .find(|(res, _)| *res > 0 && *res <= 720)
        .or(files.first())
        .map(|(_, u)| (u.clone(), "video/mp4".to_string()));
    let handle = match (s(v, "/account/name"), s(v, "/account/host")) {
        (Some(nm), Some(h)) => format!("@{nm}@{h}"),
        (Some(nm), None) => format!("@{nm}"),
        _ => String::new(),
    };
    let mut facts = Vec::new();
    if let Some(d) = n(v, "/duration") {
        facts.push(format!("{}:{:02}", d / 60, d % 60));
    }
    if let Some(vw) = n(v, "/views") {
        facts.push(format!("{vw} views"));
    }
    let author = s(v, "/account/displayName");
    let card = Card {
        title: s(v, "/name").and_then(|t| clean(&t, 300)),
        description: s(v, "/description").and_then(|d| clean(&d, 300)),
        image: abs(s(v, "/previewPath").or_else(|| s(v, "/thumbnailPath"))),
        details: json!({
            "type": "video",
            "software": "peertube",
            "author": author,
            "handle": handle,
            "avatar": abs(s(v, "/account/avatars/0/path")),
            "facts": facts,
        }),
    };
    Some((card, pick))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_paths_are_recognised() {
        let r = |u: &str| post_ref(&Url::parse(u).unwrap());
        assert_eq!(
            r("https://mastodon.social/@Gargron/109318821117356215"),
            Some(PostRef::Status("109318821117356215".into()))
        );
        assert_eq!(
            r("https://pixelfed.social/p/dansup/123456"),
            Some(PostRef::Status("123456".into()))
        );
        assert_eq!(
            r("https://misskey.io/notes/9abcdefghij"),
            Some(PostRef::Note("9abcdefghij".into()))
        );
        assert_eq!(
            r("https://lemmy.world/post/42"),
            Some(PostRef::LemmyPost("42".into()))
        );
        assert_eq!(
            r("https://kbin.social/m/tech/t/12/title"),
            Some(PostRef::Entry)
        );
        assert_eq!(
            r("https://peertube.tv/w/9c9de5e8TmB4cNxBbBDUKm"),
            Some(PostRef::PeerTubeVideo("9c9de5e8TmB4cNxBbBDUKm".into()))
        );
        assert_eq!(r("https://example.com/blog/hello"), None);
    }

    #[test]
    fn nodeinfo_detection() {
        let wk = json!({"links": [
            {"rel": "http://nodeinfo.diaspora.software/ns/schema/2.0", "href": "https://m.example/nodeinfo/2.0"},
            {"rel": "http://nodeinfo.diaspora.software/ns/schema/2.1", "href": "https://m.example/nodeinfo/2.1"},
            {"rel": "https://example.org/other", "href": "https://evil/"}
        ]});
        assert_eq!(
            nodeinfo_href(&wk).as_deref(),
            Some("https://m.example/nodeinfo/2.1")
        );
        let doc = json!({"version": "2.1", "software": {"name": "Sharkey", "version": "2024.1"}});
        let sw = software_of(&doc).unwrap();
        assert_eq!(sw, "sharkey");
        assert_eq!(family(&sw), Some("misskey"));
        assert_eq!(family("mastodon"), Some("mastodon"));
        assert_eq!(family("wordpress"), None);
        assert!(software_of(&json!({"software": {}})).is_none());
    }

    #[test]
    fn mastodon_status_fixture_parses() {
        let v = json!({
            "id": "1", "created_at": "2024-01-02T03:04:05.000Z",
            "content": "<p>Hello <a href=\"x\">#world</a></p><p>second &amp; last</p>",
            "spoiler_text": "",
            "replies_count": 2, "reblogs_count": 3, "favourites_count": 5,
            "account": {"acct": "alice", "username": "alice", "display_name": "Alice A",
                        "avatar": "https://m.example/a.png", "avatar_static": "https://m.example/a-static.png"},
            "media_attachments": [{"type": "image", "preview_url": "https://m.example/p1.jpg"}]
        });
        let c = mastodon_card(&v, "m.example").unwrap();
        assert_eq!(c.title.as_deref(), Some("Alice A"));
        let d = &c.details;
        assert_eq!(d["type"], "post");
        assert_eq!(d["handle"], "@alice@m.example");
        assert_eq!(d["avatar"], "https://m.example/a-static.png");
        assert_eq!(d["text"], "Hello #world second & last");
        assert_eq!(d["thumbs"], json!(["https://m.example/p1.jpg"]));
        assert_eq!(d["counts"]["likes"], 5);
        assert!(d["warning"].is_null());
    }

    #[test]
    fn peertube_picks_a_playable_file() {
        let v = json!({
            "name": "Talk", "duration": 125, "views": 9,
            "previewPath": "/lazy-static/previews/x.jpg",
            "account": {"name": "bob", "host": "pt.example", "displayName": "Bob"},
            "files": [],
            "streamingPlaylists": [{"files": [
                {"resolution": {"id": 1080}, "fileUrl": "https://pt.example/v-1080.mp4"},
                {"resolution": {"id": 480}, "fileUrl": "https://pt.example/v-480.mp4"},
                {"resolution": {"id": 720}, "fileUrl": "https://pt.example/v-720.mp4"}
            ]}]
        });
        let (card, video) = peertube_card(&v, "https://pt.example").unwrap();
        assert_eq!(
            video,
            Some(("https://pt.example/v-720.mp4".into(), "video/mp4".into()))
        );
        assert_eq!(
            card.image.as_deref(),
            Some("https://pt.example/lazy-static/previews/x.jpg")
        );
        assert_eq!(card.details["facts"], json!(["2:05", "9 views"]));
    }
}
