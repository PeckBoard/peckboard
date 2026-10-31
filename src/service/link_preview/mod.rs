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
//! Screenshot fallback (setting, default **off**): when a page has no
//! og:image the preview may carry a screenshot taken by the headless browser
//! sidecar (`service::browser`). Only the top-level URL is vetted — Chrome
//! resolves DNS itself and follows its own redirects and subresource loads,
//! none of which the guard can see — so enabling it lets a previewed page
//! make the server's browser touch internal addresses. Hence opt-in.

mod cache;
mod github;
pub mod internal;
pub mod parse;
pub mod providers;
pub mod ssrf;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use cache::TtlLru;
use internal::{InstanceHosts, InternalLink};
use parse::{OEmbed, Product};
use providers::Enrich;
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
    /// Provider-specific extras (GitHub: state, author, counts, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    /// No og:image and the screenshot fallback is on: `image` becomes a
    /// signed screenshot URL when the preview is handed to a client.
    #[serde(skip)]
    pub screenshot: bool,
}

impl Preview {
    /// The client copy: raw upstream image URLs swapped for signed proxy
    /// URLs (re-signed per response, so cached previews never go stale).
    pub fn for_client(&self, secret: &[u8], now: u64) -> Preview {
        let mut p = self.clone();
        p.image = match &self.image {
            Some(u) => Some(image_proxy_url(secret, ImageKind::Fetch, u, now)),
            None if self.screenshot => Some(image_proxy_url(
                secret,
                ImageKind::Screenshot,
                &self.final_url,
                now,
            )),
            None => None,
        };
        p.favicon = self
            .favicon
            .as_ref()
            .map(|u| image_proxy_url(secret, ImageKind::Fetch, u, now));
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
}

impl ImageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "img",
            Self::Screenshot => "shot",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "img" => Some(Self::Fetch),
            "shot" => Some(Self::Screenshot),
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

/// Cache key: fragment and tracking parameters dropped.
fn cache_key(url: &Url) -> String {
    let mut u = url.clone();
    u.set_fragment(None);
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

fn page_rule(ct: &str) -> BodyRule {
    if ct == "text/html" || ct == "application/xhtml+xml" {
        BodyRule::Read {
            max: ssrf::HTML_MAX_BYTES,
            truncate: true,
        }
    } else if media_kind(ct).is_some() {
        BodyRule::HeadersOnly
    } else {
        BodyRule::Refuse
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
        if let Some(pm) = providers::match_url(url) {
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
                gh => {
                    if let Some(card) =
                        github::enrich(self, db, &policy, gh, &caps, privileged).await
                    {
                        p.title = card.title;
                        p.description = card.description;
                        p.image = card.image;
                        p.favicon = Some("https://github.com/favicon.ico".into());
                        p.details = Some(card.details);
                        return Ok(p);
                    }
                }
            }
            if let Some(spec) = &provider.embed
                && let Some(src) = providers::embed_src(spec, &caps)
            {
                p.kind = "embed";
                p.embed = Some(Embed {
                    iframe_src: src,
                    sandbox_mode: spec.sandbox.as_str(),
                    width: spec.width,
                    height: spec.height,
                    aspect: spec.aspect,
                });
                // Metadata is best-effort: the embed works without it.
                if let Some(tmpl) = provider.oembed {
                    let mut c = caps.clone();
                    c.insert("url".into(), url.to_string());
                    if let Some(endpoint) = providers::render(tmpl, &c)
                        && let Some(o) = self.fetch_oembed(&endpoint, &policy).await
                    {
                        apply_oembed(&mut p, &o);
                    }
                }
                return Ok(p);
            }
            // No embed/enrichment (API down, handle unresolvable): scrape.
        }
        self.scrape(url, &policy, settings, p).await
    }

    async fn scrape(
        &self,
        url: &Url,
        policy: &FetchPolicy,
        settings: &LinkPreviewSettings,
        mut p: Preview,
    ) -> Result<Preview, PreviewError> {
        let fetched = self
            .guarded_fetch(
                url.as_str(),
                policy,
                &FetchOptions {
                    accept: "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5",
                    bearer: None,
                    body: page_rule,
                },
            )
            .await?;
        p.final_url = fetched.final_url.to_string();
        if let Some(kind) = media_kind(&fetched.content_type) {
            p.kind = "media";
            p.media = Some(Media {
                kind,
                mime: fetched.content_type.clone(),
            });
            if kind == "image" && fetched.content_type != "image/svg+xml" {
                p.image = Some(p.final_url.clone());
            }
            p.title = fetched
                .final_url
                .path_segments()
                .and_then(|mut s| s.next_back().map(str::to_string))
                .filter(|s| !s.is_empty())
                .map(|s| urlencoding::decode(&s).map(|c| c.into_owned()).unwrap_or(s));
            return Ok(p);
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
                &FetchOptions {
                    accept: "application/json",
                    bearer: None,
                    body: github::json_rule,
                },
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
                &FetchOptions {
                    accept: "application/json",
                    bearer: None,
                    body: github::json_rule,
                },
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
                &FetchOptions {
                    accept: "image/avif,image/webp,image/png,image/jpeg,image/gif,image/*;q=0.5",
                    bearer: None,
                    body: image_rule,
                },
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
                mime: "image/png".into()
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
}
