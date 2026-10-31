//! The link-preview provider table: URL pattern → embed iframe template.
//!
//! Adding a provider is a data entry in [`PROVIDERS`] — no code. A URL
//! matches an entry when its host is in `hosts` (`*.example.com` matches any
//! subdomain) and its path matches `path` (named groups become captures);
//! `query` lifts validated query parameters into captures too, and every
//! match also captures `host` and `url` (the link itself). Templates
//! substitute `{name}` (path-safe encoded) or `{name|filter}` (filters:
//! `seconds`, `query` — encode every reserved byte, `path` — keep `/`
//! between encoded segments, `subdomain` — a DNS label); an embed `params`
//! entry is dropped when a capture it names is missing. [`PARENT_HOST`] in a
//! template is left for the browser to replace with the page's own host name
//! (Twitch, Calendly want it).
//!
//! An entry may instead (or also) name a direct `media` file, an API
//! enrichment, or just tag a site so its scraped card carries the provider
//! (`..BASE` with a catch-all `path`). The CSP `frame-src` allowlist is
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
    /// origin) — for widgets that only ship as a script that wants to script
    /// the host page. Every provider below has a plain iframe document URL,
    /// so none needs it today; X used to (its frame then lost its own
    /// origin and broke).
    #[allow(dead_code)]
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
    /// `bbox` / `marker` from `mlat`+`mlon` or the `#map=` fragment.
    Osm,
    /// The page's `og:video` names the embed player (Bandcamp).
    OgEmbed,
    GithubRepo,
    GithubPull,
    GithubIssue,
    GithubCommit,
    GithubRun,
    Gist,
    GitlabRepo,
    GitlabMerge,
    GitlabIssue,
    GiteaRepo,
    GiteaPull,
    GiteaIssue,
    BitbucketRepo,
    BitbucketPull,
    DockerHub,
    HuggingFace,
    Npm,
    Crates,
    Pypi,
    StackExchange,
    HackerNews,
    Wikipedia,
    Arxiv,
    Steam,
    PolyHaven,
    /// Token-gated (admin viewers only, else the page's own card).
    Linear,
    Jira,
    Trello,
    Sentry,
}

impl Enrich {
    /// Captures this enrichment fills in (so a template may name them).
    fn fills(self) -> &'static [&'static str] {
        match self {
            Self::BlueskyDid => &["did"],
            Self::Osm => &["bbox", "marker"],
            Self::OgEmbed => &["player"],
            _ => &[],
        }
    }

    pub fn is_github(self) -> bool {
        matches!(
            self,
            Self::GithubRepo
                | Self::GithubPull
                | Self::GithubIssue
                | Self::GithubCommit
                | Self::GithubRun
        )
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

/// The link is (or names) a media file: preview it as `media` of `kind`.
pub struct MediaSpec {
    pub src: &'static str,
    /// `image` | `video` | `audio` | `model`.
    pub kind: &'static str,
}

/// How a scraped card for this provider is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardHint {
    Plain,
    /// A 3D model / game asset page: `details.type = "asset"`, plus a
    /// direct model file link when the page has one.
    Asset,
    /// A shop that often refuses bots: a refused fetch still yields a
    /// plain card (or the screenshot fallback) instead of no preview.
    Shop,
}

pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    /// Lower-case hosts; `*.example.com` matches every subdomain.
    pub hosts: &'static [&'static str],
    /// Regex over the URL path (named groups → captures).
    pub path: &'static str,
    pub query: &'static [QueryCap],
    pub embed: Option<EmbedSpec>,
    /// oEmbed endpoint for title/thumbnail metadata; `{url}` = the link.
    pub oembed: Option<&'static str>,
    pub enrich: Enrich,
    pub media: Option<MediaSpec>,
    pub card: CardHint,
}

/// Placeholder the browser swaps for `window.location.hostname`.
pub const PARENT_HOST: &str = "__PARENT_HOST__";

// ── Table ─────────────────────────────────────────────────────────────

/// A tag-only entry: every field off. Entries override what they need.
const BASE: Provider = Provider {
    id: "",
    name: "",
    hosts: &[],
    path: r"^/",
    query: &[],
    embed: None,
    oembed: None,
    enrich: Enrich::None,
    media: None,
    card: CardHint::Plain,
};

/// 16:9 player.
const fn video(iframe: &'static str, params: &'static [(&'static str, &'static str)]) -> EmbedSpec {
    ratio(iframe, params, "16/9")
}

const fn ratio(
    iframe: &'static str,
    params: &'static [(&'static str, &'static str)],
    aspect: &'static str,
) -> EmbedSpec {
    EmbedSpec {
        iframe,
        params,
        sandbox: SandboxMode::Origin,
        aspect: Some(aspect),
        width: None,
        height: None,
    }
}

/// Full width, fixed height (audio players, widgets, documents).
const fn sized(
    iframe: &'static str,
    params: &'static [(&'static str, &'static str)],
    height: u32,
) -> EmbedSpec {
    EmbedSpec {
        iframe,
        params,
        sandbox: SandboxMode::Origin,
        aspect: None,
        width: None,
        height: Some(height),
    }
}

/// A social post: capped width, height reported by the frame.
const fn post(
    iframe: &'static str,
    params: &'static [(&'static str, &'static str)],
    width: u32,
) -> EmbedSpec {
    EmbedSpec {
        iframe,
        params,
        sandbox: SandboxMode::Origin,
        aspect: None,
        width: Some(width),
        height: None,
    }
}

/// `/<owner>/<repo>` + `$suffix`, as one regex literal.
macro_rules! gh_path {
    ($suffix:literal) => {
        concat!(
            r"^/(?P<owner>[A-Za-z0-9][A-Za-z0-9-]{0,38})/(?P<repo>[A-Za-z0-9._-]{1,100})",
            $suffix
        )
    };
}

/// `/<owner>/<repo>` for forges with looser owner names + `$suffix`.
macro_rules! forge_path {
    ($suffix:literal) => {
        concat!(
            r"^/(?P<owner>[A-Za-z0-9_.-]{1,100})/(?P<repo>[A-Za-z0-9_.-]{1,100})",
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
const TWITCH_HOSTS: &[&str] = &["twitch.tv", "www.twitch.tv", "m.twitch.tv"];
const PARENT: (&str, &str) = ("parent", PARENT_HOST);
const SPOTIFY_OEMBED: Option<&str> = Some("https://open.spotify.com/oembed?url={url}");
const SOUNDCLOUD_HOSTS: &[&str] = &["soundcloud.com", "www.soundcloud.com", "m.soundcloud.com"];
const SOUNDCLOUD_OEMBED: Option<&str> = Some("https://soundcloud.com/oembed?format=json&url={url}");
const APPLE_I: QueryCap = QueryCap {
    param: "i",
    capture: "i",
    pattern: r"^[0-9]{1,15}$",
};
const FB_HOSTS: &[&str] = &["facebook.com", "www.facebook.com", "m.facebook.com"];
const GITLAB_HOSTS: &[&str] = &["gitlab.com", "www.gitlab.com"];
const GITEA_HOSTS: &[&str] = &["codeberg.org", "gitea.com", "code.forgejo.org"];
const HF_HOSTS: &[&str] = &["huggingface.co", "hf.co"];
const SE_HOSTS: &[&str] = &[
    "stackoverflow.com",
    "superuser.com",
    "serverfault.com",
    "askubuntu.com",
    "mathoverflow.net",
    "*.stackexchange.com",
];
const GMAPS_HOSTS: &[&str] = &["google.com", "www.google.com", "maps.google.com"];
const SHOP_AMAZON: &[&str] = &[
    "amazon.com",
    "*.amazon.com",
    "amazon.co.uk",
    "*.amazon.co.uk",
    "amazon.de",
    "*.amazon.de",
    "amazon.fr",
    "*.amazon.fr",
    "amazon.it",
    "*.amazon.it",
    "amazon.es",
    "*.amazon.es",
    "amazon.ca",
    "*.amazon.ca",
    "amazon.co.jp",
    "*.amazon.co.jp",
    "amazon.in",
    "*.amazon.in",
    "amazon.com.au",
    "*.amazon.com.au",
    "amazon.nl",
    "*.amazon.nl",
    "amazon.com.br",
    "*.amazon.com.br",
    "amazon.com.mx",
    "*.amazon.com.mx",
    "a.co",
    "amzn.to",
    "amzn.eu",
];

/// First match wins, so more specific entries come first.
pub static PROVIDERS: &[Provider] = &[
    // ── Video ─────────────────────────────────────────────────────────
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
        ..BASE
    },
    Provider {
        id: "youtube",
        name: "YouTube",
        hosts: YT_HOSTS,
        path: r"^/(?:shorts|embed|live|v)/(?P<id>[A-Za-z0-9_-]{11})/?$",
        query: &[YT_T, YT_START],
        embed: Some(YT_EMBED),
        oembed: YT_OEMBED,
        ..BASE
    },
    Provider {
        id: "youtube",
        name: "YouTube",
        hosts: &["youtu.be"],
        path: r"^/(?P<id>[A-Za-z0-9_-]{11})/?$",
        query: &[YT_T, YT_START],
        embed: Some(YT_EMBED),
        oembed: YT_OEMBED,
        ..BASE
    },
    // YouTube (Music) playlists / albums.
    Provider {
        id: "youtube",
        name: "YouTube",
        hosts: YT_HOSTS,
        path: r"^/playlist/?$",
        query: &[QueryCap {
            param: "list",
            capture: "list",
            pattern: r"^[A-Za-z0-9_-]{10,64}$",
        }],
        embed: Some(video(
            "https://www.youtube-nocookie.com/embed/videoseries?list={list}",
            &[],
        )),
        oembed: YT_OEMBED,
        ..BASE
    },
    Provider {
        id: "vimeo",
        name: "Vimeo",
        hosts: &["vimeo.com", "www.vimeo.com"],
        path: r"^/(?:channels/[A-Za-z0-9_-]{1,64}/|groups/[A-Za-z0-9_-]{1,64}/videos/)?(?P<id>[0-9]{5,12})(?:/(?P<h>[0-9a-f]{6,20}))?/?$",
        embed: Some(video(
            "https://player.vimeo.com/video/{id}",
            &[("h", "{h}"), ("dnt", "1")],
        )),
        oembed: Some("https://vimeo.com/api/oembed.json?url={url}"),
        ..BASE
    },
    Provider {
        id: "vimeo",
        name: "Vimeo",
        hosts: &["player.vimeo.com"],
        path: r"^/video/(?P<id>[0-9]{5,12})/?$",
        embed: Some(video(
            "https://player.vimeo.com/video/{id}",
            &[("dnt", "1")],
        )),
        ..BASE
    },
    Provider {
        id: "twitch",
        name: "Twitch",
        hosts: &["clips.twitch.tv"],
        path: r"^/(?P<clip>[A-Za-z0-9_-]{4,100})/?$",
        embed: Some(video(
            "https://clips.twitch.tv/embed",
            &[("clip", "{clip}"), PARENT, ("autoplay", "false")],
        )),
        ..BASE
    },
    Provider {
        id: "twitch",
        name: "Twitch",
        hosts: TWITCH_HOSTS,
        path: r"^/[A-Za-z0-9_]{3,25}/clip/(?P<clip>[A-Za-z0-9_-]{4,100})/?$",
        embed: Some(video(
            "https://clips.twitch.tv/embed",
            &[("clip", "{clip}"), PARENT, ("autoplay", "false")],
        )),
        ..BASE
    },
    Provider {
        id: "twitch",
        name: "Twitch",
        hosts: TWITCH_HOSTS,
        path: r"^/videos/(?P<id>[0-9]{5,15})/?$",
        embed: Some(video(
            "https://player.twitch.tv/",
            &[("video", "v{id}"), PARENT, ("autoplay", "false")],
        )),
        ..BASE
    },
    Provider {
        id: "twitch",
        name: "Twitch",
        hosts: TWITCH_HOSTS,
        path: r"^/(?P<channel>[A-Za-z0-9_]{3,25})/?$",
        embed: Some(video(
            "https://player.twitch.tv/",
            &[("channel", "{channel}"), PARENT, ("autoplay", "false")],
        )),
        ..BASE
    },
    Provider {
        id: "kick",
        name: "Kick",
        hosts: &["kick.com", "www.kick.com"],
        path: r"^/(?P<channel>[A-Za-z0-9_-]{3,25})/?$",
        embed: Some(video(
            "https://player.kick.com/{channel}",
            &[("autoplay", "false")],
        )),
        ..BASE
    },
    Provider {
        id: "kick",
        name: "Kick",
        hosts: &["kick.com", "www.kick.com"],
        ..BASE
    },
    Provider {
        id: "rumble",
        name: "Rumble",
        hosts: &["rumble.com", "www.rumble.com"],
        path: r"^/embed/(?P<id>v[a-z0-9]{3,12})/?$",
        embed: Some(video("https://rumble.com/embed/{id}/", &[])),
        ..BASE
    },
    Provider {
        id: "rumble",
        name: "Rumble",
        hosts: &["rumble.com", "www.rumble.com"],
        ..BASE
    },
    Provider {
        id: "odysee",
        name: "Odysee",
        hosts: &["odysee.com"],
        path: r"^/(?P<claim>@[A-Za-z0-9_.:-]{1,200}/[A-Za-z0-9_.:-]{1,200})$",
        embed: Some(video("https://odysee.com/$/embed/{claim|path}", &[])),
        ..BASE
    },
    Provider {
        id: "loom",
        name: "Loom",
        hosts: &["loom.com", "www.loom.com"],
        path: r"^/share/(?P<id>[0-9a-f]{32})/?$",
        embed: Some(video("https://www.loom.com/embed/{id}", &[])),
        oembed: Some("https://www.loom.com/v1/oembed?url={url}"),
        ..BASE
    },
    Provider {
        id: "dailymotion",
        name: "Dailymotion",
        hosts: &["dailymotion.com", "www.dailymotion.com"],
        path: r"^/video/(?P<id>[A-Za-z0-9]{5,12})/?$",
        embed: Some(video("https://www.dailymotion.com/embed/video/{id}", &[])),
        oembed: Some("https://www.dailymotion.com/services/oembed?url={url}"),
        ..BASE
    },
    Provider {
        id: "dailymotion",
        name: "Dailymotion",
        hosts: &["dai.ly"],
        path: r"^/(?P<id>[A-Za-z0-9]{5,12})/?$",
        embed: Some(video("https://www.dailymotion.com/embed/video/{id}", &[])),
        ..BASE
    },
    Provider {
        id: "streamable",
        name: "Streamable",
        hosts: &["streamable.com", "www.streamable.com"],
        path: r"^/(?:e/)?(?P<id>[a-z0-9]{4,10})/?$",
        embed: Some(video("https://streamable.com/e/{id}", &[])),
        oembed: Some("https://api.streamable.com/oembed.json?url={url}"),
        ..BASE
    },
    // ── Audio ─────────────────────────────────────────────────────────
    Provider {
        id: "spotify",
        name: "Spotify",
        hosts: &["open.spotify.com"],
        path: r"^/(?:intl-[a-z]{2}(?:-[A-Za-z]{2})?/)?(?P<type>track|episode)/(?P<id>[A-Za-z0-9]{22})/?$",
        embed: Some(sized(
            "https://open.spotify.com/embed/{type}/{id}",
            &[],
            152,
        )),
        oembed: SPOTIFY_OEMBED,
        ..BASE
    },
    Provider {
        id: "spotify",
        name: "Spotify",
        hosts: &["open.spotify.com"],
        path: r"^/(?:intl-[a-z]{2}(?:-[A-Za-z]{2})?/)?(?P<type>album|playlist|show|artist)/(?P<id>[A-Za-z0-9]{22})/?$",
        embed: Some(sized(
            "https://open.spotify.com/embed/{type}/{id}",
            &[],
            352,
        )),
        oembed: SPOTIFY_OEMBED,
        ..BASE
    },
    Provider {
        id: "soundcloud",
        name: "SoundCloud",
        hosts: SOUNDCLOUD_HOSTS,
        path: r"^/[A-Za-z0-9_-]{2,64}/sets/[A-Za-z0-9_-]{1,200}/?$",
        embed: Some(sized(
            "https://w.soundcloud.com/player/",
            &[("url", "{url|query}"), ("visual", "false")],
            450,
        )),
        oembed: SOUNDCLOUD_OEMBED,
        ..BASE
    },
    Provider {
        id: "soundcloud",
        name: "SoundCloud",
        hosts: SOUNDCLOUD_HOSTS,
        path: r"^/[A-Za-z0-9_-]{2,64}/[A-Za-z0-9_-]{1,200}/?$",
        embed: Some(sized(
            "https://w.soundcloud.com/player/",
            &[("url", "{url|query}"), ("visual", "false")],
            166,
        )),
        oembed: SOUNDCLOUD_OEMBED,
        ..BASE
    },
    Provider {
        id: "bandcamp",
        name: "Bandcamp",
        hosts: &["*.bandcamp.com"],
        path: r"^/(?:album|track)/[A-Za-z0-9_-]{1,200}/?$",
        embed: Some(sized(
            "https://bandcamp.com/EmbeddedPlayer/{player|path}",
            &[],
            120,
        )),
        enrich: Enrich::OgEmbed,
        ..BASE
    },
    Provider {
        id: "mixcloud",
        name: "Mixcloud",
        hosts: &["mixcloud.com", "www.mixcloud.com", "m.mixcloud.com"],
        path: r"^/(?P<user>[A-Za-z0-9_-]{1,64})/(?P<show>[A-Za-z0-9_-]{1,200})/?$",
        embed: Some(sized(
            "https://www.mixcloud.com/widget/iframe/",
            &[("feed", "%2F{user}%2F{show}%2F"), ("hide_cover", "1")],
            120,
        )),
        oembed: Some("https://www.mixcloud.com/oembed/?format=json&url={url}"),
        ..BASE
    },
    Provider {
        id: "audiomack",
        name: "Audiomack",
        hosts: &["audiomack.com", "www.audiomack.com"],
        path: r"^/(?P<artist>[A-Za-z0-9_-]{1,64})/(?P<type>song|album|playlist)/(?P<slug>[A-Za-z0-9_-]{1,200})/?$",
        embed: Some(sized(
            "https://audiomack.com/embed/{type}/{artist}/{slug}",
            &[],
            252,
        )),
        ..BASE
    },
    Provider {
        id: "deezer",
        name: "Deezer",
        hosts: &["deezer.com", "www.deezer.com"],
        path: r"^/(?:[a-z]{2}/)?(?P<type>track|album|playlist|episode|show|artist)/(?P<id>[0-9]{1,15})/?$",
        embed: Some(sized(
            "https://widget.deezer.com/widget/auto/{type}/{id}",
            &[],
            300,
        )),
        ..BASE
    },
    Provider {
        id: "tidal",
        name: "TIDAL",
        hosts: &["tidal.com", "listen.tidal.com"],
        path: r"^/(?:browse/)?(?P<type>track|album|playlist|video)/(?P<id>[A-Za-z0-9-]{1,40})/?$",
        embed: Some(sized("https://embed.tidal.com/{type}s/{id}", &[], 352)),
        ..BASE
    },
    Provider {
        id: "apple-music",
        name: "Apple Music",
        hosts: &["music.apple.com"],
        path: r"^/(?P<cc>[a-z]{2})/(?P<type>song)/(?P<slug>[A-Za-z0-9_.-]{1,200})/(?P<id>[0-9]{1,15})/?$",
        embed: Some(sized(
            "https://embed.music.apple.com/{cc}/{type}/{slug}/{id}",
            &[],
            175,
        )),
        ..BASE
    },
    Provider {
        id: "apple-music",
        name: "Apple Music",
        hosts: &["music.apple.com"],
        path: r"^/(?P<cc>[a-z]{2})/(?P<type>album|playlist|station|music-video)/(?P<slug>[A-Za-z0-9_.-]{1,200})/(?P<id>[A-Za-z0-9.-]{1,64})/?$",
        query: &[APPLE_I],
        embed: Some(sized(
            "https://embed.music.apple.com/{cc}/{type}/{slug}/{id}",
            &[("i", "{i}")],
            450,
        )),
        ..BASE
    },
    Provider {
        id: "apple-podcasts",
        name: "Apple Podcasts",
        hosts: &["podcasts.apple.com"],
        path: r"^/(?P<cc>[a-z]{2})/podcast/(?P<slug>[A-Za-z0-9_.-]{1,200})/id(?P<id>[0-9]{1,15})/?$",
        query: &[APPLE_I],
        embed: Some(sized(
            "https://embed.podcasts.apple.com/{cc}/podcast/{slug}/id{id}",
            &[("i", "{i}")],
            450,
        )),
        ..BASE
    },
    Provider {
        id: "pocketcasts",
        name: "Pocket Casts",
        hosts: &["pca.st", "pocketcasts.com", "play.pocketcasts.com"],
        ..BASE
    },
    Provider {
        id: "overcast",
        name: "Overcast",
        hosts: &["overcast.fm"],
        ..BASE
    },
    // ── Social ────────────────────────────────────────────────────────
    Provider {
        id: "x",
        name: "X",
        hosts: X_HOSTS,
        path: r"^/(?P<user>[A-Za-z0-9_]{1,15})/status(?:es)?/(?P<id>[0-9]{1,25})(?:/.*)?$",
        // A cross-origin document URL: `allow-same-origin` grants it only
        // its own (twitter) origin, which the widget needs to run.
        embed: Some(post(
            "https://platform.twitter.com/embed/Tweet.html",
            &[("id", "{id}"), ("dnt", "true")],
            550,
        )),
        oembed: Some("https://publish.twitter.com/oembed?omit_script=1&dnt=true&url={url}"),
        ..BASE
    },
    Provider {
        id: "bluesky",
        name: "Bluesky",
        hosts: &["bsky.app"],
        path: r"^/profile/(?P<handle>[A-Za-z0-9._:-]{1,253})/post/(?P<rkey>[A-Za-z0-9]{1,64})/?$",
        embed: Some(post(
            "https://embed.bsky.app/embed/{did}/app.bsky.feed.post/{rkey}",
            &[],
            600,
        )),
        oembed: Some("https://embed.bsky.app/oembed?format=json&url={url}"),
        enrich: Enrich::BlueskyDid,
        ..BASE
    },
    Provider {
        id: "threads",
        name: "Threads",
        hosts: &[
            "threads.net",
            "www.threads.net",
            "threads.com",
            "www.threads.com",
        ],
        path: r"^/@(?P<user>[A-Za-z0-9_.]{1,30})/post/(?P<id>[A-Za-z0-9_-]{5,40})/?$",
        embed: Some(post(
            "https://www.threads.net/@{user}/post/{id}/embed",
            &[],
            550,
        )),
        ..BASE
    },
    Provider {
        id: "instagram",
        name: "Instagram",
        hosts: &["instagram.com", "www.instagram.com"],
        path: r"^/(?:[A-Za-z0-9_.]{1,30}/)?(?P<kind>p|reel|tv)/(?P<id>[A-Za-z0-9_-]{5,40})/?$",
        embed: Some(post(
            "https://www.instagram.com/{kind}/{id}/embed/captioned/",
            &[],
            540,
        )),
        ..BASE
    },
    Provider {
        id: "tiktok",
        name: "TikTok",
        hosts: &["tiktok.com", "www.tiktok.com", "m.tiktok.com"],
        path: r"^/@[A-Za-z0-9_.]{1,30}/video/(?P<id>[0-9]{10,25})/?$",
        embed: Some(EmbedSpec {
            iframe: "https://www.tiktok.com/player/v1/{id}",
            params: &[("autoplay", "0")],
            sandbox: SandboxMode::Origin,
            aspect: Some("9/16"),
            width: Some(340),
            height: None,
        }),
        oembed: Some("https://www.tiktok.com/oembed?url={url}"),
        ..BASE
    },
    Provider {
        id: "reddit",
        name: "Reddit",
        hosts: &[
            "reddit.com",
            "www.reddit.com",
            "old.reddit.com",
            "new.reddit.com",
        ],
        path: r"^/r/(?P<sub>[A-Za-z0-9_]{2,21})/comments/(?P<id>[a-z0-9]{4,10})(?:/[A-Za-z0-9_-]*)?/?$",
        embed: Some(post(
            "https://embed.reddit.com/r/{sub}/comments/{id}/",
            &[],
            640,
        )),
        oembed: Some("https://www.reddit.com/oembed?url={url}"),
        ..BASE
    },
    Provider {
        id: "linkedin",
        name: "LinkedIn",
        hosts: &["linkedin.com", "www.linkedin.com"],
        path: r"^/feed/update/(?P<urn>urn:li:(?:activity|share|ugcPost):[0-9]{10,25})/?$",
        embed: Some(post(
            "https://www.linkedin.com/embed/feed/update/{urn}",
            &[],
            504,
        )),
        ..BASE
    },
    Provider {
        id: "linkedin",
        name: "LinkedIn",
        hosts: &["linkedin.com", "www.linkedin.com"],
        ..BASE
    },
    // Facebook's plugin iframes need no app token (its oEmbed API does).
    Provider {
        id: "facebook",
        name: "Facebook",
        hosts: FB_HOSTS,
        path: r"^/[A-Za-z0-9.]{1,80}/posts/[A-Za-z0-9]{5,80}/?$",
        embed: Some(post(
            "https://www.facebook.com/plugins/post.php",
            &[("href", "{url|query}"), ("show_text", "true")],
            500,
        )),
        ..BASE
    },
    Provider {
        id: "facebook",
        name: "Facebook",
        hosts: FB_HOSTS,
        path: r"^/[A-Za-z0-9.]{1,80}/videos/[0-9]{5,25}/?$",
        embed: Some(video(
            "https://www.facebook.com/plugins/video.php",
            &[("href", "{url|query}"), ("show_text", "false")],
        )),
        ..BASE
    },
    Provider {
        id: "facebook",
        name: "Facebook",
        hosts: FB_HOSTS,
        ..BASE
    },
    Provider {
        id: "pinterest",
        name: "Pinterest",
        hosts: &["pinterest.com", "*.pinterest.com"],
        path: r"^/pin/(?P<id>[0-9]{5,25})/?$",
        embed: Some(EmbedSpec {
            iframe: "https://assets.pinterest.com/ext/embed.html",
            params: &[("id", "{id}")],
            sandbox: SandboxMode::Origin,
            aspect: None,
            width: Some(345),
            height: Some(600),
        }),
        ..BASE
    },
    Provider {
        id: "telegram",
        name: "Telegram",
        hosts: &["t.me", "telegram.me"],
        path: r"^/(?P<ch>[A-Za-z0-9_]{5,32})/(?P<id>[0-9]{1,10})/?$",
        embed: Some(post("https://t.me/{ch}/{id}", &[("embed", "1")], 550)),
        ..BASE
    },
    // Tumblr's embed frame needs the post's reblog key, which isn't in the
    // URL — card only.
    Provider {
        id: "tumblr",
        name: "Tumblr",
        hosts: &["tumblr.com", "*.tumblr.com"],
        ..BASE
    },
    // ── GIFs / images ─────────────────────────────────────────────────
    Provider {
        id: "giphy",
        name: "GIPHY",
        hosts: &["giphy.com", "www.giphy.com"],
        path: r"^/(?:gifs|stickers)/(?:[A-Za-z0-9-]*-)?(?P<id>[A-Za-z0-9]{8,24})/?$",
        media: Some(MediaSpec {
            src: "https://media.giphy.com/media/{id}/200.gif",
            kind: "image",
        }),
        ..BASE
    },
    Provider {
        id: "imgur",
        name: "Imgur",
        hosts: &["imgur.com", "www.imgur.com", "m.imgur.com"],
        path: r"^/(?P<id>[A-Za-z0-9]{5,8})/?$",
        media: Some(MediaSpec {
            src: "https://i.imgur.com/{id}.jpg",
            kind: "image",
        }),
        ..BASE
    },
    Provider {
        id: "imgur",
        name: "Imgur",
        hosts: &["imgur.com", "www.imgur.com", "m.imgur.com"],
        ..BASE
    },
    Provider {
        id: "tenor",
        name: "Tenor",
        hosts: &["tenor.com", "www.tenor.com"],
        ..BASE
    },
    Provider {
        id: "flickr",
        name: "Flickr",
        hosts: &["flickr.com", "www.flickr.com", "flic.kr"],
        ..BASE
    },
    // ── Code / dev ────────────────────────────────────────────────────
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/pull/(?P<num>[0-9]{1,10})(?:/.*)?$"),
        enrich: Enrich::GithubPull,
        ..BASE
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/issues/(?P<num>[0-9]{1,10})/?$"),
        enrich: Enrich::GithubIssue,
        ..BASE
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/commit/(?P<sha>[0-9a-fA-F]{7,40})/?$"),
        enrich: Enrich::GithubCommit,
        ..BASE
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/actions/runs/(?P<run>[0-9]{1,20})(?:/.*)?$"),
        enrich: Enrich::GithubRun,
        ..BASE
    },
    Provider {
        id: "github",
        name: "GitHub",
        hosts: GH_HOSTS,
        path: gh_path!(r"/?$"),
        enrich: Enrich::GithubRepo,
        ..BASE
    },
    Provider {
        id: "gist",
        name: "GitHub Gist",
        hosts: &["gist.github.com"],
        path: r"^/(?:[A-Za-z0-9-]{1,39}/)?(?P<id>[0-9a-f]{20,40})/?$",
        enrich: Enrich::Gist,
        ..BASE
    },
    Provider {
        id: "gitlab",
        name: "GitLab",
        hosts: GITLAB_HOSTS,
        path: concat!(
            r"^/(?P<project>[A-Za-z0-9_.-]{1,100}(?:/[A-Za-z0-9_.-]{1,100}){1,5})",
            r"/-/merge_requests/(?P<num>[0-9]{1,10})(?:/.*)?$"
        ),
        enrich: Enrich::GitlabMerge,
        ..BASE
    },
    Provider {
        id: "gitlab",
        name: "GitLab",
        hosts: GITLAB_HOSTS,
        path: concat!(
            r"^/(?P<project>[A-Za-z0-9_.-]{1,100}(?:/[A-Za-z0-9_.-]{1,100}){1,5})",
            r"/-/(?:issues|work_items)/(?P<num>[0-9]{1,10})/?$"
        ),
        enrich: Enrich::GitlabIssue,
        ..BASE
    },
    Provider {
        id: "gitlab",
        name: "GitLab",
        hosts: GITLAB_HOSTS,
        path: r"^/(?P<project>[A-Za-z0-9_.-]{1,100}/[A-Za-z0-9_.-]{1,100})/?$",
        enrich: Enrich::GitlabRepo,
        ..BASE
    },
    Provider {
        id: "gitea",
        name: "Forgejo / Gitea",
        hosts: GITEA_HOSTS,
        path: forge_path!(r"/pulls/(?P<num>[0-9]{1,10})(?:/.*)?$"),
        enrich: Enrich::GiteaPull,
        ..BASE
    },
    Provider {
        id: "gitea",
        name: "Forgejo / Gitea",
        hosts: GITEA_HOSTS,
        path: forge_path!(r"/issues/(?P<num>[0-9]{1,10})/?$"),
        enrich: Enrich::GiteaIssue,
        ..BASE
    },
    Provider {
        id: "gitea",
        name: "Forgejo / Gitea",
        hosts: GITEA_HOSTS,
        path: forge_path!(r"/?$"),
        enrich: Enrich::GiteaRepo,
        ..BASE
    },
    Provider {
        id: "bitbucket",
        name: "Bitbucket",
        hosts: &["bitbucket.org", "www.bitbucket.org"],
        path: forge_path!(r"/pull-requests/(?P<num>[0-9]{1,10})(?:/.*)?$"),
        enrich: Enrich::BitbucketPull,
        ..BASE
    },
    Provider {
        id: "bitbucket",
        name: "Bitbucket",
        hosts: &["bitbucket.org", "www.bitbucket.org"],
        path: forge_path!(r"/?$"),
        enrich: Enrich::BitbucketRepo,
        ..BASE
    },
    Provider {
        id: "dockerhub",
        name: "Docker Hub",
        hosts: &["hub.docker.com"],
        path: r"^/(?:r/(?P<ns>[a-z0-9._-]{1,255})|_)/(?P<name>[a-z0-9._-]{1,255})/?$",
        enrich: Enrich::DockerHub,
        ..BASE
    },
    // GHCR has no anonymous metadata API: the GitHub package page's card.
    Provider {
        id: "ghcr",
        name: "GitHub Container Registry",
        hosts: &["ghcr.io"],
        ..BASE
    },
    Provider {
        id: "codepen",
        name: "CodePen",
        hosts: &["codepen.io"],
        path: r"^/(?P<user>[A-Za-z0-9_-]{1,64})/(?:pen|full|details)/(?P<id>[A-Za-z0-9]{4,12})/?$",
        embed: Some(sized(
            "https://codepen.io/{user}/embed/{id}",
            &[("default-tab", "result")],
            400,
        )),
        oembed: Some("https://codepen.io/api/oembed?format=json&url={url}"),
        ..BASE
    },
    Provider {
        id: "codesandbox",
        name: "CodeSandbox",
        hosts: &["codesandbox.io"],
        path: r"^/(?:s|p/sandbox|embed)/(?P<id>[A-Za-z0-9_-]{4,80})/?$",
        embed: Some(sized("https://codesandbox.io/embed/{id}", &[], 500)),
        ..BASE
    },
    Provider {
        id: "stackblitz",
        name: "StackBlitz",
        hosts: &["stackblitz.com"],
        path: r"^/edit/(?P<id>[A-Za-z0-9_-]{1,100})/?$",
        embed: Some(sized(
            "https://stackblitz.com/edit/{id}",
            &[("embed", "1")],
            500,
        )),
        ..BASE
    },
    Provider {
        id: "replit",
        name: "Replit",
        hosts: &["replit.com"],
        path: r"^/@(?P<user>[A-Za-z0-9_-]{1,64})/(?P<repl>[A-Za-z0-9_.-]{1,100})/?$",
        embed: Some(sized(
            "https://replit.com/@{user}/{repl}",
            &[("embed", "true")],
            500,
        )),
        ..BASE
    },
    Provider {
        id: "jsfiddle",
        name: "JSFiddle",
        hosts: &["jsfiddle.net"],
        path: r"^/(?P<user>[A-Za-z0-9_-]{1,64})/(?P<id>[A-Za-z0-9]{4,12})/?$",
        embed: Some(sized(
            "https://jsfiddle.net/{user}/{id}/embedded/",
            &[],
            400,
        )),
        ..BASE
    },
    Provider {
        id: "jsfiddle",
        name: "JSFiddle",
        hosts: &["jsfiddle.net"],
        path: r"^/(?P<id>[A-Za-z0-9]{4,12})/?$",
        embed: Some(sized("https://jsfiddle.net/{id}/embedded/", &[], 400)),
        ..BASE
    },
    Provider {
        id: "observable",
        name: "Observable",
        hosts: &["observablehq.com"],
        path: r"^/@(?P<user>[A-Za-z0-9_-]{1,64})/(?P<nb>[A-Za-z0-9_-]{1,100})/?$",
        embed: Some(sized(
            "https://observablehq.com/embed/@{user}/{nb}",
            &[],
            500,
        )),
        ..BASE
    },
    Provider {
        id: "nbviewer",
        name: "nbviewer",
        hosts: &["nbviewer.org", "nbviewer.jupyter.org"],
        ..BASE
    },
    Provider {
        id: "colab",
        name: "Google Colab",
        hosts: &["colab.research.google.com"],
        ..BASE
    },
    Provider {
        id: "huggingface",
        name: "Hugging Face",
        hosts: HF_HOSTS,
        path: r"^/spaces/(?P<owner>[A-Za-z0-9_.-]{1,96})/(?P<space>[A-Za-z0-9_.-]{1,96})/?$",
        embed: Some(sized(
            "https://{owner|subdomain}-{space|subdomain}.hf.space",
            &[],
            600,
        )),
        ..BASE
    },
    Provider {
        id: "huggingface",
        name: "Hugging Face",
        hosts: HF_HOSTS,
        path: r"^/(?:datasets/)?(?P<owner>[A-Za-z0-9_.-]{1,96})/(?P<repo>[A-Za-z0-9_.-]{1,96})/?$",
        enrich: Enrich::HuggingFace,
        ..BASE
    },
    Provider {
        id: "kaggle",
        name: "Kaggle",
        hosts: &["kaggle.com", "www.kaggle.com"],
        ..BASE
    },
    Provider {
        id: "wandb",
        name: "Weights & Biases",
        hosts: &["wandb.ai"],
        ..BASE
    },
    Provider {
        id: "npm",
        name: "npm",
        hosts: &["npmjs.com", "www.npmjs.com"],
        path: r"^/package/(?P<name>(?:@[a-z0-9][a-z0-9._-]{0,213}/)?[a-z0-9][a-z0-9._-]{0,213})(?:/v/[A-Za-z0-9._+-]{1,64})?/?$",
        enrich: Enrich::Npm,
        ..BASE
    },
    Provider {
        id: "crates",
        name: "crates.io",
        hosts: &["crates.io"],
        path: r"^/crates/(?P<name>[A-Za-z0-9_-]{1,64})(?:/[0-9][A-Za-z0-9._+-]{0,63})?/?$",
        enrich: Enrich::Crates,
        ..BASE
    },
    Provider {
        id: "pypi",
        name: "PyPI",
        hosts: &["pypi.org"],
        path: r"^/project/(?P<name>[A-Za-z0-9._-]{1,100})(?:/[A-Za-z0-9._+-]{1,64})?/?$",
        enrich: Enrich::Pypi,
        ..BASE
    },
    Provider {
        id: "stackexchange",
        name: "Stack Exchange",
        hosts: SE_HOSTS,
        path: r"^/(?:questions|q)/(?P<id>[0-9]{1,12})(?:/.*)?$",
        enrich: Enrich::StackExchange,
        ..BASE
    },
    Provider {
        id: "vercel",
        name: "Vercel",
        hosts: &["*.vercel.app", "vercel.com"],
        ..BASE
    },
    Provider {
        id: "netlify",
        name: "Netlify",
        hosts: &["*.netlify.app", "netlify.com", "www.netlify.com"],
        ..BASE
    },
    Provider {
        id: "sentry",
        name: "Sentry",
        hosts: &["*.sentry.io"],
        path: r"^/issues/(?P<issue>[0-9]{1,20})(?:/.*)?$",
        enrich: Enrich::Sentry,
        ..BASE
    },
    Provider {
        id: "sentry",
        name: "Sentry",
        hosts: &["sentry.io"],
        path: r"^/organizations/(?P<org>[a-z0-9_-]{1,64})/issues/(?P<issue>[0-9]{1,20})(?:/.*)?$",
        enrich: Enrich::Sentry,
        ..BASE
    },
    Provider {
        id: "linear",
        name: "Linear",
        hosts: &["linear.app"],
        path: r"^/[A-Za-z0-9_-]{1,64}/issue/(?P<key>[A-Z][A-Z0-9]{0,9}-[0-9]{1,8})(?:/.*)?$",
        enrich: Enrich::Linear,
        ..BASE
    },
    Provider {
        id: "jira",
        name: "Jira",
        hosts: &["*.atlassian.net"],
        path: r"^/browse/(?P<key>[A-Z][A-Z0-9_]{0,9}-[0-9]{1,8})/?$",
        enrich: Enrich::Jira,
        ..BASE
    },
    Provider {
        id: "trello",
        name: "Trello",
        hosts: &["trello.com"],
        path: r"^/c/(?P<card>[A-Za-z0-9]{8,24})(?:/.*)?$",
        enrich: Enrich::Trello,
        ..BASE
    },
    Provider {
        id: "trello",
        name: "Trello",
        hosts: &["trello.com"],
        ..BASE
    },
    // ── Files / docs / forms ──────────────────────────────────────────
    Provider {
        id: "google-docs",
        name: "Google Docs",
        hosts: &["docs.google.com"],
        path: r"^/document/d/(?P<id>[A-Za-z0-9_-]{20,100})(?:/.*)?$",
        embed: Some(sized(
            "https://docs.google.com/document/d/{id}/preview",
            &[],
            600,
        )),
        ..BASE
    },
    Provider {
        id: "google-sheets",
        name: "Google Sheets",
        hosts: &["docs.google.com"],
        path: r"^/spreadsheets/d/(?P<id>[A-Za-z0-9_-]{20,100})(?:/.*)?$",
        embed: Some(sized(
            "https://docs.google.com/spreadsheets/d/{id}/preview",
            &[],
            500,
        )),
        ..BASE
    },
    Provider {
        id: "google-slides",
        name: "Google Slides",
        hosts: &["docs.google.com"],
        path: r"^/presentation/d/(?P<id>[A-Za-z0-9_-]{20,100})(?:/.*)?$",
        embed: Some(video(
            "https://docs.google.com/presentation/d/{id}/embed",
            &[],
        )),
        ..BASE
    },
    Provider {
        id: "google-forms",
        name: "Google Forms",
        hosts: &["docs.google.com"],
        path: r"^/forms/d/e/(?P<id>[A-Za-z0-9_-]{20,100})(?:/.*)?$",
        embed: Some(sized(
            "https://docs.google.com/forms/d/e/{id}/viewform",
            &[("embedded", "true")],
            600,
        )),
        ..BASE
    },
    Provider {
        id: "google-forms",
        name: "Google Forms",
        hosts: &["docs.google.com"],
        path: r"^/forms/d/(?P<id>[A-Za-z0-9_-]{20,100})(?:/.*)?$",
        embed: Some(sized(
            "https://docs.google.com/forms/d/{id}/viewform",
            &[("embedded", "true")],
            600,
        )),
        ..BASE
    },
    Provider {
        id: "google-drive",
        name: "Google Drive",
        hosts: &["drive.google.com"],
        path: r"^/file/d/(?P<id>[A-Za-z0-9_-]{20,100})(?:/.*)?$",
        embed: Some(ratio(
            "https://drive.google.com/file/d/{id}/preview",
            &[],
            "4/3",
        )),
        ..BASE
    },
    Provider {
        id: "dropbox",
        name: "Dropbox",
        hosts: &["dropbox.com", "www.dropbox.com"],
        ..BASE
    },
    Provider {
        id: "onedrive",
        name: "OneDrive",
        hosts: &["onedrive.live.com", "1drv.ms", "*.sharepoint.com"],
        ..BASE
    },
    Provider {
        id: "notion",
        name: "Notion",
        hosts: &["notion.so", "www.notion.so", "*.notion.site"],
        ..BASE
    },
    Provider {
        id: "canva",
        name: "Canva",
        hosts: &["canva.com", "www.canva.com"],
        path: r"^/design/(?P<id>[A-Za-z0-9_-]{6,40})/(?P<key>[A-Za-z0-9_-]{6,40})/view/?$",
        embed: Some(video(
            "https://www.canva.com/design/{id}/{key}/view",
            &[("embed", "")],
        )),
        ..BASE
    },
    Provider {
        id: "airtable",
        name: "Airtable",
        hosts: &["airtable.com"],
        path: r"^/(?P<shr>shr[A-Za-z0-9]{10,20})/?$",
        embed: Some(sized("https://airtable.com/embed/{shr}", &[], 533)),
        ..BASE
    },
    Provider {
        id: "airtable",
        name: "Airtable",
        hosts: &["airtable.com"],
        path: r"^/(?P<app>app[A-Za-z0-9]{10,20})/(?P<shr>shr[A-Za-z0-9]{10,20})/?$",
        embed: Some(sized("https://airtable.com/embed/{app}/{shr}", &[], 533)),
        ..BASE
    },
    Provider {
        id: "figma",
        name: "Figma",
        hosts: &["figma.com", "www.figma.com"],
        path: r"^/(?:file|design|proto|board)/[A-Za-z0-9]{10,40}(?:/.*)?$",
        embed: Some(video(
            "https://www.figma.com/embed",
            &[("embed_host", "peckboard"), ("url", "{url|query}")],
        )),
        ..BASE
    },
    Provider {
        id: "miro",
        name: "Miro",
        hosts: &["miro.com"],
        path: r"^/app/board/(?P<id>[A-Za-z0-9_=-]{6,40})/?$",
        embed: Some(video("https://miro.com/app/live-embed/{id|path}/", &[])),
        ..BASE
    },
    Provider {
        id: "excalidraw",
        name: "Excalidraw",
        hosts: &["excalidraw.com"],
        ..BASE
    },
    Provider {
        id: "desmos",
        name: "Desmos",
        hosts: &["desmos.com", "www.desmos.com"],
        path: r"^/calculator/(?P<id>[a-z0-9]{6,20})/?$",
        embed: Some(video(
            "https://www.desmos.com/calculator/{id}",
            &[("embed", "")],
        )),
        ..BASE
    },
    Provider {
        id: "slideshare",
        name: "SlideShare",
        hosts: &["slideshare.net", "www.slideshare.net"],
        ..BASE
    },
    Provider {
        id: "speakerdeck",
        name: "Speaker Deck",
        hosts: &["speakerdeck.com"],
        ..BASE
    },
    Provider {
        id: "scribd",
        name: "Scribd",
        hosts: &["scribd.com", "www.scribd.com"],
        path: r"^/(?:document|doc|presentation)/(?P<id>[0-9]{4,15})(?:/.*)?$",
        embed: Some(sized(
            "https://www.scribd.com/embeds/{id}/content",
            &[],
            600,
        )),
        ..BASE
    },
    Provider {
        id: "typeform",
        name: "Typeform",
        hosts: &["*.typeform.com"],
        path: r"^/to/(?P<id>[A-Za-z0-9]{4,20})/?$",
        embed: Some(sized("https://form.typeform.com/to/{id}", &[], 500)),
        ..BASE
    },
    Provider {
        id: "calendly",
        name: "Calendly",
        hosts: &["calendly.com"],
        path: r"^/(?P<page>[A-Za-z0-9_-]{1,64}(?:/[A-Za-z0-9_-]{1,64})?)/?$",
        embed: Some(sized(
            "https://calendly.com/{page|path}",
            &[
                ("embed_domain", PARENT_HOST),
                ("embed_type", "Inline"),
                ("hide_gdpr_banner", "1"),
            ],
            700,
        )),
        ..BASE
    },
    // ── Charts ────────────────────────────────────────────────────────
    Provider {
        id: "datawrapper",
        name: "Datawrapper",
        hosts: &["datawrapper.dwcdn.net"],
        path: r"^/(?P<id>[A-Za-z0-9]{5})(?:/[0-9]{1,3})?/?$",
        embed: Some(sized("https://datawrapper.dwcdn.net/{id}/", &[], 450)),
        ..BASE
    },
    Provider {
        id: "datawrapper",
        name: "Datawrapper",
        hosts: &["datawrapper.de", "www.datawrapper.de"],
        path: r"^/_/(?P<id>[A-Za-z0-9]{5})/?$",
        embed: Some(sized("https://datawrapper.dwcdn.net/{id}/", &[], 450)),
        ..BASE
    },
    Provider {
        id: "flourish",
        name: "Flourish",
        hosts: &["public.flourish.studio"],
        path: r"^/(?P<kind>visualisation|story)/(?P<id>[0-9]{3,12})/?$",
        embed: Some(video("https://flo.uri.sh/{kind}/{id}/embed", &[])),
        ..BASE
    },
    Provider {
        id: "tableau",
        name: "Tableau Public",
        hosts: &["public.tableau.com"],
        path: r"^/(?:app/profile/[A-Za-z0-9_.-]{1,64}/)?viz/(?P<wb>[A-Za-z0-9_-]{1,100})/(?P<sheet>[A-Za-z0-9_-]{1,100})/?$",
        embed: Some(ratio(
            "https://public.tableau.com/views/{wb}/{sheet}",
            &[(":embed", "y"), (":showVizHome", "no")],
            "4/3",
        )),
        ..BASE
    },
    // TradingView's chart widget wants an `EXCHANGE:SYMBOL` the URL doesn't
    // reliably carry — card only.
    Provider {
        id: "tradingview",
        name: "TradingView",
        hosts: &["tradingview.com", "www.tradingview.com"],
        ..BASE
    },
    Provider {
        id: "plotly",
        name: "Plotly",
        hosts: &["chart-studio.plotly.com", "plot.ly"],
        path: r"^/~(?P<user>[A-Za-z0-9_.-]{1,64})/(?P<id>[0-9]{1,8})/?$",
        embed: Some(ratio(
            "https://chart-studio.plotly.com/~{user}/{id}.embed",
            &[],
            "4/3",
        )),
        ..BASE
    },
    // ── 3D printing / assets ──────────────────────────────────────────
    Provider {
        id: "sketchfab",
        name: "Sketchfab",
        hosts: &["sketchfab.com", "www.sketchfab.com"],
        path: r"^/(?:3d-models/(?:[A-Za-z0-9_-]*-)?|models/)(?P<uid>[0-9a-f]{32})(?:/embed)?/?$",
        embed: Some(video("https://sketchfab.com/models/{uid}/embed", &[])),
        oembed: Some("https://sketchfab.com/oembed?url={url}"),
        ..BASE
    },
    Provider {
        id: "polyhaven",
        name: "Poly Haven",
        hosts: &["polyhaven.com"],
        path: r"^/a/(?P<id>[a-z0-9_]{1,80})/?$",
        enrich: Enrich::PolyHaven,
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "printables",
        name: "Printables",
        hosts: &["printables.com", "www.printables.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "thingiverse",
        name: "Thingiverse",
        hosts: &["thingiverse.com", "www.thingiverse.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "makerworld",
        name: "MakerWorld",
        hosts: &["makerworld.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "thangs",
        name: "Thangs",
        hosts: &["thangs.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "cults3d",
        name: "Cults",
        hosts: &["cults3d.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "myminifactory",
        name: "MyMiniFactory",
        hosts: &["myminifactory.com", "www.myminifactory.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "fab",
        name: "Fab",
        hosts: &["fab.com", "www.fab.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "unity-assets",
        name: "Unity Asset Store",
        hosts: &["assetstore.unity.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "opengameart",
        name: "OpenGameArt",
        hosts: &["opengameart.org"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "kenney",
        name: "Kenney",
        hosts: &["kenney.nl", "www.kenney.nl"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "turbosquid",
        name: "TurboSquid",
        hosts: &["turbosquid.com", "www.turbosquid.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "cgtrader",
        name: "CGTrader",
        hosts: &["cgtrader.com", "www.cgtrader.com"],
        card: CardHint::Asset,
        ..BASE
    },
    Provider {
        id: "blenderkit",
        name: "BlenderKit",
        hosts: &["blenderkit.com", "www.blenderkit.com"],
        card: CardHint::Asset,
        ..BASE
    },
    // ── Games ─────────────────────────────────────────────────────────
    Provider {
        id: "steam",
        name: "Steam",
        hosts: &["store.steampowered.com"],
        path: r"^/app/(?P<app>[0-9]{1,10})(?:/.*)?$",
        enrich: Enrich::Steam,
        ..BASE
    },
    Provider {
        id: "itch",
        name: "itch.io",
        hosts: &["itch.io", "*.itch.io"],
        ..BASE
    },
    // ── Shopping (schema.org Product JSON-LD comes from the scrape) ───
    Provider {
        id: "amazon",
        name: "Amazon",
        hosts: SHOP_AMAZON,
        card: CardHint::Shop,
        ..BASE
    },
    Provider {
        id: "ebay",
        name: "eBay",
        hosts: &[
            "ebay.com",
            "www.ebay.com",
            "ebay.co.uk",
            "www.ebay.co.uk",
            "ebay.de",
            "www.ebay.de",
            "ebay.us",
        ],
        card: CardHint::Shop,
        ..BASE
    },
    Provider {
        id: "etsy",
        name: "Etsy",
        hosts: &["etsy.com", "www.etsy.com", "etsy.me"],
        card: CardHint::Shop,
        ..BASE
    },
    Provider {
        id: "aliexpress",
        name: "AliExpress",
        hosts: &["aliexpress.com", "*.aliexpress.com"],
        card: CardHint::Shop,
        ..BASE
    },
    // ── News / knowledge ──────────────────────────────────────────────
    Provider {
        id: "hackernews",
        name: "Hacker News",
        hosts: &["news.ycombinator.com"],
        path: r"^/item/?$",
        query: &[QueryCap {
            param: "id",
            capture: "id",
            pattern: r"^[0-9]{1,12}$",
        }],
        enrich: Enrich::HackerNews,
        ..BASE
    },
    Provider {
        id: "producthunt",
        name: "Product Hunt",
        hosts: &["producthunt.com", "www.producthunt.com"],
        ..BASE
    },
    Provider {
        id: "substack",
        name: "Substack",
        hosts: &["substack.com", "*.substack.com"],
        ..BASE
    },
    Provider {
        id: "medium",
        name: "Medium",
        hosts: &["medium.com", "*.medium.com"],
        ..BASE
    },
    Provider {
        id: "quora",
        name: "Quora",
        hosts: &["quora.com", "*.quora.com"],
        ..BASE
    },
    Provider {
        id: "wikipedia",
        name: "Wikipedia",
        hosts: &["*.wikipedia.org"],
        path: r"^/wiki/(?P<title>[^/?#]{1,300})$",
        enrich: Enrich::Wikipedia,
        ..BASE
    },
    Provider {
        id: "arxiv",
        name: "arXiv",
        hosts: &["arxiv.org", "www.arxiv.org"],
        path: r"^/(?:abs|pdf|html)/(?P<id>(?:[0-9]{4}\.[0-9]{4,5}|[a-z-]{2,20}(?:\.[A-Z]{2})?/[0-9]{7})(?:v[0-9]{1,3})?)(?:\.pdf)?/?$",
        enrich: Enrich::Arxiv,
        ..BASE
    },
    // ── Maps / activity ───────────────────────────────────────────────
    Provider {
        id: "google-maps",
        name: "Google Maps",
        hosts: GMAPS_HOSTS,
        path: r"^/maps/@(?P<lat>-?[0-9]{1,3}\.[0-9]{1,12}),(?P<lng>-?[0-9]{1,3}\.[0-9]{1,12}),(?P<z>[0-9]{1,2})(?:\.[0-9]+)?z",
        embed: Some(ratio(
            "https://maps.google.com/maps",
            &[("q", "{lat}%2C{lng}"), ("z", "{z}"), ("output", "embed")],
            "4/3",
        )),
        ..BASE
    },
    // Street View (`@lat,lng,3a,…`): its embed needs an API key, so the
    // same spot as a map.
    Provider {
        id: "google-maps",
        name: "Google Maps",
        hosts: GMAPS_HOSTS,
        path: r"^/maps/@(?P<lat>-?[0-9]{1,3}\.[0-9]{1,12}),(?P<lng>-?[0-9]{1,3}\.[0-9]{1,12}),",
        embed: Some(ratio(
            "https://maps.google.com/maps",
            &[("q", "{lat}%2C{lng}"), ("z", "17"), ("output", "embed")],
            "4/3",
        )),
        ..BASE
    },
    Provider {
        id: "google-maps",
        name: "Google Maps",
        hosts: GMAPS_HOSTS,
        path: r"^/maps/place/(?P<place>[A-Za-z0-9+,._-]{1,200})(?:/.*)?$",
        embed: Some(ratio(
            "https://maps.google.com/maps",
            &[("q", "{place|path}"), ("output", "embed")],
            "4/3",
        )),
        ..BASE
    },
    Provider {
        id: "google-maps",
        name: "Google Maps",
        hosts: &["maps.app.goo.gl", "goo.gl"],
        ..BASE
    },
    // MapKit JS needs a developer token — card only.
    Provider {
        id: "apple-maps",
        name: "Apple Maps",
        hosts: &["maps.apple.com"],
        ..BASE
    },
    Provider {
        id: "openstreetmap",
        name: "OpenStreetMap",
        hosts: &["openstreetmap.org", "www.openstreetmap.org"],
        path: r"^/$",
        embed: Some(ratio(
            "https://www.openstreetmap.org/export/embed.html?bbox={bbox}",
            &[("layer", "mapnik"), ("marker", "{marker}")],
            "4/3",
        )),
        enrich: Enrich::Osm,
        ..BASE
    },
    Provider {
        id: "strava",
        name: "Strava",
        hosts: &["strava.com", "www.strava.com"],
        path: r"^/activities/(?P<id>[0-9]{5,15})/?$",
        embed: Some(sized(
            "https://strava-embeds.com/activity/{id}",
            &[("style", "standard")],
            405,
        )),
        ..BASE
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

/// `pattern` is an exact host or `*.suffix` (any subdomain, not the apex).
fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
        None => pattern == host,
    }
}

pub fn match_url(url: &Url) -> Option<ProviderMatch> {
    let host = url.host_str()?.to_ascii_lowercase();
    let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    for (p, c) in PROVIDERS.iter().zip(COMPILED.iter()) {
        if !p.hosts.iter().any(|h| host_matches(h, &host)) {
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
        // The iframe/media path must be fillable (e.g. `/watch` without `v`).
        let template = p
            .embed
            .as_ref()
            .map(|e| e.iframe)
            .or(p.media.as_ref().map(|m| m.src));
        if let Some(t) = template {
            let fillable = PLACEHOLDER_RE
                .captures_iter(t)
                .all(|ph| caps.contains_key(&ph[1]) || p.enrich.fills().contains(&&ph[1]));
            if !fillable {
                continue;
            }
        }
        caps.insert("host".into(), host.clone());
        let mut link = url.clone();
        link.set_fragment(None);
        caps.insert("url".into(), link.to_string());
        return Some(ProviderMatch { provider: p, caps });
    }
    None
}

/// Percent-encode for a path segment or query value (keeps `:` and `@`,
/// which DIDs and handles use).
fn encode(s: &str) -> String {
    encode_keeping(s, b":@")
}

fn encode_keeping(s: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') || keep.contains(&b)
        {
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
        let filled = match ph.get(2).map(|f| f.as_str()) {
            None => encode(value),
            Some("seconds") => encode(&seconds(value)?),
            // Every reserved byte encoded (a whole URL as a query value).
            Some("query") => encode_keeping(value, b""),
            // Segments encoded, `/` kept; never a `..` segment.
            Some("path") => {
                let segs: Vec<&str> = value.split('/').collect();
                if segs.iter().any(|s| *s == "." || *s == "..") {
                    return None;
                }
                segs.iter()
                    .map(|s| encode_keeping(s, b":@=,+"))
                    .collect::<Vec<_>>()
                    .join("/")
            }
            Some("subdomain") => {
                let label: String = value
                    .to_ascii_lowercase()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                if label.is_empty() || label.len() > 63 {
                    return None;
                }
                label
            }
            Some(_) => return None,
        };
        out.push_str(&filled);
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
/// A templated host label (`{owner}-{space}.hf.space`) becomes a wildcard
/// (`https://*.hf.space`).
pub fn frame_src_origins() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in PROVIDERS {
        let Some(e) = &p.embed else { continue };
        let Some((scheme, rest)) = e.iframe.split_once("://") else {
            continue;
        };
        let host = rest.split(['/', '?']).next().unwrap_or("");
        let host = match host.rfind('}') {
            Some(i) => match host[i..].find('.') {
                Some(dot) => format!("*{}", &host[i + dot..]),
                None => continue,
            },
            None => host.to_string(),
        };
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
    fn tweets_embed_as_cross_origin_frames() {
        for raw in [
            "https://twitter.com/jack/status/20",
            "https://x.com/jack/status/20?s=46",
            "https://mobile.twitter.com/jack/status/20/photo/1",
        ] {
            let pm = m(raw).unwrap_or_else(|| panic!("{raw}"));
            assert_eq!(pm.provider.id, "x");
            let e = pm.provider.embed.as_ref().unwrap();
            assert_eq!(e.sandbox, SandboxMode::Origin);
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

    /// One or two representative URLs per provider: `(url, provider id,
    /// expected embed src)`. `None` = no embed (a card, media or API entry).
    #[test]
    fn every_provider_matches_its_representative_urls() {
        const P: &str = PARENT_HOST;
        let cases: &[(&str, &str, Option<&str>)] = &[
            (
                "https://music.youtube.com/playlist?list=OLAK5uy_abcdefghijk",
                "youtube",
                Some("https://www.youtube-nocookie.com/embed/videoseries?list=OLAK5uy_abcdefghijk"),
            ),
            (
                "https://vimeo.com/76979871",
                "vimeo",
                Some("https://player.vimeo.com/video/76979871?dnt=1"),
            ),
            (
                "https://vimeo.com/76979871/abcdef1234",
                "vimeo",
                Some("https://player.vimeo.com/video/76979871?h=abcdef1234&dnt=1"),
            ),
            (
                "https://clips.twitch.tv/FunnyClipSlug-abc",
                "twitch",
                Some(&*Box::leak(
                    format!("https://clips.twitch.tv/embed?clip=FunnyClipSlug-abc&parent={P}&autoplay=false")
                        .into_boxed_str(),
                )),
            ),
            (
                "https://www.twitch.tv/videos/123456789",
                "twitch",
                Some(&*Box::leak(
                    format!("https://player.twitch.tv/?video=v123456789&parent={P}&autoplay=false")
                        .into_boxed_str(),
                )),
            ),
            (
                "https://www.twitch.tv/somechannel",
                "twitch",
                Some(&*Box::leak(
                    format!("https://player.twitch.tv/?channel=somechannel&parent={P}&autoplay=false")
                        .into_boxed_str(),
                )),
            ),
            (
                "https://kick.com/streamer",
                "kick",
                Some("https://player.kick.com/streamer?autoplay=false"),
            ),
            (
                "https://rumble.com/embed/v4abcd/",
                "rumble",
                Some("https://rumble.com/embed/v4abcd/"),
            ),
            ("https://rumble.com/v4abcd-some-title.html", "rumble", None),
            (
                "https://odysee.com/@chan:1/video-name:a",
                "odysee",
                Some("https://odysee.com/$/embed/@chan:1/video-name:a"),
            ),
            (
                "https://www.loom.com/share/0123456789abcdef0123456789abcdef",
                "loom",
                Some("https://www.loom.com/embed/0123456789abcdef0123456789abcdef"),
            ),
            (
                "https://www.dailymotion.com/video/x8abcd1",
                "dailymotion",
                Some("https://www.dailymotion.com/embed/video/x8abcd1"),
            ),
            (
                "https://dai.ly/x8abcd1",
                "dailymotion",
                Some("https://www.dailymotion.com/embed/video/x8abcd1"),
            ),
            (
                "https://streamable.com/abc12",
                "streamable",
                Some("https://streamable.com/e/abc12"),
            ),
            (
                "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC",
                "spotify",
                Some("https://open.spotify.com/embed/track/4uLU6hMCjMI75M1A2tKUQC"),
            ),
            (
                "https://open.spotify.com/intl-de/album/4uLU6hMCjMI75M1A2tKUQC",
                "spotify",
                Some("https://open.spotify.com/embed/album/4uLU6hMCjMI75M1A2tKUQC"),
            ),
            (
                "https://soundcloud.com/artist/track-name",
                "soundcloud",
                Some(
                    "https://w.soundcloud.com/player/?url=https%3A%2F%2Fsoundcloud.com%2Fartist%2Ftrack-name&visual=false",
                ),
            ),
            ("https://artist.bandcamp.com/album/great-album", "bandcamp", None),
            (
                "https://www.mixcloud.com/dj/show-name/",
                "mixcloud",
                Some("https://www.mixcloud.com/widget/iframe/?feed=%2Fdj%2Fshow-name%2F&hide_cover=1"),
            ),
            (
                "https://audiomack.com/artist/song/track",
                "audiomack",
                Some("https://audiomack.com/embed/song/artist/track"),
            ),
            (
                "https://www.deezer.com/en/track/3135556",
                "deezer",
                Some("https://widget.deezer.com/widget/auto/track/3135556"),
            ),
            (
                "https://tidal.com/browse/album/12345",
                "tidal",
                Some("https://embed.tidal.com/albums/12345"),
            ),
            (
                "https://music.apple.com/us/album/some-album/1440857781?i=1440857900",
                "apple-music",
                Some("https://embed.music.apple.com/us/album/some-album/1440857781?i=1440857900"),
            ),
            (
                "https://podcasts.apple.com/us/podcast/the-show/id1200361736",
                "apple-podcasts",
                Some("https://embed.podcasts.apple.com/us/podcast/the-show/id1200361736"),
            ),
            ("https://pca.st/episode/abc", "pocketcasts", None),
            ("https://overcast.fm/+AbCdEf", "overcast", None),
            (
                "https://www.threads.net/@zuck/post/C1a2B3c4D5",
                "threads",
                Some("https://www.threads.net/@zuck/post/C1a2B3c4D5/embed"),
            ),
            (
                "https://www.instagram.com/p/CxYz123AbC/",
                "instagram",
                Some("https://www.instagram.com/p/CxYz123AbC/embed/captioned/"),
            ),
            (
                "https://www.tiktok.com/@user/video/7234567890123456789",
                "tiktok",
                Some("https://www.tiktok.com/player/v1/7234567890123456789?autoplay=0"),
            ),
            (
                "https://www.reddit.com/r/rust/comments/abc123/some_title/",
                "reddit",
                Some("https://embed.reddit.com/r/rust/comments/abc123/"),
            ),
            (
                "https://www.linkedin.com/feed/update/urn:li:activity:7123456789012345678/",
                "linkedin",
                Some("https://www.linkedin.com/embed/feed/update/urn:li:activity:7123456789012345678"),
            ),
            ("https://www.linkedin.com/posts/someone_slug-activity-1", "linkedin", None),
            (
                "https://www.facebook.com/page.name/posts/pfbid0abc",
                "facebook",
                Some(
                    "https://www.facebook.com/plugins/post.php?href=https%3A%2F%2Fwww.facebook.com%2Fpage.name%2Fposts%2Fpfbid0abc&show_text=true",
                ),
            ),
            (
                "https://www.pinterest.com/pin/123456789012/",
                "pinterest",
                Some("https://assets.pinterest.com/ext/embed.html?id=123456789012"),
            ),
            (
                "https://de.pinterest.com/pin/123456789012/",
                "pinterest",
                Some("https://assets.pinterest.com/ext/embed.html?id=123456789012"),
            ),
            (
                "https://t.me/durov/123",
                "telegram",
                Some("https://t.me/durov/123?embed=1"),
            ),
            ("https://staff.tumblr.com/post/123", "tumblr", None),
            ("https://giphy.com/gifs/cat-funny-abc123XYZ9", "giphy", None),
            ("https://imgur.com/AbCd123", "imgur", None),
            ("https://imgur.com/gallery/AbCd123", "imgur", None),
            ("https://tenor.com/view/cat-gif-123", "tenor", None),
            ("https://www.flickr.com/photos/u/123", "flickr", None),
            ("https://gist.github.com/octo/0123456789abcdef0123", "gist", None),
            ("https://gitlab.com/group/sub/proj/-/merge_requests/12", "gitlab", None),
            ("https://gitlab.com/group/proj/-/issues/3", "gitlab", None),
            ("https://gitlab.com/group/proj", "gitlab", None),
            ("https://codeberg.org/forgejo/forgejo/pulls/42", "gitea", None),
            ("https://codeberg.org/forgejo/forgejo", "gitea", None),
            ("https://bitbucket.org/team/repo/pull-requests/7", "bitbucket", None),
            ("https://hub.docker.com/_/nginx", "dockerhub", None),
            ("https://hub.docker.com/r/grafana/grafana", "dockerhub", None),
            ("https://ghcr.io/owner/image", "ghcr", None),
            (
                "https://codepen.io/someone/pen/AbCdEf",
                "codepen",
                Some("https://codepen.io/someone/embed/AbCdEf?default-tab=result"),
            ),
            (
                "https://codesandbox.io/s/abc-123",
                "codesandbox",
                Some("https://codesandbox.io/embed/abc-123"),
            ),
            (
                "https://stackblitz.com/edit/vite-react",
                "stackblitz",
                Some("https://stackblitz.com/edit/vite-react?embed=1"),
            ),
            (
                "https://replit.com/@user/MyRepl",
                "replit",
                Some("https://replit.com/@user/MyRepl?embed=true"),
            ),
            (
                "https://jsfiddle.net/user/ab12cd/",
                "jsfiddle",
                Some("https://jsfiddle.net/user/ab12cd/embedded/"),
            ),
            (
                "https://observablehq.com/@d3/bar-chart",
                "observable",
                Some("https://observablehq.com/embed/@d3/bar-chart"),
            ),
            ("https://nbviewer.org/github/x/y/blob/main/a.ipynb", "nbviewer", None),
            ("https://colab.research.google.com/drive/abc", "colab", None),
            (
                "https://huggingface.co/spaces/Some_Org/my.space",
                "huggingface",
                Some("https://some-org-my-space.hf.space"),
            ),
            ("https://huggingface.co/meta-llama/Llama-3-8B", "huggingface", None),
            ("https://huggingface.co/datasets/squad/plain", "huggingface", None),
            ("https://www.kaggle.com/code/x/y", "kaggle", None),
            ("https://wandb.ai/team/project/reports/x", "wandb", None),
            ("https://www.npmjs.com/package/@types/node", "npm", None),
            ("https://crates.io/crates/serde", "crates", None),
            ("https://pypi.org/project/requests/", "pypi", None),
            (
                "https://stackoverflow.com/questions/11227809/why-is-it-faster",
                "stackexchange",
                None,
            ),
            ("https://math.stackexchange.com/q/12345", "stackexchange", None),
            ("https://my-app.vercel.app/", "vercel", None),
            ("https://my-site.netlify.app/", "netlify", None),
            ("https://acme.sentry.io/issues/123456/", "sentry", None),
            ("https://sentry.io/organizations/acme/issues/123/", "sentry", None),
            ("https://linear.app/acme/issue/ENG-123/fix-it", "linear", None),
            ("https://acme.atlassian.net/browse/PROJ-42", "jira", None),
            ("https://trello.com/c/AbCdEfGh/12-card", "trello", None),
            (
                "https://docs.google.com/document/d/1AbCdEfGhIjKlMnOpQrStUvWxYz/edit",
                "google-docs",
                Some("https://docs.google.com/document/d/1AbCdEfGhIjKlMnOpQrStUvWxYz/preview"),
            ),
            (
                "https://docs.google.com/spreadsheets/d/1AbCdEfGhIjKlMnOpQrStUvWxYz/edit#gid=0",
                "google-sheets",
                Some("https://docs.google.com/spreadsheets/d/1AbCdEfGhIjKlMnOpQrStUvWxYz/preview"),
            ),
            (
                "https://docs.google.com/presentation/d/1AbCdEfGhIjKlMnOpQrStUvWxYz/edit",
                "google-slides",
                Some("https://docs.google.com/presentation/d/1AbCdEfGhIjKlMnOpQrStUvWxYz/embed"),
            ),
            (
                "https://docs.google.com/forms/d/e/1FAIpQLSdAbCdEfGhIjKlMnOp/viewform",
                "google-forms",
                Some("https://docs.google.com/forms/d/e/1FAIpQLSdAbCdEfGhIjKlMnOp/viewform?embedded=true"),
            ),
            (
                "https://drive.google.com/file/d/1AbCdEfGhIjKlMnOpQrStUvWxYz/view",
                "google-drive",
                Some("https://drive.google.com/file/d/1AbCdEfGhIjKlMnOpQrStUvWxYz/preview"),
            ),
            ("https://www.dropbox.com/s/abc/file.pdf", "dropbox", None),
            ("https://1drv.ms/w/s!abc", "onedrive", None),
            ("https://acme.notion.site/Page-123", "notion", None),
            (
                "https://www.canva.com/design/DAFabc123/xyz789abc/view",
                "canva",
                Some("https://www.canva.com/design/DAFabc123/xyz789abc/view?embed="),
            ),
            (
                "https://airtable.com/shrAbCdEfGhIjKlMn",
                "airtable",
                Some("https://airtable.com/embed/shrAbCdEfGhIjKlMn"),
            ),
            (
                "https://www.figma.com/design/AbCdEfGhIjKl/Name",
                "figma",
                Some(
                    "https://www.figma.com/embed?embed_host=peckboard&url=https%3A%2F%2Fwww.figma.com%2Fdesign%2FAbCdEfGhIjKl%2FName",
                ),
            ),
            (
                "https://miro.com/app/board/uXjVOAbC12=/",
                "miro",
                Some("https://miro.com/app/live-embed/uXjVOAbC12=/"),
            ),
            ("https://excalidraw.com/#json=abc,def", "excalidraw", None),
            (
                "https://www.desmos.com/calculator/abcdef1234",
                "desmos",
                Some("https://www.desmos.com/calculator/abcdef1234?embed="),
            ),
            ("https://www.slideshare.net/user/deck", "slideshare", None),
            ("https://speakerdeck.com/user/deck", "speakerdeck", None),
            (
                "https://www.scribd.com/document/123456/Title",
                "scribd",
                Some("https://www.scribd.com/embeds/123456/content"),
            ),
            (
                "https://acme.typeform.com/to/AbCd12",
                "typeform",
                Some("https://form.typeform.com/to/AbCd12"),
            ),
            (
                "https://calendly.com/someone/30min",
                "calendly",
                Some(&*Box::leak(
                    format!("https://calendly.com/someone/30min?embed_domain={P}&embed_type=Inline&hide_gdpr_banner=1")
                        .into_boxed_str(),
                )),
            ),
            (
                "https://datawrapper.dwcdn.net/AbC12/3/",
                "datawrapper",
                Some("https://datawrapper.dwcdn.net/AbC12/"),
            ),
            (
                "https://public.flourish.studio/visualisation/1234567/",
                "flourish",
                Some("https://flo.uri.sh/visualisation/1234567/embed"),
            ),
            (
                "https://public.tableau.com/app/profile/me/viz/Workbook/Sheet1",
                "tableau",
                Some("https://public.tableau.com/views/Workbook/Sheet1?:embed=y&:showVizHome=no"),
            ),
            ("https://www.tradingview.com/symbols/NASDAQ-AAPL/", "tradingview", None),
            (
                "https://chart-studio.plotly.com/~user/12",
                "plotly",
                Some("https://chart-studio.plotly.com/~user/12.embed"),
            ),
            (
                "https://sketchfab.com/3d-models/cool-thing-0123456789abcdef0123456789abcdef",
                "sketchfab",
                Some("https://sketchfab.com/models/0123456789abcdef0123456789abcdef/embed"),
            ),
            ("https://polyhaven.com/a/rock_01", "polyhaven", None),
            ("https://www.printables.com/model/12345-thing", "printables", None),
            ("https://www.thingiverse.com/thing:12345", "thingiverse", None),
            ("https://makerworld.com/en/models/12345", "makerworld", None),
            ("https://thangs.com/designer/x/3d-model/y", "thangs", None),
            ("https://cults3d.com/en/3d-model/x", "cults3d", None),
            ("https://www.myminifactory.com/object/3d-print-x", "myminifactory", None),
            ("https://www.fab.com/listings/abc", "fab", None),
            ("https://assetstore.unity.com/packages/x", "unity-assets", None),
            ("https://opengameart.org/content/x", "opengameart", None),
            ("https://kenney.nl/assets/x", "kenney", None),
            ("https://www.turbosquid.com/3d-models/x", "turbosquid", None),
            ("https://www.cgtrader.com/3d-models/x", "cgtrader", None),
            ("https://www.blenderkit.com/asset-gallery-detail/x", "blenderkit", None),
            ("https://store.steampowered.com/app/620/Portal_2/", "steam", None),
            ("https://dev.itch.io/game", "itch", None),
            ("https://www.amazon.com/Thing/dp/B000123456", "amazon", None),
            ("https://www.amazon.co.uk/dp/B000123456", "amazon", None),
            ("https://www.ebay.com/itm/1234567890", "ebay", None),
            ("https://www.etsy.com/listing/123/thing", "etsy", None),
            ("https://www.aliexpress.com/item/100.html", "aliexpress", None),
            ("https://news.ycombinator.com/item?id=8863", "hackernews", None),
            ("https://www.producthunt.com/posts/x", "producthunt", None),
            ("https://writer.substack.com/p/post", "substack", None),
            ("https://medium.com/@me/post-123", "medium", None),
            ("https://www.quora.com/What-is-x", "quora", None),
            ("https://en.wikipedia.org/wiki/Rust_(programming_language)", "wikipedia", None),
            ("https://arxiv.org/abs/1706.03762v7", "arxiv", None),
            ("https://arxiv.org/pdf/1706.03762", "arxiv", None),
            (
                "https://www.google.com/maps/@52.5200066,13.404954,14z",
                "google-maps",
                Some("https://maps.google.com/maps?q=52.5200066%2C13.404954&z=14&output=embed"),
            ),
            (
                "https://www.google.com/maps/@48.8583701,2.2944813,3a,75y,90t/data=x",
                "google-maps",
                Some("https://maps.google.com/maps?q=48.8583701%2C2.2944813&z=17&output=embed"),
            ),
            (
                "https://www.google.com/maps/place/Eiffel+Tower/@48.8,2.29,17z",
                "google-maps",
                Some("https://maps.google.com/maps?q=Eiffel+Tower&output=embed"),
            ),
            ("https://maps.apple.com/?q=Berlin", "apple-maps", None),
            (
                "https://www.strava.com/activities/1234567890",
                "strava",
                Some("https://strava-embeds.com/activity/1234567890?style=standard"),
            ),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (url, id, want) in cases {
            let pm = m(url).unwrap_or_else(|| panic!("{url} matched nothing"));
            assert_eq!(pm.provider.id, *id, "{url}");
            seen.insert(*id);
            let got = pm
                .provider
                .embed
                .as_ref()
                .and_then(|e| embed_src(e, &pm.caps));
            if want.is_some() || pm.provider.enrich.fills().is_empty() {
                assert_eq!(got.as_deref(), *want, "{url}");
            }
        }
        // Every provider in the table has at least one case above (the
        // original four are covered by the dedicated tests).
        for p in PROVIDERS {
            if !matches!(
                p.id,
                "youtube" | "x" | "bluesky" | "github" | "openstreetmap"
            ) {
                assert!(seen.contains(p.id), "no test case for {}", p.id);
            }
        }
    }

    #[test]
    fn direct_media_and_hints() {
        let pm = m("https://giphy.com/gifs/cat-funny-abc123XYZ9").unwrap();
        let media = pm.provider.media.as_ref().unwrap();
        assert_eq!(
            render(media.src, &pm.caps).as_deref(),
            Some("https://media.giphy.com/media/abc123XYZ9/200.gif")
        );
        assert_eq!(
            m("https://www.printables.com/model/1")
                .unwrap()
                .provider
                .card,
            CardHint::Asset
        );
        assert_eq!(
            m("https://smile.amazon.de/dp/B0").unwrap().provider.card,
            CardHint::Shop
        );
        // `*.` never matches the bare suffix or a look-alike.
        assert!(m("https://evilbandcamp.com/album/x").is_none());
        assert!(m("https://wikipedia.org/wiki/X").is_none());
        // A `..` segment never reaches a `path` template.
        let mut caps = BTreeMap::new();
        caps.insert("p".to_string(), "a/../b".to_string());
        assert!(render("https://x/{p|path}", &caps).is_none());
    }

    #[test]
    fn frame_src_is_exactly_the_embed_origins() {
        let origins = frame_src_origins();
        for want in [
            "https://www.youtube-nocookie.com",
            "https://platform.twitter.com",
            "https://embed.bsky.app",
            "https://player.vimeo.com",
            "https://player.twitch.tv",
            "https://clips.twitch.tv",
            "https://open.spotify.com",
            "https://*.hf.space",
            "https://www.openstreetmap.org",
        ] {
            assert!(origins.iter().any(|o| o == want), "missing {want}");
        }
        // No duplicates, nothing but embed hosts, and nothing templated.
        let unique: std::collections::BTreeSet<_> = origins.iter().collect();
        assert_eq!(unique.len(), origins.len());
        assert!(
            origins
                .iter()
                .all(|o| !o.contains('{') && o.starts_with("https://"))
        );
        let embed_hosts = PROVIDERS.iter().filter(|p| p.embed.is_some()).count();
        assert!(origins.len() <= embed_hosts);
        // A card-only provider never widens frame-src.
        assert!(!origins.iter().any(|o| o.contains("tumblr")));
    }
}
