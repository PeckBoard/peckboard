//! Link previews / embeds (`GET /api/link-preview`).
//!
//! Given a URL, produce one of four preview kinds:
//! - `internal` — the URL points at this instance; only `{entity, id}` is
//!   returned (never fetched) so the UI loads live data with the viewer's
//!   own permissions ([`internal`]).
//! - `embed` — a [`providers`] table entry maps it to a sandboxed iframe.
//! - `media` — the URL itself serves an image/video/audio/model file.
//! - `card` — Open Graph / Twitter Card / oEmbed / JSON-LD metadata scraped
//!   from the page ([`parse`]), or a GitHub API card.
//!
//! Every network read goes through [`ssrf`] (proof-token API: only a
//! `VettedTarget` can be fetched). Image URLs in a preview are rewritten to
//! signed `/api/link-preview/image` URLs so the browser never contacts
//! arbitrary hosts; the proxy re-applies the same guard. Results are cached
//! in memory (6 h, errors 10 min) and outbound fetches are rate-limited per
//! domain.
//!
//! Remote audio / video / 3D-model files get a signed `media.src` on the
//! same route (`k=media`): the server streams them through the guard with
//! `Range` passthrough, a content-type allowlist and per-type size caps
//! (video 500 MB, audio / model 100 MB), so inline players work under
//! `media-src 'self'`.
//!
//! Fediverse posts (any instance: nodeinfo probe, then that software's JSON
//! API — [`fediverse`]), `.ics` events ([`ics`]), API-backed cards
//! ([`apis`], [`github`]) and self-hosted app fingerprints ([`lan`]) all
//! become `card`s with typed `details`.
//!
//! Screenshot fallback (setting, default **off**): when a page has no
//! og:image the preview may carry a screenshot taken by the headless browser
//! sidecar (`service::browser`). Only the top-level URL is vetted — Chrome
//! resolves DNS itself and follows its own redirects and subresource loads,
//! none of which the guard can see — so enabling it lets a previewed page
//! make the server's browser touch internal addresses. Hence opt-in.

mod apis;
mod cache;
mod fediverse;
mod github;
mod ics;
pub mod internal;
mod lan;
pub mod parse;
pub mod providers;
pub mod ssrf;

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use cache::TtlLru;
use internal::{InstanceHosts, InternalLink};
use parse::{OEmbed, Product};
use providers::{CardHint, Enrich};
use ssrf::{BodyRule, FetchError, FetchOptions, FetchPolicy, Fetched, TrustedHost, VettedTarget};

/// Key under the core settings KV (`core.settings` / `app`).
pub const SETTINGS_KEY: &str = "link_preview";

const OK_TTL: Duration = Duration::from_secs(6 * 3600);
const ERR_TTL: Duration = Duration::from_secs(10 * 60);
const PREVIEW_CACHE_ENTRIES: usize = 2000;
const IMAGE_CACHE_ENTRIES: usize = 500;
const IMAGE_CACHE_BYTES: usize = 128 * 1024 * 1024;
/// Outbound fetches per domain per minute (cache hits are free).
const RATE_PER_MINUTE: u32 = 30;
const MAX_TRUSTED_HOSTS: usize = 100;
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);

// ── Settings ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkPreviewSettings {
    /// Exact host names or IP/CIDR ranges that bypass the private-address
    /// block — for link-preview fetches only.
    #[serde(default)]
    pub trusted_internal_hosts: Vec<String>,
    /// Screenshot pages without an og:image (see module docs).
    #[serde(default)]
    pub screenshot_fallback: bool,
}

impl LinkPreviewSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.trusted_internal_hosts.len() > MAX_TRUSTED_HOSTS {
            return Err(format!("at most {MAX_TRUSTED_HOSTS} trusted hosts"));
        }
        for h in &self.trusted_internal_hosts {
            if TrustedHost::parse(h).is_none() {
                return Err(format!(
                    "invalid trusted host {h:?}: use a host name, IP, or CIDR"
                ));
            }
        }
        Ok(())
    }

    pub fn policy(&self) -> FetchPolicy {
        FetchPolicy::for_previews(
            self.trusted_internal_hosts
                .iter()
                .filter_map(|h| TrustedHost::parse(h))
                .collect(),
        )
    }
}

pub async fn load_settings(db: &Db) -> LinkPreviewSettings {
    use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
    let db = db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str(&json).unwrap_or_default(),
        _ => LinkPreviewSettings::default(),
    }
}

pub async fn save_settings(db: &Db, settings: &LinkPreviewSettings) -> anyhow::Result<()> {
    use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
    let db = db.clone();
    let value = serde_json::to_string(settings)?;
    tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY, &value)
    })
    .await??;
    // Trust changes alter what a cached result would be.
    service().clear();
    Ok(())
}

// ── Response shape ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Embed {
    pub iframe_src: String,
    /// `origin` | `srcdoc`.
    pub sandbox_mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Media {
    /// `image` | `video` | `audio` | `model`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub mime: String,
    /// Signed same-origin `/api/link-preview/image?k=media` URL streaming
    /// the file (video / audio / model) — set by [`Preview::for_client`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    /// The file to stream when it isn't the link itself (a PeerTube video,
    /// a provider's direct media URL).
    #[serde(skip)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Preview {
    pub url: String,
    pub final_url: String,
    /// `embed` | `card` | `media` | `internal`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed: Option<Embed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_ld_product: Option<Product>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal: Option<InternalLink>,
    /// Provider-specific extras: `type` plus the fields documented in
    /// [`apis`] (GitHub: state, author, counts, …; `post`: author, handle,
    /// avatar, text, thumbs; `event`: start, end, location; `asset`:
    /// download_url). `avatar` / `thumbs` are proxied like `image`, and a
    /// `download_url` gains a streamed `download_src`, in [`Preview::for_client`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    /// No og:image and the screenshot fallback is on: `image` becomes a
    /// signed screenshot URL when the preview is handed to a client.
    #[serde(skip)]
    pub screenshot: bool,
    /// The scraped page's `og:video` (embed-player discovery).
    #[serde(skip)]
    pub og_video: Option<String>,
}

impl Preview {
    /// The client copy: raw upstream image URLs swapped for signed proxy
    /// URLs, and streamed media given a signed `src` (re-signed per
    /// response, so cached previews never go stale).
    pub fn for_client(&self, secret: &[u8], now: u64) -> Preview {
        let mut p = self.clone();
        let img = |u: &str| image_proxy_url(secret, ImageKind::Fetch, u, now);
        p.image = match &self.image {
            Some(u) => Some(img(u)),
            None if self.screenshot => Some(image_proxy_url(
                secret,
                ImageKind::Screenshot,
                &self.final_url,
                now,
            )),
            None => None,
        };
        p.favicon = self.favicon.as_deref().map(img);
        if let Some(m) = &mut p.media
            && m.kind != "image"
        {
            let target = m.target.as_deref().unwrap_or(&self.final_url);
            m.src = Some(image_proxy_url(secret, ImageKind::Media, target, now));
        }
        if let Some(d) = p.details.as_mut().and_then(|d| d.as_object_mut()) {
            if let Some(a) = d.get("avatar").and_then(|v| v.as_str()).map(img) {
                d.insert("avatar".into(), a.into());
            }
            if let Some(serde_json::Value::Array(list)) = d.get_mut("thumbs") {
                for t in list.iter_mut() {
                    if let Some(u) = t.as_str().map(img) {
                        *t = u.into();
                    }
                }
            }
            if let Some(src) = d
                .get("download_url")
                .and_then(|v| v.as_str())
                .map(|u| image_proxy_url(secret, ImageKind::Media, u, now))
            {
                d.insert("download_src".into(), src.into());
            }
        }
        p
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewError {
    pub status: u16,
    pub message: String,
}

impl From<FetchError> for PreviewError {
    fn from(e: FetchError) -> Self {
        Self {
            status: e.http_status(),
            message: e.to_string(),
        }
    }
}

fn err(status: u16, message: impl Into<String>) -> PreviewError {
    PreviewError {
        status,
        message: message.into(),
    }
}

// ── Signed image-proxy URLs ───────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Fetch,
    Screenshot,
    /// A streamed audio / video / model file ([`LinkPreviewService::media_stream`]).
    Media,
}

impl ImageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "img",
            Self::Screenshot => "shot",
            Self::Media => "media",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "img" => Some(Self::Fetch),
            "shot" => Some(Self::Screenshot),
            "media" => Some(Self::Media),
            _ => None,
        }
    }
}

fn image_sig(secret: &[u8], kind: ImageKind, target: &str, exp: u64) -> String {
    let msg = format!("link-preview-image\n{}\n{exp}\n{target}", kind.as_str());
    hex::encode(crate::service::shared_files::hmac_sha256(
        secret,
        msg.as_bytes(),
    ))
}

/// Relative signed URL for the image proxy. `exp` is rounded to the hour
/// (valid 24–25 h) so the URL — and the browser's cache of it — is stable.
pub fn image_proxy_url(secret: &[u8], kind: ImageKind, target: &str, now: u64) -> String {
    let exp = (now / 3600 + 25) * 3600;
    format!(
        "/api/link-preview/image?k={}&u={}&exp={exp}&sig={}",
        kind.as_str(),
        urlencoding::encode(target),
        image_sig(secret, kind, target, exp)
    )
}

pub fn verify_image_sig(
    secret: &[u8],
    kind: ImageKind,
    target: &str,
    exp: u64,
    sig: &str,
    now: u64,
) -> bool {
    if exp < now {
        return false;
    }
    let want = image_sig(secret, kind, target, exp);
    let (a, b) = (want.as_bytes(), sig.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The raster type of `b` by magic bytes — never SVG (scriptable).
pub fn sniff_raster(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" && matches!(&b[8..12], b"avif" | b"avis") {
        Some("image/avif")
    } else if b.starts_with(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else if b.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

// ── Service ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProxiedImage {
    pub mime: &'static str,
    pub bytes: Bytes,
}
/// A streamed media response: upstream status (200 / 206) and headers, plus
/// the size-capped body.
pub struct MediaStream {
    pub status: u16,
    pub mime: String,
    pub content_length: Option<u64>,
    pub content_range: Option<String>,
    pub body: Body,
}

pub struct LinkPreviewService {
    previews: Mutex<TtlLru<Result<Preview, PreviewError>>>,
    images: Mutex<TtlLru<Result<ProxiedImage, PreviewError>>>,
    /// host → (window start, fetches in window).
    rate: Mutex<HashMap<String, (Instant, u32)>>,
}

pub fn service() -> &'static LinkPreviewService {
    static SVC: OnceLock<LinkPreviewService> = OnceLock::new();
    SVC.get_or_init(LinkPreviewService::new)
}

/// Cache key: fragment and tracking parameters dropped — except a map
/// position fragment (`#map=15/52.5/13.4`), which names what is shown.
fn cache_key(url: &Url) -> String {
    let mut u = url.clone();
    if !u.fragment().is_some_and(|f| f.starts_with("map=")) {
        u.set_fragment(None);
    }
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| {
            !k.starts_with("utm_") && !matches!(k.as_ref(), "fbclid" | "gclid" | "mc_eid")
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    u.to_string()
}

fn media_kind(ct: &str) -> Option<&'static str> {
    match ct.split('/').next()? {
        "image" => Some("image"),
        "video" => Some("video"),
        "audio" => Some("audio"),
        "model" => Some("model"),
        _ => None,
    }
}

const ICS_MAX_BYTES: usize = 1024 * 1024;

fn page_rule(ct: &str) -> BodyRule {
    if ct == "text/html" || ct == "application/xhtml+xml" {
        BodyRule::Read {
            max: ssrf::HTML_MAX_BYTES,
            truncate: true,
        }
    } else if ct == "text/calendar" {
        BodyRule::Read {
            max: ICS_MAX_BYTES,
            truncate: true,
        }
    } else if media_kind(ct).is_some() || MODEL_CTS.contains(&ct) {
        BodyRule::HeadersOnly
    } else {
        BodyRule::Refuse
    }
}

/// A `.ics` link: calendars are often served as text/plain or octet-stream.
fn ics_rule(ct: &str) -> BodyRule {
    if matches!(
        ct,
        "text/calendar" | "text/plain" | "application/octet-stream" | "application/ics" | ""
    ) {
        BodyRule::Read {
            max: ICS_MAX_BYTES,
            truncate: true,
        }
    } else {
        page_rule(ct)
    }
}

fn image_rule(ct: &str) -> BodyRule {
    // The bytes are sniffed afterwards; the header only filters the obvious.
    if (ct.starts_with("image/") && ct != "image/svg+xml")
        || ct == "application/octet-stream"
        || ct.is_empty()
    {
        BodyRule::Read {
            max: ssrf::IMAGE_MAX_BYTES,
            truncate: false,
        }
    } else {
        BodyRule::Refuse
    }
}

impl LinkPreviewService {
    pub fn new() -> Self {
        Self {
            previews: Mutex::new(TtlLru::new(PREVIEW_CACHE_ENTRIES, usize::MAX)),
            images: Mutex::new(TtlLru::new(IMAGE_CACHE_ENTRIES, IMAGE_CACHE_BYTES)),
            rate: Mutex::new(HashMap::new()),
        }
    }

    pub fn clear(&self) {
        self.previews.lock().unwrap().clear();
        self.images.lock().unwrap().clear();
    }

    fn take_rate_slot(&self, host: &str) -> Result<(), FetchError> {
        let now = Instant::now();
        let mut rate = self.rate.lock().unwrap();
        if rate.len() > 10_000 {
            rate.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(60));
        }
        let slot = rate.entry(host.to_string()).or_insert((now, 0));
        if now.duration_since(slot.0) >= Duration::from_secs(60) {
            *slot = (now, 0);
        }
        if slot.1 >= RATE_PER_MINUTE {
            return Err(FetchError::RateLimited(host.to_string()));
        }
        slot.1 += 1;
        Ok(())
    }

    /// Vet, rate-limit, fetch. The only path from this module to the network
    /// besides the screenshot sidecar call.
    pub(crate) async fn guarded_fetch(
        &self,
        raw: &str,
        policy: &FetchPolicy,
        opts: &FetchOptions<'_>,
    ) -> Result<Fetched, FetchError> {
        let target = VettedTarget::vet(raw, policy).await?;
        self.take_rate_slot(&target.host_key())?;
        ssrf::fetch(target, policy, opts).await
    }

    /// Build (or serve from cache) the preview for `raw`. `hosts` names this
    /// instance's own authorities, for `internal` detection. `privileged`
    /// (the requester is an admin) lets enrichment use the configured
    /// GitHub token; the two views are cached apart.
    pub async fn preview(
        &self,
        db: &Db,
        raw: &str,
        hosts: &InstanceHosts,
        privileged: bool,
    ) -> Result<Preview, PreviewError> {
        let url = Url::parse(raw.trim()).map_err(|e| err(400, format!("invalid url: {e}")))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(FetchError::Scheme.into());
        }
        if hosts.is_ours(&url) {
            return Ok(Preview {
                url: url.to_string(),
                final_url: url.to_string(),
                kind: "internal",
                internal: Some(internal::classify(&url)),
                ..Default::default()
            });
        }
        let key = if privileged {
            format!("admin {}", cache_key(&url))
        } else {
            cache_key(&url)
        };
        if let Some(hit) = self.previews.lock().unwrap().get(&key) {
            return hit;
        }
        let settings = load_settings(db).await;
        let res = self.build(db, &url, &settings, privileged).await;
        // A rate-limit refusal is transient — never pin it in the cache.
        if !matches!(&res, Err(e) if e.status == 429) {
            let ttl = if res.is_ok() { OK_TTL } else { ERR_TTL };
            self.previews
                .lock()
                .unwrap()
                .insert(key, res.clone(), ttl, 1);
        }
        res
    }

    async fn build(
        &self,
        db: &Db,
        url: &Url,
        settings: &LinkPreviewSettings,
        privileged: bool,
    ) -> Result<Preview, PreviewError> {
        let policy = settings.policy();
        let mut p = Preview {
            url: url.to_string(),
            final_url: url.to_string(),
            kind: "card",
            ..Default::default()
        };
        let Some(pm) = providers::match_url(url) else {
            // Any fediverse instance: a post-shaped path + a nodeinfo probe.
            if let Some(post) = fediverse::post_ref(url)
                && let Some(found) = fediverse::lookup(self, &policy, url, &post).await
            {
                return Ok(fediverse_preview(p, found));
            }
            return self
                .scrape(url, &policy, settings, p, CardHint::Plain)
                .await;
        };
        let provider = pm.provider;
        p.provider = Some(provider.id.to_string());
        p.site_name = Some(provider.name.to_string());
        let mut caps = pm.caps;
        match provider.enrich {
            Enrich::None => {}
            Enrich::BlueskyDid => {
                if let Some(did) = self.bluesky_did(&caps["handle"], &policy).await {
                    caps.insert("did".into(), did);
                }
            }
            Enrich::Osm => osm_caps(url, &mut caps),
            Enrich::OgEmbed => {
                // The player id is only in the page's `og:video`.
                let mut scraped = self
                    .scrape(url, &policy, settings, p, provider.card)
                    .await?;
                if let Some(spec) = &provider.embed
                    && let Some(player) = scraped
                        .og_video
                        .as_deref()
                        .and_then(|v| v.strip_prefix("https://bandcamp.com/EmbeddedPlayer/"))
                {
                    caps.insert("player".into(), player.trim_end_matches('/').to_string());
                    if let Some(src) = providers::embed_src(spec, &caps) {
                        scraped.kind = "embed";
                        scraped.embed = Some(embed_of(spec, src));
                    }
                }
                return Ok(scraped);
            }
            gh if gh.is_github() => {
                if let Some(card) = github::enrich(self, db, &policy, gh, &caps, privileged).await {
                    apply_card(&mut p, card);
                    p.favicon = Some("https://github.com/favicon.ico".into());
                    return Ok(p);
                }
            }
            kind => {
                if let Some(card) = apis::enrich(self, db, &policy, kind, &caps, privileged).await {
                    apply_card(&mut p, card);
                    return Ok(p);
                }
            }
        }
        if let Some(media) = &provider.media
            && let Some(src) = providers::render(media.src, &caps)
        {
            p.kind = "media";
            p.media = Some(Media {
                kind: media.kind,
                mime: mime_for(&src),
                src: None,
                target: Some(src.clone()),
            });
            if media.kind == "image" {
                p.image = Some(src);
            }
            p.title = Some(provider.name.to_string());
            return Ok(p);
        }
        if let Some(spec) = &provider.embed
            && let Some(src) = providers::embed_src(spec, &caps)
        {
            p.kind = "embed";
            p.embed = Some(embed_of(spec, src));
            // Metadata is best-effort: the embed works without it.
            if let Some(tmpl) = provider.oembed
                && let Some(endpoint) = providers::render(tmpl, &caps)
                && let Some(o) = self.fetch_oembed(&endpoint, &policy).await
            {
                apply_oembed(&mut p, &o);
            }
            return Ok(p);
        }
        // No embed/enrichment (API down, token absent, handle
        // unresolvable): scrape the page itself.
        match self
            .scrape(url, &policy, settings, p.clone(), provider.card)
            .await
        {
            Err(e) if provider.card == CardHint::Shop && e.status != 429 => {
                Ok(shop_fallback(p, url, settings))
            }
            res => res,
        }
    }

    async fn scrape(
        &self,
        url: &Url,
        policy: &FetchPolicy,
        settings: &LinkPreviewSettings,
        mut p: Preview,
        hint: CardHint,
    ) -> Result<Preview, PreviewError> {
        let is_ics = url.path().to_ascii_lowercase().ends_with(".ics");
        let fetched = self
            .guarded_fetch(
                url.as_str(),
                policy,
                &FetchOptions {
                    accept: "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5",
                    auth: None,
                    post_json: None,
                    body: if is_ics { ics_rule } else { page_rule },
                },
            )
            .await?;
        p.final_url = fetched.final_url.to_string();
        let is_html =
            fetched.content_type == "text/html" || fetched.content_type == "application/xhtml+xml";
        if fetched.content_type == "text/calendar" || (is_ics && !is_html) {
            let card = ics::event_card(&String::from_utf8_lossy(&fetched.body))
                .ok_or_else(|| err(422, "no event in the calendar"))?;
            apply_card(&mut p, card);
            p.provider.get_or_insert_with(|| "ics".into());
            return Ok(p);
        }
        if let Some((kind, mime)) = media_kind_for(&fetched.content_type, &fetched.final_url) {
            p.kind = "media";
            if kind == "image" && fetched.content_type != "image/svg+xml" {
                p.image = Some(p.final_url.clone());
            }
            p.media = Some(Media {
                kind,
                mime,
                src: None,
                target: None,
            });
            p.title = fetched
                .final_url
                .path_segments()
                .and_then(|mut s| s.next_back().map(str::to_string))
                .filter(|s| !s.is_empty())
                .map(|s| urlencoding::decode(&s).map(|c| c.into_owned()).unwrap_or(s));
            return Ok(p);
        }
        if !is_html {
            return Err(FetchError::ContentType(fetched.content_type).into());
        }
        let html = String::from_utf8_lossy(&fetched.body);
        let meta = parse::parse_html(&html, &fetched.final_url);
        p.title = p.title.or(meta.title);
        p.description = p.description.or(meta.description);
        p.site_name = meta.site_name.or(p.site_name);
        p.image = p.image.or(meta.image);
        p.favicon = p.favicon.or(meta.favicon);
        p.theme_color = meta.theme_color;
        p.json_ld_product = meta.product;
        p.og_video = meta.video;
        if hint == CardHint::Asset {
            p.details = Some(serde_json::json!({
                "type": "asset",
                "download_url": model_link(&html, &fetched.final_url),
            }));
        } else if p.provider.is_none()
            && let Some((id, name)) = lan::fingerprint(&html)
        {
            p.provider = Some(id.into());
            p.site_name = p.site_name.or_else(|| Some(name.into()));
            p.title = p.title.or_else(|| Some(name.into()));
            p.details = Some(serde_json::json!({ "type": "app", "app": id, "facts": [name] }));
        }
        if (p.title.is_none() || p.image.is_none())
            && let Some(endpoint) = meta.oembed_url
            && let Some(o) = self.fetch_oembed(&endpoint, policy).await
        {
            apply_oembed(&mut p, &o);
        }
        if p.image.is_none() && settings.screenshot_fallback {
            p.screenshot = true;
        }
        Ok(p)
    }

    async fn fetch_oembed(&self, endpoint: &str, policy: &FetchPolicy) -> Option<OEmbed> {
        let f = self
            .guarded_fetch(
                endpoint,
                policy,
                &FetchOptions::get("application/json", github::json_rule),
            )
            .await
            .ok()?;
        parse::parse_oembed(&f.body)
    }

    async fn bluesky_did(&self, handle: &str, policy: &FetchPolicy) -> Option<String> {
        let valid = |d: &str| {
            d.starts_with("did:")
                && d.len() <= 200
                && d.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
        };
        if handle.starts_with("did:") {
            return valid(handle).then(|| handle.to_string());
        }
        let endpoint = format!(
            "https://public.api.bsky.app/xrpc/com.atproto.identity.resolveHandle?handle={}",
            urlencoding::encode(handle)
        );
        let f = self
            .guarded_fetch(
                &endpoint,
                policy,
                &FetchOptions::get("application/json", github::json_rule),
            )
            .await
            .ok()?;
        let v: serde_json::Value = serde_json::from_slice(&f.body).ok()?;
        let did = v.get("did")?.as_str()?;
        valid(did).then(|| did.to_string())
    }

    /// Serve an image-proxy request (signature already verified by the
    /// route): a guarded fetch of `target`, or a screenshot of it.
    pub async fn image(
        &self,
        db: &Db,
        kind: ImageKind,
        target: &str,
    ) -> Result<ProxiedImage, PreviewError> {
        let key = format!("{}\n{target}", kind.as_str());
        if let Some(hit) = self.images.lock().unwrap().get(&key) {
            return hit;
        }
        let settings = load_settings(db).await;
        let policy = settings.policy();
        let res = match kind {
            ImageKind::Fetch => self.fetch_image(target, &policy).await,
            ImageKind::Screenshot if settings.screenshot_fallback => {
                self.screenshot(target, &policy).await
            }
            ImageKind::Screenshot => Err(err(404, "screenshot fallback is disabled")),
            ImageKind::Media => Err(err(400, "media is streamed, not proxied as an image")),
        };
        if !matches!(&res, Err(e) if e.status == 429) {
            let (ttl, weight) = match &res {
                Ok(img) => (OK_TTL, img.bytes.len()),
                Err(_) => (ERR_TTL, 0),
            };
            self.images
                .lock()
                .unwrap()
                .insert(key, res.clone(), ttl, weight);
        }
        res
    }

    async fn fetch_image(
        &self,
        target: &str,
        policy: &FetchPolicy,
    ) -> Result<ProxiedImage, PreviewError> {
        let f = self
            .guarded_fetch(
                target,
                policy,
                &FetchOptions::get(
                    "image/avif,image/webp,image/png,image/jpeg,image/gif,image/*;q=0.5",
                    image_rule,
                ),
            )
            .await?;
        let mime = sniff_raster(&f.body).ok_or_else(|| err(415, "not a raster image"))?;
        Ok(ProxiedImage {
            mime,
            bytes: Bytes::from(f.body),
        })
    }

    async fn screenshot(
        &self,
        target: &str,
        policy: &FetchPolicy,
    ) -> Result<ProxiedImage, PreviewError> {
        use crate::service::browser;
        use base64::Engine;
        let vetted = VettedTarget::vet(target, policy).await?;
        self.take_rate_slot(&vetted.host_key())?;
        let shot = tokio::time::timeout(SCREENSHOT_TIMEOUT, async {
            let created = browser::post(
                "/api/pages",
                serde_json::json!({
                    "name": "link-preview",
                    "description": "peckboard link preview screenshot",
                    "url": vetted.url().as_str(),
                }),
            )
            .await?;
            let page_id = created
                .get("pageId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("browser returned no pageId"))?
                .to_string();
            let shot = browser::post(
                &format!("/api/pages/{page_id}/screenshot"),
                serde_json::json!({ "fullPage": false }),
            )
            .await;
            let _ = browser::delete(&format!("/api/pages/{page_id}")).await;
            let shot = shot?;
            let b64 = shot
                .get("screenshot")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("browser returned no screenshot"))?;
            let b64 = b64.rsplit(',').next().unwrap_or(b64);
            Ok::<_, anyhow::Error>(base64::engine::general_purpose::STANDARD.decode(b64)?)
        })
        .await
        .map_err(|_| err(504, "screenshot timed out"))?
        .map_err(|e| err(502, format!("screenshot failed: {e}")))?;
        if shot.len() > ssrf::IMAGE_MAX_BYTES {
            return Err(FetchError::TooLarge(ssrf::IMAGE_MAX_BYTES).into());
        }
        let mime = sniff_raster(&shot).ok_or_else(|| err(502, "screenshot is not an image"))?;
        Ok(ProxiedImage {
            mime,
            bytes: Bytes::from(shot),
        })
    }

    /// Stream a remote audio / video / model file (signature already
    /// verified by the route): vetted like every fetch, `Range` passed
    /// through, type allowlisted, size capped per type. Not rate-limited —
    /// a player issues many range requests for one file.
    pub async fn media_stream(
        &self,
        db: &Db,
        target: &str,
        range: Option<&str>,
    ) -> Result<MediaStream, PreviewError> {
        let policy = load_settings(db).await.policy();
        self.media_stream_with(target, range, &policy, None).await
    }

    async fn media_stream_with(
        &self,
        target: &str,
        range: Option<&str>,
        policy: &FetchPolicy,
        cap_override: Option<u64>,
    ) -> Result<MediaStream, PreviewError> {
        static RANGE_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^bytes=[0-9]{0,15}-[0-9]{0,15}$").unwrap());
        let range = range.filter(|r| RANGE_RE.is_match(r));
        let vetted = VettedTarget::vet(target, policy).await?;
        let up = ssrf::fetch_stream(
            vetted,
            policy,
            "video/*,audio/*,model/*,application/octet-stream;q=0.5",
            range,
        )
        .await?;
        let (mime, cap) = media_allowance(&up.content_type, &up.final_url)
            .ok_or_else(|| PreviewError::from(FetchError::ContentType(up.content_type.clone())))?;
        let cap = cap_override.unwrap_or(cap);
        let total = up
            .content_range
            .as_deref()
            .and_then(|r| r.rsplit('/').next())
            .and_then(|t| t.parse::<u64>().ok());
        if up.content_length.is_some_and(|n| n > cap) || total.is_some_and(|n| n > cap) {
            return Err(err(413, format!("media exceeds {cap} bytes")));
        }
        let mut seen = 0u64;
        let stream = up.response.bytes_stream().map(move |chunk| {
            let chunk = chunk.map_err(std::io::Error::other)?;
            seen += chunk.len() as u64;
            if seen > cap {
                return Err(std::io::Error::other("media exceeds its size cap"));
            }
            Ok(chunk)
        });
        Ok(MediaStream {
            status: up.status,
            mime,
            content_length: up.content_length,
            content_range: up.content_range,
            body: Body::from_stream(stream),
        })
    }
}

impl Default for LinkPreviewService {
    fn default() -> Self {
        Self::new()
    }
}

fn apply_oembed(p: &mut Preview, o: &OEmbed) {
    if p.title.is_none() {
        p.title = o.title.clone().or_else(|| o.author_name.clone());
    }
    if p.description.is_none()
        && let Some(a) = &o.author_name
        && p.title.as_deref() != Some(a)
    {
        p.description = Some(format!("by {a}"));
    }
    if p.site_name.is_none() {
        p.site_name = o.provider_name.clone();
    }
    if p.image.is_none() {
        p.image = o.thumbnail_url.clone();
    }
}
fn embed_of(spec: &providers::EmbedSpec, src: String) -> Embed {
    Embed {
        iframe_src: src,
        sandbox_mode: spec.sandbox.as_str(),
        width: spec.width,
        height: spec.height,
        aspect: spec.aspect,
    }
}

fn apply_card(p: &mut Preview, card: github::GhCard) {
    p.title = card.title.or(p.title.take());
    p.description = card.description.or(p.description.take());
    p.image = card.image.or(p.image.take());
    p.details = Some(card.details);
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn fediverse_preview(mut p: Preview, f: fediverse::FediPost) -> Preview {
    p.provider = Some(f.software.clone());
    p.site_name = Some(title_case(&f.software));
    apply_card(&mut p, f.card);
    if let Some((file, mime)) = f.video {
        p.kind = "media";
        p.media = Some(Media {
            kind: "video",
            mime,
            src: None,
            target: Some(file),
        });
    }
    p
}

/// A shop refused the scrape (bot wall): a plain card named after the URL
/// slug, with the screenshot fallback when it is on.
fn shop_fallback(mut p: Preview, url: &Url, settings: &LinkPreviewSettings) -> Preview {
    p.title = url
        .path_segments()
        .and_then(|segs| {
            segs.filter(|s| s.len() > 6 && s.contains('-') && s.chars().any(char::is_alphabetic))
                .max_by_key(|s| s.len())
                .map(|s| {
                    urlencoding::decode(s)
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| s.to_string())
                        .replace('-', " ")
                })
        })
        .and_then(|t| parse::clean(&t, 200))
        .or_else(|| p.site_name.clone());
    p.screenshot = settings.screenshot_fallback;
    p
}

/// OpenStreetMap: `bbox` + `marker` from `?mlat=&mlon=[&zoom=]` or the
/// `#map=zoom/lat/lon` fragment.
fn osm_caps(url: &Url, caps: &mut BTreeMap<String, String>) {
    let q = |k: &str| {
        url.query_pairs()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.into_owned())
    };
    let (zoom, lat, lon) = match (q("mlat"), q("mlon")) {
        (Some(lat), Some(lon)) => (q("zoom").unwrap_or_else(|| "15".into()), lat, lon),
        _ => {
            let Some(map) = url
                .fragment()
                .and_then(|f| f.split('&').find_map(|kv| kv.strip_prefix("map=")))
            else {
                return;
            };
            let parts: Vec<&str> = map.split('/').collect();
            let [z, lat, lon] = parts[..] else { return };
            (z.to_string(), lat.to_string(), lon.to_string())
        }
    };
    let (Ok(z), Ok(lat), Ok(lon)) = (zoom.parse::<f64>(), lat.parse::<f64>(), lon.parse::<f64>())
    else {
        return;
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return;
    }
    let d = 180.0 / 2f64.powf(z.clamp(1.0, 19.0));
    caps.insert(
        "bbox".into(),
        format!(
            "{:.5},{:.5},{:.5},{:.5}",
            lon - d,
            lat - d / 2.0,
            lon + d,
            lat + d / 2.0
        ),
    );
    caps.insert("marker".into(), format!("{lat:.5},{lon:.5}"));
}

const MODEL_CTS: &[&str] = &[
    "application/octet-stream",
    "binary/octet-stream",
    "application/sla",
    "application/vnd.ms-pki.stl",
    "application/vnd.ms-package.3dmanufacturing-3dmodel+xml",
];

fn ext_of(path: &str) -> Option<String> {
    let last = path.rsplit('/').next()?;
    last.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
}

fn model_mime_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "stl" => "model/stl",
        "3mf" => "model/3mf",
        "glb" => "model/gltf-binary",
        "gltf" => "model/gltf+json",
        "obj" => "model/obj",
        "ply" => "model/ply",
        _ => return None,
    })
}

/// `(kind, mime)` of a media response. Model files usually arrive as
/// `application/octet-stream`, so their extension decides.
fn media_kind_for(ct: &str, url: &Url) -> Option<(&'static str, String)> {
    if let Some(k) = media_kind(ct) {
        return Some((k, ct.to_string()));
    }
    if (MODEL_CTS.contains(&ct) || ct.is_empty())
        && let Some(m) = ext_of(url.path()).and_then(|e| model_mime_for_ext(&e))
    {
        return Some(("model", m.to_string()));
    }
    None
}

/// Rough mime of a provider's direct media URL, by extension.
fn mime_for(src: &str) -> String {
    let path = src.split(['?', '#']).next().unwrap_or(src);
    match ext_of(path).as_deref() {
        Some("gif") => "image/gif",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("mp4") => "video/mp4",
        Some(e) => model_mime_for_ext(e).unwrap_or("application/octet-stream"),
        None => "application/octet-stream",
    }
    .to_string()
}

/// The first direct 3D-model file an asset page links to.
fn model_link(html: &str, base: &Url) -> Option<String> {
    static MODEL_LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?:href|src|data-src|data-url)\s*=\s*["']([^"'<>\s]+?\.(?:stl|3mf|glb|gltf|obj)(?:\?[^"'<>\s]*)?)["']"#,
        )
        .unwrap()
    });
    MODEL_LINK
        .captures(html)
        .and_then(|c| parse::absolutize(base, &parse::decode_entities(&c[1])))
}

const VIDEO_MAX_BYTES: u64 = 500 * 1024 * 1024;
const AUDIO_MAX_BYTES: u64 = 100 * 1024 * 1024;
const MODEL_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// `(served mime, size cap)` for an upstream media response, or `None`
/// when its type isn't on the allowlist.
fn media_allowance(ct: &str, url: &Url) -> Option<(String, u64)> {
    const VIDEO: &[&str] = &[
        "video/mp4",
        "video/webm",
        "video/ogg",
        "video/quicktime",
        "video/x-m4v",
    ];
    const AUDIO: &[&str] = &[
        "audio/mpeg",
        "audio/mp3",
        "audio/mp4",
        "audio/aac",
        "audio/ogg",
        "audio/opus",
        "audio/wav",
        "audio/x-wav",
        "audio/wave",
        "audio/webm",
        "audio/flac",
        "audio/x-flac",
        "audio/x-m4a",
    ];
    const MODEL: &[&str] = &[
        "model/gltf-binary",
        "model/gltf+json",
        "model/stl",
        "model/x.stl-binary",
        "model/x.stl-ascii",
        "model/obj",
        "model/3mf",
        "model/ply",
    ];
    if VIDEO.contains(&ct) {
        return Some((ct.to_string(), VIDEO_MAX_BYTES));
    }
    if AUDIO.contains(&ct) {
        return Some((ct.to_string(), AUDIO_MAX_BYTES));
    }
    if MODEL.contains(&ct) {
        return Some((ct.to_string(), MODEL_MAX_BYTES));
    }
    match media_kind_for(ct, url) {
        Some(("model", mime)) => Some((mime, MODEL_MAX_BYTES)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::header, routing::get};

    async fn serve(app: Router) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    async fn db_trusting(entries: &[&str]) -> Db {
        let db = Db::in_memory().unwrap();
        save_settings(
            &db,
            &LinkPreviewSettings {
                trusted_internal_hosts: entries.iter().map(|s| s.to_string()).collect(),
                screenshot_fallback: false,
            },
        )
        .await
        .unwrap();
        db
    }

    fn page_app() -> Router {
        Router::new()
            .route(
                "/article",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        r#"<html><head><title>T</title>
                        <meta property="og:title" content="Hello">
                        <meta property="og:image" content="/hero.png">
                        <link rel="alternate" type="application/json+oembed" href="/oembed"></head></html>"#,
                    )
                }),
            )
            .route(
                "/bare",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        r#"<title>Bare</title><link rel="alternate" type="application/json+oembed" href="/oembed">"#,
                    )
                }),
            )
            .route(
                "/oembed",
                get(|| async {
                    Json(serde_json::json!({"title":"OE","thumbnail_url":"https://cdn.example/t.jpg"}))
                }),
            )
            .route("/hero.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], PNG) }))
            .route(
                "/fake.png",
                get(|| async { ([(header::CONTENT_TYPE, "image/png")], "<svg onload=x>") }),
            )
            .route(
                "/repos/o/r/pulls/5",
                get(|| async {
                    Json(serde_json::json!({
                        "number": 5, "title": "Fix the thing", "body": "Long body",
                        "state": "open", "merged": false, "draft": false,
                        "user": {"login": "octo", "avatar_url": "https://avatars.example/u/1"},
                        "additions": 10, "deletions": 2, "changed_files": 1, "comments": 3
                    }))
                }),
            )
    }

    #[tokio::test]
    async fn scrapes_cards_media_and_oembed_through_the_guard() {
        let port = serve(page_app()).await;
        let db = db_trusting(&["localhost"]).await;
        let svc = LinkPreviewService::new();
        let hosts = InstanceHosts::default();
        let base = format!("http://localhost:{port}");

        let p = svc
            .preview(&db, &format!("{base}/article#frag"), &hosts, false)
            .await
            .unwrap();
        assert_eq!(p.kind, "card");
        assert_eq!(p.title.as_deref(), Some("Hello"));
        assert_eq!(p.image.as_deref(), Some(&*format!("{base}/hero.png")));

        // Missing og:image → oEmbed discovery fills it.
        let p = svc
            .preview(&db, &format!("{base}/bare"), &hosts, false)
            .await
            .unwrap();
        assert_eq!(p.title.as_deref(), Some("Bare"));
        assert_eq!(p.image.as_deref(), Some("https://cdn.example/t.jpg"));

        let p = svc
            .preview(&db, &format!("{base}/hero.png"), &hosts, false)
            .await
            .unwrap();
        assert_eq!(p.kind, "media");
        assert_eq!(
            p.media,
            Some(Media {
                kind: "image",
                mime: "image/png".into(),
                src: None,
                target: None,
            })
        );

        // The proxy sniffs bytes: a PNG label on non-PNG bytes is refused.
        let img = svc
            .image(&db, ImageKind::Fetch, &format!("{base}/hero.png"))
            .await
            .unwrap();
        assert_eq!(img.mime, "image/png");
        let bad = svc
            .image(&db, ImageKind::Fetch, &format!("{base}/fake.png"))
            .await
            .unwrap_err();
        assert_eq!(bad.status, 415);
        // Screenshot fallback is off by default.
        let off = svc
            .image(&db, ImageKind::Screenshot, &format!("{base}/bare"))
            .await
            .unwrap_err();
        assert_eq!(off.status, 404);
    }

    #[tokio::test]
    async fn private_targets_are_refused_without_trust() {
        let port = serve(page_app()).await;
        let db = Db::in_memory().unwrap();
        let svc = LinkPreviewService::new();
        let e = svc
            .preview(
                &db,
                &format!("http://localhost:{port}/article"),
                &InstanceHosts::default(),
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(e.status, 403);
        let e = svc
            .preview(&db, "file:///etc/passwd", &InstanceHosts::default(), false)
            .await
            .unwrap_err();
        assert_eq!(e.status, 400);
    }

    #[tokio::test]
    async fn internal_links_are_classified_without_fetching() {
        let db = Db::in_memory().unwrap();
        let hosts = InstanceHosts::new(Some("board.lan:3344"), &[3344]);
        let p = service()
            .preview(&db, "http://board.lan:3344/sessions/s-1", &hosts, false)
            .await
            .unwrap();
        assert_eq!(p.kind, "internal");
        assert_eq!(p.internal.unwrap().id.as_deref(), Some("s-1"));
        assert!(p.title.is_none());
    }

    #[tokio::test]
    async fn github_pull_requests_enrich_from_the_api() {
        let port = serve(page_app()).await;
        let policy = FetchPolicy::for_previews(vec![TrustedHost::parse("localhost").unwrap()]);
        let svc = LinkPreviewService::new();
        let pm =
            providers::match_url(&Url::parse("https://github.com/o/r/pull/5").unwrap()).unwrap();
        let card = github::enrich_with(
            &svc,
            &policy,
            &format!("http://localhost:{port}"),
            None,
            pm.provider.enrich,
            &pm.caps,
        )
        .await
        .unwrap();
        assert_eq!(card.title.as_deref(), Some("Fix the thing"));
        assert_eq!(card.details["author"], "octo");
        assert_eq!(card.details["additions"], 10);
        assert_eq!(card.details["type"], "pull");
    }

    #[test]
    fn rate_limit_is_per_domain() {
        let svc = LinkPreviewService::new();
        for _ in 0..RATE_PER_MINUTE {
            svc.take_rate_slot("a.example").unwrap();
        }
        assert!(matches!(
            svc.take_rate_slot("a.example"),
            Err(FetchError::RateLimited(_))
        ));
        assert!(svc.take_rate_slot("b.example").is_ok());
    }

    #[test]
    fn image_urls_are_signed_and_proxied() {
        let secret = b"k";
        let p = Preview {
            url: "https://e.com/".into(),
            final_url: "https://e.com/".into(),
            kind: "card",
            image: Some("https://cdn.e.com/a.png?x=1&y=2".into()),
            ..Default::default()
        };
        let c = p.for_client(secret, 1_000);
        let img = c.image.unwrap();
        assert!(img.starts_with(
            "/api/link-preview/image?k=img&u=https%3A%2F%2Fcdn.e.com%2Fa.png%3Fx%3D1%26y%3D2&exp="
        ));
        let exp: u64 = img
            .split("exp=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let sig = img.split("sig=").nth(1).unwrap();
        assert!(verify_image_sig(
            secret,
            ImageKind::Fetch,
            "https://cdn.e.com/a.png?x=1&y=2",
            exp,
            sig,
            1_000
        ));
        assert!(!verify_image_sig(
            secret,
            ImageKind::Screenshot,
            "https://cdn.e.com/a.png?x=1&y=2",
            exp,
            sig,
            1_000
        ));
        assert!(!verify_image_sig(
            secret,
            ImageKind::Fetch,
            "https://evil/",
            exp,
            sig,
            1_000
        ));
        assert!(!verify_image_sig(
            secret,
            ImageKind::Fetch,
            "https://cdn.e.com/a.png?x=1&y=2",
            exp,
            sig,
            exp + 1
        ));
        assert_eq!(
            cache_key(&Url::parse("https://E.com/a?utm_source=x#f").unwrap()),
            "https://e.com/a"
        );
    }

    fn fedi_app() -> Router {
        use axum::http::HeaderMap;
        Router::new()
            .route(
                "/.well-known/nodeinfo",
                get(|headers: HeaderMap| async move {
                    let host = headers[header::HOST].to_str().unwrap().to_string();
                    Json(serde_json::json!({"links": [{
                        "rel": "http://nodeinfo.diaspora.software/ns/schema/2.0",
                        "href": format!("http://{host}/nodeinfo/2.0"),
                    }]}))
                }),
            )
            .route(
                "/nodeinfo/2.0",
                get(|| async { Json(serde_json::json!({"software": {"name": "mastodon"}})) }),
            )
            .route(
                "/api/v1/statuses/123",
                get(|| async {
                    Json(serde_json::json!({
                        "content": "<p>Hello fediverse</p>", "created_at": "2026-01-01T00:00:00Z",
                        "account": {"acct": "alice", "display_name": "Alice", "avatar": "https://cdn.example/a.png"},
                        "media_attachments": [], "replies_count": 1, "reblogs_count": 0, "favourites_count": 2
                    }))
                }),
            )
            .route(
                "/event.ics",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/calendar")],
                        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Standup\r\nDTSTART:20261102T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                    )
                }),
            )
            .route(
                "/a.mp3",
                get(|headers: HeaderMap| async move {
                    use axum::response::IntoResponse;
                    let body = vec![7u8; 100];
                    match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
                        Some("bytes=10-19") => (
                            axum::http::StatusCode::PARTIAL_CONTENT,
                            [
                                (header::CONTENT_TYPE, "audio/mpeg"),
                                (header::CONTENT_RANGE, "bytes 10-19/100"),
                            ],
                            body[10..20].to_vec(),
                        )
                            .into_response(),
                        _ => ([(header::CONTENT_TYPE, "audio/mpeg")], body).into_response(),
                    }
                }),
            )
            .route(
                "/page.html",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<p>x</p>") }),
            )
    }

    #[tokio::test]
    async fn fediverse_posts_and_calendar_events_become_cards() {
        let port = serve(fedi_app()).await;
        let db = db_trusting(&["localhost"]).await;
        let svc = LinkPreviewService::new();
        let hosts = InstanceHosts::default();
        let base = format!("http://localhost:{port}");

        let p = svc
            .preview(&db, &format!("{base}/@alice/123"), &hosts, false)
            .await
            .unwrap();
        assert_eq!(p.kind, "card");
        assert_eq!(p.provider.as_deref(), Some("mastodon"));
        let d = p.details.as_ref().unwrap();
        assert_eq!(d["type"], "post");
        assert_eq!(d["handle"], "@alice@localhost");
        assert_eq!(d["text"], "Hello fediverse");
        // The avatar is proxied like any image.
        let c = p.for_client(b"k", 1_000);
        assert!(
            c.details.unwrap()["avatar"]
                .as_str()
                .unwrap()
                .starts_with("/api/link-preview/image?k=img&")
        );

        let p = svc
            .preview(&db, &format!("{base}/event.ics"), &hosts, false)
            .await
            .unwrap();
        assert_eq!(p.title.as_deref(), Some("Standup"));
        let d = p.details.unwrap();
        assert_eq!(d["type"], "event");
        assert_eq!(d["start"], "2026-11-02T09:00:00Z");

        // A remote audio file: media with a signed stream URL.
        let p = svc
            .preview(&db, &format!("{base}/a.mp3"), &hosts, false)
            .await
            .unwrap();
        assert_eq!(p.kind, "media");
        let m = p.for_client(b"k", 1_000).media.unwrap();
        assert_eq!(m.kind, "audio");
        assert!(
            m.src
                .unwrap()
                .starts_with("/api/link-preview/image?k=media&")
        );
    }

    #[tokio::test]
    async fn media_proxy_streams_ranges_within_caps_through_the_guard() {
        let port = serve(fedi_app()).await;
        let svc = LinkPreviewService::new();
        let trusted = FetchPolicy::for_previews(vec![TrustedHost::parse("localhost").unwrap()]);
        let url = |p: &str| format!("http://localhost:{port}{p}");

        let full = svc
            .media_stream_with(&url("/a.mp3"), None, &trusted, None)
            .await
            .unwrap();
        assert_eq!((full.status, full.mime.as_str()), (200, "audio/mpeg"));
        assert_eq!(full.content_length, Some(100));
        let bytes = axum::body::to_bytes(full.body, 1 << 20).await.unwrap();
        assert_eq!(bytes.len(), 100);

        let part = svc
            .media_stream_with(&url("/a.mp3"), Some("bytes=10-19"), &trusted, None)
            .await
            .unwrap();
        assert_eq!(part.status, 206);
        assert_eq!(part.content_range.as_deref(), Some("bytes 10-19/100"));
        assert_eq!(
            axum::body::to_bytes(part.body, 1 << 20)
                .await
                .unwrap()
                .len(),
            10
        );

        // A malformed / multi-range header is dropped, not forwarded.
        let multi = svc
            .media_stream_with(&url("/a.mp3"), Some("bytes=0-1,5-6"), &trusted, None)
            .await
            .unwrap();
        assert_eq!(multi.status, 200);

        // Over the cap (judged on the full size, even for a range).
        for range in [None, Some("bytes=10-19")] {
            let e = svc
                .media_stream_with(&url("/a.mp3"), range, &trusted, Some(50))
                .await
                .err()
                .unwrap();
            assert_eq!(e.status, 413);
        }
        // Not on the allowlist.
        let e = svc
            .media_stream_with(&url("/page.html"), None, &trusted, None)
            .await
            .err()
            .unwrap();
        assert_eq!(e.status, 415);
        // The SSRF guard: loopback is refused without trust.
        let e = svc
            .media_stream_with(&url("/a.mp3"), None, &FetchPolicy::strict(), None)
            .await
            .err()
            .unwrap();
        assert_eq!(e.status, 403);
    }
}
