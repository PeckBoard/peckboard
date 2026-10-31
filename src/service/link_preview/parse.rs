//! Metadata extraction from a fetched HTML page: Open Graph, Twitter Card,
//! `<title>`, meta description, favicon, theme-color, oEmbed discovery
//! links, and schema.org `Product` JSON-LD. Regex-scanned rather than
//! DOM-parsed — previews need a handful of head tags, not a tree.

use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;
use serde::Serialize;

const MAX_TITLE: usize = 300;
const MAX_DESCRIPTION: usize = 1000;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Product {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_count: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageMeta {
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
    /// Absolute http(s) URLs (resolved against the page URL).
    pub image: Option<String>,
    pub favicon: Option<String>,
    pub theme_color: Option<String>,
    pub oembed_url: Option<String>,
    pub product: Option<Product>,
    /// `og:video` (absolute): an embed player URL on some sites (Bandcamp).
    pub video: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OEmbed {
    pub title: Option<String>,
    pub author_name: Option<String>,
    pub provider_name: Option<String>,
    pub thumbnail_url: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<(meta|link)\b((?:[^>"']|"[^"]*"|'[^']*')*)>"#).unwrap());
static ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)([a-zA-Z_:][-a-zA-Z0-9_:.]*)\s*(?:=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?"#,
    )
    .unwrap()
});
static TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>").unwrap());
static SCRIPT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<script\b((?:[^>"']|"[^"]*"|'[^']*')*)>(.*?)</script\s*>"#).unwrap()
});
static ENTITY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z]{2,8});").unwrap());

fn attrs(raw: &str) -> Vec<(String, String)> {
    ATTR_RE
        .captures_iter(raw)
        .map(|c| {
            let v = c
                .get(2)
                .or_else(|| c.get(3))
                .or_else(|| c.get(4))
                .map(|m| m.as_str())
                .unwrap_or("");
            (c[1].to_ascii_lowercase(), decode_entities(v))
        })
        .collect()
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    ENTITY_RE
        .replace_all(s, |c: &regex::Captures| {
            let e = &c[1];
            let ch = if let Some(hex) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = e.strip_prefix('#') {
                dec.parse().ok().and_then(char::from_u32)
            } else {
                match e {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{a0}'),
                    "mdash" => Some('—'),
                    "ndash" => Some('–'),
                    "hellip" => Some('…'),
                    "middot" => Some('·'),
                    _ => None,
                }
            };
            ch.map(String::from).unwrap_or_else(|| c[0].to_string())
        })
        .into_owned()
}

/// Collapse whitespace, trim, and cap to `max` chars; `None` when empty.
pub(crate) fn clean(s: &str, max: usize) -> Option<String> {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= max {
        return Some(collapsed);
    }
    let mut out: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    Some(out)
}

/// Resolve `href` against `base`, keeping only http(s) results.
pub fn absolutize(base: &Url, href: &str) -> Option<String> {
    let u = base.join(href.trim()).ok()?;
    matches!(u.scheme(), "http" | "https").then(|| u.to_string())
}

fn first<'a>(meta: &'a [(String, String)], keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| {
        meta.iter()
            .find(|(name, v)| name == k && !v.trim().is_empty())
            .map(|(_, v)| v.as_str())
    })
}

pub fn parse_html(html: &str, base: &Url) -> PageMeta {
    // (property-or-name, content) for every <meta>, in document order.
    let mut meta: Vec<(String, String)> = Vec::new();
    let mut icons: Vec<(String, String)> = Vec::new(); // (rel, href)
    let mut oembed_url = None;
    for cap in TAG_RE.captures_iter(html) {
        let a = attrs(&cap[2]);
        if cap[1].eq_ignore_ascii_case("meta") {
            let key = attr(&a, "property")
                .or_else(|| attr(&a, "name"))
                .or_else(|| attr(&a, "itemprop"));
            if let (Some(k), Some(v)) = (key, attr(&a, "content")) {
                meta.push((k.trim().to_ascii_lowercase(), v.to_string()));
            }
        } else {
            let rel = attr(&a, "rel").unwrap_or("").to_ascii_lowercase();
            let Some(href) = attr(&a, "href") else {
                continue;
            };
            let rels: Vec<&str> = rel.split_whitespace().collect();
            if rels.contains(&"alternate")
                && attr(&a, "type")
                    .is_some_and(|t| t.eq_ignore_ascii_case("application/json+oembed"))
                && oembed_url.is_none()
            {
                oembed_url = absolutize(base, href);
            } else if rels
                .iter()
                .any(|r| *r == "icon" || r.starts_with("apple-touch-icon"))
            {
                icons.push((rel.clone(), href.to_string()));
            }
        }
    }

    let title_tag = TITLE_RE
        .captures(html)
        .map(|c| decode_entities(&c[1]))
        .and_then(|t| clean(&t, MAX_TITLE));
    let product = parse_json_ld_product(html);

    let title = first(&meta, &["og:title", "twitter:title"])
        .and_then(|t| clean(t, MAX_TITLE))
        .or(title_tag)
        .or_else(|| product.as_ref().and_then(|p| p.name.clone()));
    let description = first(
        &meta,
        &["og:description", "twitter:description", "description"],
    )
    .and_then(|d| clean(d, MAX_DESCRIPTION));
    let site_name =
        first(&meta, &["og:site_name", "application-name"]).and_then(|s| clean(s, MAX_TITLE));
    let image = first(
        &meta,
        &[
            "og:image:secure_url",
            "og:image",
            "og:image:url",
            "twitter:image",
            "twitter:image:src",
        ],
    )
    .and_then(|i| absolutize(base, i));
    let theme_color = first(&meta, &["theme-color"])
        .map(str::trim)
        .filter(|c| is_css_color(c))
        .map(str::to_string);
    // Prefer a plain `icon`, then `shortcut icon`, then apple-touch-icon;
    // fall back to the conventional /favicon.ico.
    let favicon = icons
        .iter()
        .min_by_key(|(rel, _)| match rel.as_str() {
            "icon" => 0,
            r if r.contains("icon") && !r.contains("apple") => 1,
            _ => 2,
        })
        .and_then(|(_, href)| absolutize(base, href))
        .or_else(|| absolutize(base, "/favicon.ico"));

    let video = first(&meta, &["og:video:secure_url", "og:video:url", "og:video"])
        .and_then(|v| absolutize(base, v));

    PageMeta {
        title,
        description,
        site_name,
        image,
        favicon,
        theme_color,
        oembed_url,
        product,
        video,
    }
}

/// A theme-color the UI can drop into CSS safely: `#rgb[a]`/`#rrggbb[aa]`,
/// `rgb()/rgba()/hsl()/hsla()` with plain numeric args, or a bare keyword.
fn is_css_color(c: &str) -> bool {
    static COLOR_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^(?:#[0-9a-fA-F]{3,4}|#[0-9a-fA-F]{6}|#[0-9a-fA-F]{8}|(?:rgb|rgba|hsl|hsla)\([0-9.,%\s/]+\)|[a-zA-Z]{3,20})$",
        )
        .unwrap()
    });
    c.len() <= 64 && COLOR_RE.is_match(c)
}

fn json_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => clean(&decode_entities(s), MAX_TITLE),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn json_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn is_type(v: &serde_json::Value, ty: &str) -> bool {
    match v.get("@type") {
        Some(serde_json::Value::String(s)) => s == ty,
        Some(serde_json::Value::Array(a)) => a.iter().any(|t| t.as_str() == Some(ty)),
        _ => false,
    }
}

fn find_product(v: &serde_json::Value, depth: usize) -> Option<&serde_json::Value> {
    if depth > 6 {
        return None;
    }
    match v {
        serde_json::Value::Array(items) => items.iter().find_map(|i| find_product(i, depth + 1)),
        serde_json::Value::Object(o) => {
            if is_type(v, "Product") || is_type(v, "ProductGroup") {
                return Some(v);
            }
            o.get("@graph")
                .or_else(|| o.get("mainEntity"))
                .and_then(|g| find_product(g, depth + 1))
        }
        _ => None,
    }
}

/// The first schema.org `Product` in any `application/ld+json` block.
pub fn parse_json_ld_product(html: &str) -> Option<Product> {
    for cap in SCRIPT_RE.captures_iter(html) {
        let a = attrs(&cap[1]);
        if !attr(&a, "type").is_some_and(|t| t.trim().eq_ignore_ascii_case("application/ld+json")) {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(cap[2].trim()) else {
            continue;
        };
        let Some(p) = find_product(&doc, 0) else {
            continue;
        };
        let offer = match p.get("offers") {
            Some(serde_json::Value::Array(a)) => a.first(),
            other => other,
        };
        let price = offer.and_then(|o| {
            o.get("price")
                .or_else(|| o.get("lowPrice"))
                .and_then(json_str)
        });
        let currency = offer.and_then(|o| o.get("priceCurrency").and_then(json_str));
        let availability = offer
            .and_then(|o| o.get("availability").and_then(json_str))
            .map(|a| a.rsplit('/').next().unwrap_or(&a).to_string());
        let rating_obj = p.get("aggregateRating");
        let rating = rating_obj.and_then(|r| r.get("ratingValue").and_then(json_f64));
        let review_count = rating_obj
            .and_then(|r| r.get("reviewCount").or_else(|| r.get("ratingCount")))
            .and_then(json_f64)
            .map(|n| n as u64);
        return Some(Product {
            name: p.get("name").and_then(json_str),
            price,
            currency,
            availability,
            rating,
            review_count,
        });
    }
    None
}

pub fn parse_oembed(body: &[u8]) -> Option<OEmbed> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .and_then(|x| clean(x, MAX_TITLE))
    };
    let n = |k: &str| {
        v.get(k)
            .and_then(json_f64)
            .filter(|n| *n > 0.0 && *n < 10_000.0)
            .map(|n| n as u32)
    };
    Some(OEmbed {
        title: s("title"),
        author_name: s("author_name"),
        provider_name: s("provider_name"),
        thumbnail_url: s("thumbnail_url")
            .filter(|u| u.starts_with("http://") || u.starts_with("https://")),
        width: n("width"),
        height: n("height"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r##"<!doctype html>
<html><head>
<meta charset="utf-8">
<title>  Fallback   Title </title>
<meta property="og:title" content="Widget &amp; Co &#8212; Pro">
<meta content="An &quot;excellent&quot; widget" property='og:description'>
<meta name="description" content="meta description">
<meta property="og:site_name" content="Widget Shop">
<meta property="og:image" content="/img/hero.png?a=1&amp;b=2">
<meta name="theme-color" content="#1a2b3c">
<link rel="apple-touch-icon" href="/apple.png">
<link rel="icon" type="image/png" href="https://cdn.example.com/fav.png">
<link rel="alternate" type="application/json+oembed" href="/oembed?url=x" title="oEmbed">
<script type="application/ld+json">
{"@context":"https://schema.org","@graph":[{"@type":"WebPage"},{"@type":"Product","name":"Widget Pro",
 "offers":{"@type":"Offer","price":"19.99","priceCurrency":"EUR","availability":"https://schema.org/InStock"},
 "aggregateRating":{"@type":"AggregateRating","ratingValue":"4.6","reviewCount":128}}]}
</script>
</head><body><meta property="og:title" content="late duplicate"></body></html>"##;

    #[test]
    fn open_graph_and_head_tags_parse() {
        let base = Url::parse("https://shop.example.com/p/widget").unwrap();
        let m = parse_html(FIXTURE, &base);
        assert_eq!(m.title.as_deref(), Some("Widget & Co — Pro"));
        assert_eq!(m.description.as_deref(), Some("An \"excellent\" widget"));
        assert_eq!(m.site_name.as_deref(), Some("Widget Shop"));
        assert_eq!(
            m.image.as_deref(),
            Some("https://shop.example.com/img/hero.png?a=1&b=2")
        );
        assert_eq!(
            m.favicon.as_deref(),
            Some("https://cdn.example.com/fav.png")
        );
        assert_eq!(m.theme_color.as_deref(), Some("#1a2b3c"));
        assert_eq!(
            m.oembed_url.as_deref(),
            Some("https://shop.example.com/oembed?url=x")
        );
        let p = m.product.unwrap();
        assert_eq!(p.name.as_deref(), Some("Widget Pro"));
        assert_eq!(p.price.as_deref(), Some("19.99"));
        assert_eq!(p.currency.as_deref(), Some("EUR"));
        assert_eq!(p.availability.as_deref(), Some("InStock"));
        assert_eq!(p.rating, Some(4.6));
        assert_eq!(p.review_count, Some(128));
    }

    #[test]
    fn twitter_card_and_title_fallbacks() {
        let base = Url::parse("http://example.org/a/b").unwrap();
        let html = r#"<head><title>Plain &lt;Title&gt;</title>
            <meta name="twitter:image" content="pic.jpg">
            <meta name="twitter:description" content="tw desc">
            <meta name="theme-color" content="red;background:url(x)">
            <link rel="shortcut icon" href="javascript:alert(1)"></head>"#;
        let m = parse_html(html, &base);
        assert_eq!(m.title.as_deref(), Some("Plain <Title>"));
        assert_eq!(m.description.as_deref(), Some("tw desc"));
        assert_eq!(m.image.as_deref(), Some("http://example.org/a/pic.jpg"));
        // A CSS-injection-shaped theme color is dropped.
        assert_eq!(m.theme_color, None);
        // javascript: icons never survive; the conventional path is used.
        assert_eq!(m.favicon.as_deref(), Some("http://example.org/favicon.ico"));
        assert!(m.product.is_none());
    }

    #[test]
    fn oembed_json_parses() {
        let o = parse_oembed(
            br#"{"type":"video","title":"Never Gonna","author_name":"Rick","provider_name":"YouTube",
                "thumbnail_url":"https://i.ytimg.com/vi/x/hq.jpg","width":200,"height":"113","html":"<iframe>"}"#,
        )
        .unwrap();
        assert_eq!(o.title.as_deref(), Some("Never Gonna"));
        assert_eq!(o.author_name.as_deref(), Some("Rick"));
        assert_eq!(o.provider_name.as_deref(), Some("YouTube"));
        assert_eq!(
            o.thumbnail_url.as_deref(),
            Some("https://i.ytimg.com/vi/x/hq.jpg")
        );
        assert_eq!((o.width, o.height), (Some(200), Some(113)));
    }
}
