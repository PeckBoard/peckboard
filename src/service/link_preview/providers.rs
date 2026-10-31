//! The link-preview provider table: URL pattern → embed iframe template.
//!
//! Adding a provider is a data entry in [`PROVIDERS`] — no code. A URL
//! matches an entry when its host is in `hosts` and its path matches `path`
//! (named groups become captures); `query` lifts validated query parameters
//! into captures too. Templates substitute `{name}` (path-safe encoded) or
//! `{name|filter}` (filters: `seconds`); an embed `params` entry is dropped
//! when a capture it names is missing. The CSP `frame-src` allowlist is
//! derived from the embed templates' origins ([`frame_src_origins`]), so an
//! embed host can't be framed without being listed here.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// The embed is a cross-origin iframe of `iframe_src`, isolated by its
    /// own origin.
    Origin,
    /// The UI wraps `iframe_src` in a sandboxed `srcdoc` frame (opaque
    /// origin) — for widgets that otherwise want to script the host page.
    Srcdoc,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Origin => "origin",
            Self::Srcdoc => "srcdoc",
        }
    }
}

/// Extra lookups a provider needs before its templates can be filled, or to
/// build a richer card than the page's own meta tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enrich {
    None,
    /// Resolve the `handle` capture to a `did` (Bluesky embeds need a DID).
    BlueskyDid,
    GithubRepo,
    GithubPull,
    GithubIssue,
    GithubCommit,
    GithubRun,
}

impl Enrich {
    /// Captures this enrichment fills in (so a template may name them).
    fn fills(self) -> &'static [&'static str] {
        match self {
            Self::BlueskyDid => &["did"],
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QueryCap {
    pub param: &'static str,
    pub capture: &'static str,
    /// Anchored regex the raw value must match to be captured.
    pub pattern: &'static str,
}

pub struct EmbedSpec {
    /// `https://host/path/{capture}` — the iframe document URL.
    pub iframe: &'static str,
    /// Query parameters appended to `iframe`: `(key, template)`.
    pub params: &'static [(&'static str, &'static str)],
    pub sandbox: SandboxMode,
    pub aspect: Option<&'static str>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    /// Exact, lower-case hosts.
    pub hosts: &'static [&'static str],
    /// Regex over the URL path (named groups → captures).
    pub path: &'static str,
    pub query: &'static [QueryCap],
    pub embed: Option<EmbedSpec>,
    /// oEmbed endpoint for title/thumbnail metadata; `{url}` = the link.
    pub oembed: Option<&'static str>,
    pub enrich: Enrich,
}

// ── Table ─────────────────────────────────────────────────────────────

/// `/<owner>/<repo>` + `$suffix`, as one regex literal.
macro_rules! gh_path {
    ($suffix:literal) => {
        concat!(
            r"^/(?P<owner>[A-Za-z0-9][A-Za-z0-9-]{0,38})/(?P<repo>[A-Za-z0-9._-]{1,100})",
            $suffix
        )
    };
}

const YT_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
];
const YT_T: QueryCap = QueryCap {
    param: "t",
    capture: "t",
    pattern: r"^(?:[0-9]+s?|(?:[0-9]+h)?(?:[0-9]+m)?(?:[0-9]+s)?)$",
};
const YT_START: QueryCap = QueryCap {
    param: "start",
    capture: "t",
    pattern: r"^[0-9]+$",
};
const YT_EMBED: EmbedSpec = EmbedSpec {
    iframe: "https://www.youtube-nocookie.com/embed/{id}",
    params: &[("start", "{t|seconds}")],
    sandbox: SandboxMode::Origin,
    aspect: Some("16/9"),
    width: None,
    height: None,
};
const YT_OEMBED: Option<&str> = Some("https://www.youtube.com/oembed?format=json&url={url}");
const X_HOSTS: &[&str] = &[
    "twitter.com",
    "www.twitter.com",
    "mobile.twitter.com",
    "x.com",
    "www.x.com",
    "mobile.x.com",
];
const GH_HOSTS: &[&str] = &["github.com", "www.github.com"];

/// First match wins, so more specific entries come first.
pub static PROVIDERS: &[Provider] = &[
    Provider {
        id: "youtube",
        name: "YouTube",
        hosts: YT_HOSTS,
        path: r"^/watch/?$",
        query: &[
            QueryCap {
                param: "v",
                capture: "id",
                pattern: r"^[A-Za-z0-9_-]{11}$",
            },
            YT_T,
            YT_START,
        ],
        embed: Some(YT_EMBED),
        oembed: YT_OEMBED,
        enrich: Enrich::None,
    },
    Provider {
        id: "youtube",
        name: "YouTube",
        hosts: YT_HOSTS,
        path: r"^/(?:shorts|embed|live|v)/(?P<id>[A-Za-z0-9_-]{11})/?$",
        query: &[YT_T, YT_START],
        embed: Some(YT_EMBED),
        oembed: YT_OEMBED,
        enrich: Enrich::None,
    },
    Provider {
        id: "youtube",
        name: "YouTube",
        hosts: &["youtu.be"],
        path: r"^/(?P<id>[A-Za-z0-9_-]{11})/?$",
        query: &[YT_T, YT_START],
        embed: Some(YT_EMBED),
        oembed: YT_OEMBED,
        enrich: Enrich::None,
    },
    Provider {
        id: "x",
        name: "X",
        hosts: X_HOSTS,
        path: r"^/(?P<user>[A-Za-z0-9_]{1,15})/status(?:es)?/(?P<id>[0-9]{1,25})(?:/.*)?$",
        query: &[],
        embed: Some(EmbedSpec {
            iframe: "https://platform.twitter.com/embed/Tweet.html",
            params: &[("id", "{id}"), ("dnt", "true")],
            sandbox: SandboxMode::Srcdoc,
            aspect: None,
            width: Some(550),
            height: None,
        }),
        oembed: Some("https://publish.twitter.com/oembed?omit_script=1&dnt=true&url={url}"),
        enrich: Enrich::None,
    },
    Provider {
        id: "bluesky",
        name: "Bluesky",
        hosts: &["bsky.app"],
        path: r"^/profile/(?P<handle>[A-Za-z0-9._:-]{1,253})/post/(?P<rkey>[A-Za-z0-9]{1,64})/?$",
        query: &[],
        embed: Some(EmbedSpec {
            iframe: "https://embed.bsky.app/embed/{did}/app.bsky.feed.post/{rkey}",
            params: &[],
            sandbox: SandboxMode::Origin,
            aspect: None,
            width: Some(600),
            height: None,
        }),
        oembed: Some("https://embed.bsky.app/oembed?format=json&url={url}"),
        enrich: Enrich::BlueskyDid,
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/pull/(?P<num>[0-9]{1,10})(?:/.*)?$"),
        query: &[],
        embed: None,
        oembed: None,
        enrich: Enrich::GithubPull,
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/issues/(?P<num>[0-9]{1,10})/?$"),
        query: &[],
        embed: None,
        oembed: None,
        enrich: Enrich::GithubIssue,
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/commit/(?P<sha>[0-9a-fA-F]{7,40})/?$"),
        query: &[],
        embed: None,
        oembed: None,
        enrich: Enrich::GithubCommit,
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/actions/runs/(?P<run>[0-9]{1,20})(?:/.*)?$"),
        query: &[],
        embed: None,
        oembed: None,
        enrich: Enrich::GithubRun,
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/?$"),
        query: &[],
        embed: None,
        oembed: None,
        enrich: Enrich::GithubRepo,
    },
];

// ── Matching ──────────────────────────────────────────────────────────

struct Compiled {
    path: Regex,
    query: Vec<Regex>,
}

static COMPILED: LazyLock<Vec<Compiled>> = LazyLock::new(|| {
    PROVIDERS
        .iter()
        .map(|p| Compiled {
            path: Regex::new(p.path).expect("provider path regex"),
            query: p
                .query
                .iter()
                .map(|q| Regex::new(q.pattern).expect("provider query regex"))
                .collect(),
        })
        .collect()
});

static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([a-z_]+)(?:\|([a-z_]+))?\}").unwrap());

#[derive(Clone)]
pub struct ProviderMatch {
    pub provider: &'static Provider,
    pub caps: BTreeMap<String, String>,
}

pub fn match_url(url: &Url) -> Option<ProviderMatch> {
    let host = url.host_str()?.to_ascii_lowercase();
    let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    for (p, c) in PROVIDERS.iter().zip(COMPILED.iter()) {
        if !p.hosts.contains(&host.as_str()) {
            continue;
        }
        let Some(m) = c.path.captures(url.path()) else {
            continue;
        };
        let mut caps = BTreeMap::new();
        for name in c.path.capture_names().flatten() {
            if let Some(v) = m.name(name) {
                caps.insert(name.to_string(), v.as_str().to_string());
            }
        }
        for (q, re) in p.query.iter().zip(&c.query) {
            if caps.contains_key(q.capture) {
                continue;
            }
            if let Some((_, v)) = pairs.iter().find(|(k, _)| k == q.param)
                && re.is_match(v)
            {
                caps.insert(q.capture.to_string(), v.clone());
            }
        }
        // The iframe path must be fillable (e.g. `/watch` without `v`).
        if let Some(e) = &p.embed {
            let fillable = PLACEHOLDER_RE
                .captures_iter(e.iframe)
                .all(|ph| caps.contains_key(&ph[1]) || p.enrich.fills().contains(&&ph[1]));
            if !fillable {
                continue;
            }
        }
        return Some(ProviderMatch { provider: p, caps });
    }
    None
}

/// Percent-encode for a path segment or query value (keeps `:` and `@`,
/// which DIDs and handles use).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':' | b'@') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `90`, `90s`, `1h2m3s`, `2m` → seconds.
fn seconds(raw: &str) -> Option<String> {
    if let Ok(n) = raw.trim_end_matches('s').parse::<u64>() {
        return Some(n.to_string());
    }
    let mut total = 0u64;
    let mut num = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        total += n * match ch {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
    }
    num.is_empty().then(|| total.to_string())
}

/// Fill `{name}` / `{name|filter}`; `None` when a capture is missing or a
/// filter rejects its value.
pub fn render(template: &str, caps: &BTreeMap<String, String>) -> Option<String> {
    let mut out = String::new();
    let mut last = 0;
    for ph in PLACEHOLDER_RE.captures_iter(template) {
        let whole = ph.get(0).unwrap();
        out.push_str(&template[last..whole.start()]);
        let value = caps.get(&ph[1])?;
        let value = match ph.get(2).map(|f| f.as_str()) {
            None => value.clone(),
            Some("seconds") => seconds(value)?,
            Some(_) => return None,
        };
        out.push_str(&encode(&value));
        last = whole.end();
    }
    out.push_str(&template[last..]);
    Some(out)
}

/// The embed iframe URL for a match, once enrichment has filled its captures.
pub fn embed_src(spec: &EmbedSpec, caps: &BTreeMap<String, String>) -> Option<String> {
    let mut src = render(spec.iframe, caps)?;
    let params: Vec<String> = spec
        .params
        .iter()
        .filter_map(|(k, t)| render(t, caps).map(|v| format!("{k}={v}")))
        .collect();
    if !params.is_empty() {
        src.push(if src.contains('?') { '&' } else { '?' });
        src.push_str(&params.join("&"));
    }
    Some(src)
}

/// `https://host` of every embed template — the CSP `frame-src` allowlist.
pub fn frame_src_origins() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in PROVIDERS {
        let Some(e) = &p.embed else { continue };
        let Some((scheme, rest)) = e.iframe.split_once("://") else {
            continue;
        };
        let host = rest.split(['/', '?']).next().unwrap_or("");
        let origin = format!("{scheme}://{host}");
        if !out.contains(&origin) {
            out.push(origin);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(raw: &str) -> Option<ProviderMatch> {
        match_url(&Url::parse(raw).unwrap())
    }

    fn src(raw: &str) -> Option<String> {
        let pm = m(raw)?;
        embed_src(pm.provider.embed.as_ref()?, &pm.caps)
    }

    #[test]
    fn youtube_variants_map_to_nocookie_embeds_with_start_time() {
        let want = "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ";
        assert_eq!(
            src("https://www.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(),
            Some(want)
        );
        assert_eq!(
            src("https://youtube.com/watch?t=1m30s&v=dQw4w9WgXcQ&list=x").as_deref(),
            Some(&*format!("{want}?start=90"))
        );
        assert_eq!(
            src("https://m.youtube.com/watch?v=dQw4w9WgXcQ&t=1h2m3s").as_deref(),
            Some(&*format!("{want}?start=3723"))
        );
        assert_eq!(
            src("https://youtu.be/dQw4w9WgXcQ?t=42").as_deref(),
            Some(&*format!("{want}?start=42"))
        );
        assert_eq!(
            src("https://www.youtube.com/shorts/dQw4w9WgXcQ").as_deref(),
            Some(want)
        );
        assert_eq!(
            src("https://www.youtube.com/watch?v=dQw4w9WgXcQ&start=15").as_deref(),
            Some(&*format!("{want}?start=15"))
        );
        // A malformed timestamp is dropped, not passed through.
        assert_eq!(
            src("https://youtu.be/dQw4w9WgXcQ?t=abc").as_deref(),
            Some(want)
        );
        assert!(m("https://www.youtube.com/watch?v=short").is_none());
        assert!(m("https://www.youtube.com/feed/trending").is_none());
        assert!(m("https://evil-youtube.com/watch?v=dQw4w9WgXcQ").is_none());
        let pm = m("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let e = pm.provider.embed.as_ref().unwrap();
        assert_eq!(e.sandbox, SandboxMode::Origin);
        assert_eq!(e.aspect, Some("16/9"));
    }

    #[test]
    fn tweets_embed_through_srcdoc() {
        for raw in [
            "https://twitter.com/jack/status/20",
            "https://x.com/jack/status/20?s=46",
            "https://mobile.twitter.com/jack/status/20/photo/1",
        ] {
            let pm = m(raw).unwrap_or_else(|| panic!("{raw}"));
            assert_eq!(pm.provider.id, "x");
            let e = pm.provider.embed.as_ref().unwrap();
            assert_eq!(e.sandbox, SandboxMode::Srcdoc);
            assert_eq!(
                embed_src(e, &pm.caps).as_deref(),
                Some("https://platform.twitter.com/embed/Tweet.html?id=20&dnt=true")
            );
        }
        assert!(m("https://x.com/jack").is_none());
    }

    #[test]
    fn bluesky_posts_need_a_did() {
        let pm = m("https://bsky.app/profile/alice.bsky.social/post/3kabc123").unwrap();
        assert_eq!(pm.provider.enrich, Enrich::BlueskyDid);
        assert_eq!(pm.caps["handle"], "alice.bsky.social");
        let e = pm.provider.embed.as_ref().unwrap();
        // Unfillable until enrichment resolves the handle …
        assert!(embed_src(e, &pm.caps).is_none());
        let mut caps = pm.caps.clone();
        caps.insert("did".into(), "did:plc:abc123".into());
        assert_eq!(
            embed_src(e, &caps).as_deref(),
            Some("https://embed.bsky.app/embed/did:plc:abc123/app.bsky.feed.post/3kabc123")
        );
    }

    #[test]
    fn github_urls_pick_the_right_card() {
        let pm = m("https://github.com/rust-lang/rust/pull/12345/files").unwrap();
        assert_eq!(pm.provider.enrich, Enrich::GithubPull);
        assert_eq!(
            (
                pm.caps["owner"].as_str(),
                pm.caps["repo"].as_str(),
                pm.caps["num"].as_str()
            ),
            ("rust-lang", "rust", "12345")
        );
        assert!(pm.provider.embed.is_none());
        assert_eq!(
            m("https://github.com/o/r/issues/7")
                .unwrap()
                .provider
                .enrich,
            Enrich::GithubIssue
        );
        assert_eq!(
            m("https://github.com/o/r/commit/abc1234")
                .unwrap()
                .provider
                .enrich,
            Enrich::GithubCommit
        );
        assert_eq!(
            m("https://github.com/o/r/actions/runs/99/job/1")
                .unwrap()
                .provider
                .enrich,
            Enrich::GithubRun
        );
        assert_eq!(
            m("https://github.com/o/r").unwrap().provider.enrich,
            Enrich::GithubRepo
        );
        assert!(m("https://github.com/o/r/tree/main/src").is_none());
    }

    #[test]
    fn frame_src_is_exactly_the_embed_origins() {
        assert_eq!(
            frame_src_origins(),
            vec![
                "https://www.youtube-nocookie.com",
                "https://platform.twitter.com",
                "https://embed.bsky.app",
            ]
        );
    }
}
