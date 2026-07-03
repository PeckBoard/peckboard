//! Web tools: `search_web`, `fetch_web`, `web_get_part`, and `parse_web`.
//!
//! `search_web` queries DuckDuckGo's keyword HTML endpoint through the same
//! host fetch and returns ranked `{title, url, snippet}` results, so the agent
//! can find pages when it has no URL yet (then `fetch_web`/`parse_web` reads
//! one). `fetch_web` pulls a URL through the host's SSRF-contained fetch, stores the
//! body in the plugin document store under a generated **reference**, and
//! returns that reference plus a short preview — so a large page never floods
//! the agent's context. The agent then calls `web_get_part` to pull specific
//! lines / a byte slice / search matches out of the stored page. `parse_web`
//! converts HTML to readable text, extracts the title / headings / links, and
//! stores the cleaned text as its own reference.

use super::host_bridge::{HostCtx, HostFn};

/// Document-store collection holding fetched/parsed page bodies.
const PAGES_COLLECTION: &str = "pages";
/// Max raw bytes per stored chunk doc (kept well under core's 256 KiB doc cap
/// so the JSON envelope + escaping still fits).
const PAGE_CHUNK_BYTES: usize = 100_000;
/// Ceiling on stored chunks per page (~4 MiB); the rest is dropped + flagged.
const PAGE_MAX_CHUNKS: usize = 40;
/// Characters of body returned inline as a preview by `fetch_web`.
const PREVIEW_CHARS: usize = 1500;

/// DuckDuckGo's no-JavaScript keyword endpoint. Returns a plain HTML results
/// page we can parse without an API key. (The instant-answer JSON API only
/// covers "instant answers", not ranked web results, so it's not usable here.)
const SEARCH_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
/// Hard cap on results returned, regardless of `max_results`.
const SEARCH_MAX_RESULTS: usize = 25;

// ── search_web ────────────────────────────────────────────────────────

pub fn search_web_tool(
    args: serde_json::Value,
    ctx: &HostCtx,
) -> Result<serde_json::Value, String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or("`query` (non-empty string) is required")?
        .to_string();
    let max = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .clamp(1, SEARCH_MAX_RESULTS as u64) as usize;

    let url = format!("{SEARCH_ENDPOINT}?q={}", percent_encode(&query));
    // A browser-ish User-Agent: the HTML endpoint serves an empty page to the
    // host's default agent string. Headers go through the host's SSRF-checked
    // fetch like any other request.
    let resp = ctx.call_host(
        HostFn::HttpFetch,
        &serde_json::json!({
            "url": url,
            "method": "GET",
            "headers": {
                "User-Agent": "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0",
                "Accept": "text/html",
            },
        }),
    )?;

    let status = resp["status"].as_u64().unwrap_or(0);
    let body = resp["body"].as_str().unwrap_or("");
    if status != 200 {
        return Err(format!(
            "web search returned HTTP {status} (DuckDuckGo may be rate-limiting; try again)"
        ));
    }

    let results = parse_ddg_results(body, max);
    Ok(serde_json::json!({
        "query": query,
        "engine": "duckduckgo",
        "result_count": results.len(),
        "results": results,
        "next": if results.is_empty() {
            "No results parsed — refine the query, or the engine returned none."
        } else {
            "Call fetch_web or parse_web on a result url to read the page."
        },
    }))
}

/// Pull `{title, url, snippet}` out of a DuckDuckGo HTML results page. Each
/// result is a `result__a` anchor (title + a `/l/?uddg=…` redirect wrapper)
/// followed by a `result__snippet` anchor; we pair them by order. Robust to
/// attribute reordering — the href is pulled from the matched tag separately.
fn parse_ddg_results(html: &str, max: usize) -> Vec<serde_json::Value> {
    use regex::Regex;

    let anchor_re = Regex::new(r#"(?is)<a\b([^>]*\bclass="result__a"[^>]*)>(.*?)</a>"#).unwrap();
    let href_re = Regex::new(r#"(?is)\bhref\s*=\s*"([^"]*)""#).unwrap();
    let snippet_re =
        Regex::new(r#"(?is)<a\b[^>]*\bclass="result__snippet"[^>]*>(.*?)</a>"#).unwrap();

    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .map(|c| collapse_ws(&decode_entities(&strip_tags(&c[1]))))
        .collect();

    let mut out = Vec::new();
    for (i, cap) in anchor_re.captures_iter(html).enumerate() {
        if out.len() >= max {
            break;
        }
        let title = collapse_ws(&decode_entities(&strip_tags(&cap[2])));
        let href = href_re
            .captures(&cap[1])
            .map(|h| decode_entities(&h[1]))
            .unwrap_or_default();
        let url = normalize_result_url(&href);
        if title.is_empty() || url.is_empty() {
            continue;
        }
        out.push(serde_json::json!({
            "title": title,
            "url": url,
            "snippet": snippets.get(i).cloned().unwrap_or_default(),
        }));
    }
    out
}

/// Turn a DuckDuckGo result href into the real target URL. Results are wrapped
/// as `//duckduckgo.com/l/?uddg=<percent-encoded url>&rut=…`; unwrap and decode
/// it. Protocol-relative direct links get an `https:` scheme; anything else is
/// returned as-is.
fn normalize_result_url(href: &str) -> String {
    if let Some(idx) = href.find("uddg=") {
        let rest = &href[idx + "uddg=".len()..];
        let encoded = rest.split('&').next().unwrap_or("");
        return percent_decode(encoded);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

/// Percent-encode a query string for a URL (RFC 3986 unreserved set kept
/// literal; everything else `%`-escaped). Avoids pulling in a urlencoding dep.
fn percent_encode(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Percent-decode a URL component (`%XX` → byte, `+` → space). Invalid escapes
/// are passed through literally; the result is lossy-UTF-8 decoded.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ── fetch_web ─────────────────────────────────────────────────────────

pub fn fetch_web_tool(args: serde_json::Value, ctx: &HostCtx) -> Result<serde_json::Value, String> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("`url` (string) is required")?
        .to_string();
    let method = args
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .to_string();

    let mut req = serde_json::json!({ "url": url, "method": method });
    if let Some(h) = args.get("headers").filter(|h| h.is_object()) {
        req["headers"] = h.clone();
    }
    let resp = ctx.call_host(HostFn::HttpFetch, &req)?;

    let status = resp["status"].as_u64().unwrap_or(0);
    let final_url = resp["final_url"].as_str().unwrap_or(&url).to_string();
    let body = resp["body"].as_str().unwrap_or("").to_string();
    let fetch_truncated = resp["truncated"].as_bool().unwrap_or(false);
    let content_type = resp["headers"]["content-type"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let location = resp["headers"]["location"].as_str().map(str::to_string);

    let title = extract_title(&body);
    let reference = HostCtx::gen_id();
    let store_truncated = store_page(
        ctx,
        &reference,
        &serde_json::json!({
            "url": url,
            "final_url": final_url,
            "status": status,
            "content_type": content_type,
            "title": title,
        }),
        &body,
    )?;

    let mut out = serde_json::json!({
        "reference": reference,
        "status": status,
        "final_url": final_url,
        "content_type": content_type,
        "title": title,
        "length": body.chars().count(),
        "line_count": body.lines().count(),
        "truncated": fetch_truncated || store_truncated,
        "preview": take_chars(&body, PREVIEW_CHARS),
        "next": "Use web_get_part with this reference (modes: info | lines | slice | search) to read more.",
    });
    // Surface a redirect's target so the agent can re-fetch it (we don't follow
    // redirects host-side, to keep the SSRF check on every hop).
    if (300..400).contains(&status)
        && let Some(loc) = location
    {
        out["redirect_location"] = serde_json::Value::String(loc);
    }
    Ok(out)
}

// ── web_get_part ──────────────────────────────────────────────────────

pub fn web_get_part_tool(
    args: serde_json::Value,
    ctx: &HostCtx,
) -> Result<serde_json::Value, String> {
    let reference = args
        .get("reference")
        .and_then(|v| v.as_str())
        .ok_or("`reference` (string, from fetch_web/parse_web) is required")?;
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("info");

    let meta = load_page_meta(ctx, reference)?
        .ok_or_else(|| format!("unknown reference '{reference}' (it may have expired)"))?;

    match mode {
        "info" => {
            let text = load_page_text(ctx, reference)?;
            Ok(serde_json::json!({
                "reference": reference,
                "url": meta["url"],
                "final_url": meta["final_url"],
                "status": meta["status"],
                "content_type": meta["content_type"],
                "title": meta["title"],
                "length": text.chars().count(),
                "line_count": text.lines().count(),
            }))
        }
        "lines" => {
            let text = load_page_text(ctx, reference)?;
            let start = args
                .get("start_line")
                .and_then(|v| v.as_u64())
                .unwrap_or(1)
                .max(1) as usize;
            let count = args
                .get("line_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(100)
                .clamp(1, 5000) as usize;
            let all: Vec<&str> = text.lines().collect();
            let total = all.len();
            let from = (start - 1).min(total);
            let to = (from + count).min(total);
            let slice = all[from..to].join("\n");
            Ok(serde_json::json!({
                "reference": reference,
                "start_line": start,
                "returned_lines": to - from,
                "total_lines": total,
                "has_more": to < total,
                "content": slice,
            }))
        }
        "slice" => {
            let text = load_page_text(ctx, reference)?;
            let chars: Vec<char> = text.chars().collect();
            let total = chars.len();
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let length = args
                .get("length")
                .and_then(|v| v.as_u64())
                .unwrap_or(4000)
                .clamp(1, 100_000) as usize;
            let from = offset.min(total);
            let to = (from + length).min(total);
            Ok(serde_json::json!({
                "reference": reference,
                "offset": from,
                "returned": to - from,
                "total_length": total,
                "has_more": to < total,
                "content": chars[from..to].iter().collect::<String>(),
            }))
        }
        "search" => {
            let text = load_page_text(ctx, reference)?;
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or("`query` is required for search mode")?;
            let ci = args
                .get("case_insensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let max = args
                .get("max_matches")
                .and_then(|v| v.as_u64())
                .unwrap_or(30)
                .clamp(1, 500) as usize;
            let needle = if ci {
                query.to_lowercase()
            } else {
                query.to_string()
            };
            let mut matches = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let hay = if ci {
                    line.to_lowercase()
                } else {
                    line.to_string()
                };
                if hay.contains(&needle) {
                    matches
                        .push(serde_json::json!({ "line": i + 1, "text": take_chars(line, 400) }));
                    if matches.len() >= max {
                        break;
                    }
                }
            }
            Ok(serde_json::json!({
                "reference": reference,
                "query": query,
                "match_count": matches.len(),
                "truncated": matches.len() >= max,
                "matches": matches,
            }))
        }
        other => Err(format!(
            "unknown mode '{other}'; use info | lines | slice | search"
        )),
    }
}

// ── parse_web ─────────────────────────────────────────────────────────

pub fn parse_web_tool(args: serde_json::Value, ctx: &HostCtx) -> Result<serde_json::Value, String> {
    // Source: an explicit reference (already fetched) or a url (fetch now).
    let html = if let Some(reference) = args.get("reference").and_then(|v| v.as_str()) {
        load_page_text(ctx, reference)?
    } else if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
        let resp = ctx.call_host(HostFn::HttpFetch, &serde_json::json!({ "url": url }))?;
        resp["body"].as_str().unwrap_or("").to_string()
    } else {
        return Err("provide either `url` or `reference`".to_string());
    };

    let parsed = parse_html(&html);

    // Store the cleaned text under its own reference for follow-up reads.
    let reference = HostCtx::gen_id();
    let store_truncated = store_page(
        ctx,
        &reference,
        &serde_json::json!({
            "url": args.get("url").cloned().unwrap_or(serde_json::Value::Null),
            "final_url": serde_json::Value::Null,
            "status": serde_json::Value::Null,
            "content_type": "text/plain",
            "title": parsed.title,
        }),
        &parsed.text,
    )?;

    let headings: Vec<serde_json::Value> = parsed
        .headings
        .iter()
        .take(100)
        .map(|(lvl, t)| serde_json::json!({ "level": lvl, "text": t }))
        .collect();
    let links: Vec<serde_json::Value> = parsed
        .links
        .iter()
        .take(200)
        .map(|(href, t)| serde_json::json!({ "href": href, "text": t }))
        .collect();

    Ok(serde_json::json!({
        "title": parsed.title,
        "text_reference": reference,
        "text_length": parsed.text.chars().count(),
        "line_count": parsed.text.lines().count(),
        "text_truncated": store_truncated,
        "text_preview": take_chars(&parsed.text, PREVIEW_CHARS),
        "headings": headings,
        "heading_count": parsed.headings.len(),
        "links": links,
        "link_count": parsed.links.len(),
        "next": "Use web_get_part with text_reference to read the full cleaned text.",
    }))
}

// ── Page storage (chunked across the document store) ──────────────────

/// Store a page body as chunk docs plus a meta doc. Returns `true` when the
/// body was truncated to fit `PAGE_MAX_CHUNKS`.
fn store_page(
    ctx: &HostCtx,
    reference: &str,
    meta: &serde_json::Value,
    content: &str,
) -> Result<bool, String> {
    // Best-effort clear any stale chunks for this reference (ids are random, so
    // this is just hygiene).
    let chunks = split_chunks(content);
    let truncated = chunks.len() > PAGE_MAX_CHUNKS;
    let kept = chunks.len().min(PAGE_MAX_CHUNKS);

    for (i, chunk) in chunks.iter().take(kept).enumerate() {
        ctx.call_host(
            HostFn::StorePut,
            &serde_json::json!({
                "collection": PAGES_COLLECTION,
                "key": chunk_key(reference, i),
                "data": { "text": chunk },
            }),
        )?;
    }

    let mut meta_doc = meta.clone();
    meta_doc["chunks"] = serde_json::json!(kept);
    meta_doc["truncated"] = serde_json::json!(truncated);
    ctx.call_host(
        HostFn::StorePut,
        &serde_json::json!({
            "collection": PAGES_COLLECTION,
            "key": meta_key(reference),
            "data": meta_doc,
        }),
    )?;
    Ok(truncated)
}

fn load_page_meta(ctx: &HostCtx, reference: &str) -> Result<Option<serde_json::Value>, String> {
    let v = ctx.call_host(
        HostFn::StoreGet,
        &serde_json::json!({ "collection": PAGES_COLLECTION, "key": meta_key(reference) }),
    )?;
    match &v["value"] {
        serde_json::Value::Null => Ok(None),
        other => Ok(Some(other.clone())),
    }
}

fn load_page_text(ctx: &HostCtx, reference: &str) -> Result<String, String> {
    let meta = load_page_meta(ctx, reference)?
        .ok_or_else(|| format!("unknown reference '{reference}'"))?;
    let chunks = meta["chunks"].as_u64().unwrap_or(0) as usize;
    let mut text = String::new();
    for i in 0..chunks {
        let v = ctx.call_host(
            HostFn::StoreGet,
            &serde_json::json!({ "collection": PAGES_COLLECTION, "key": chunk_key(reference, i) }),
        )?;
        if let Some(s) = v["value"]["text"].as_str() {
            text.push_str(s);
        }
    }
    Ok(text)
}

fn meta_key(reference: &str) -> String {
    format!("{reference}:meta")
}

fn chunk_key(reference: &str, i: usize) -> String {
    format!("{reference}:c{i}")
}

/// Split text into chunks of at most `PAGE_CHUNK_BYTES` raw bytes, never
/// splitting a UTF-8 character.
fn split_chunks(content: &str) -> Vec<String> {
    if content.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for ch in content.chars() {
        if cur.len() + ch.len_utf8() > PAGE_CHUNK_BYTES && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

// ── HTML → text / structure ───────────────────────────────────────────

pub struct ParsedHtml {
    pub title: String,
    pub text: String,
    pub headings: Vec<(u8, String)>,
    pub links: Vec<(String, String)>,
}

fn extract_title(html: &str) -> String {
    let lower = html.to_lowercase();
    if let Some(start) = lower.find("<title")
        && let Some(gt) = lower[start..].find('>')
    {
        let after = start + gt + 1;
        if let Some(end) = lower[after..].find("</title>") {
            return collapse_ws(&decode_entities(&strip_tags(&html[after..after + end])));
        }
    }
    String::new()
}

/// Parse HTML into readable text plus structured headings/links. Implemented
/// with small regexes (the `regex` crate has no backreferences, so each
/// heading level is matched separately).
pub fn parse_html(html: &str) -> ParsedHtml {
    use regex::Regex;

    // Drop comments, scripts, and styles wholesale before anything else.
    let no_comments = Regex::new(r"(?s)<!--.*?-->")
        .unwrap()
        .replace_all(html, " ");
    let no_script = Regex::new(r"(?is)<script\b.*?</script>")
        .unwrap()
        .replace_all(&no_comments, " ");
    let cleaned = Regex::new(r"(?is)<style\b.*?</style>")
        .unwrap()
        .replace_all(&no_script, " ")
        .into_owned();

    let title = extract_title(&cleaned);

    // Headings, per level (no backreferences available).
    let mut headings = Vec::new();
    for level in 1u8..=6 {
        let re = Regex::new(&format!(r"(?is)<h{level}\b[^>]*>(.*?)</h{level}>")).unwrap();
        for cap in re.captures_iter(&cleaned) {
            let t = collapse_ws(&decode_entities(&strip_tags(&cap[1])));
            if !t.is_empty() {
                headings.push((level, t));
            }
        }
    }

    // Links: href + anchor text.
    let mut links = Vec::new();
    let link_re =
        Regex::new(r#"(?is)<a\b[^>]*?href\s*=\s*["']?([^"'>\s]+)[^>]*>(.*?)</a>"#).unwrap();
    for cap in link_re.captures_iter(&cleaned) {
        let href = decode_entities(&cap[1]);
        let text = collapse_ws(&decode_entities(&strip_tags(&cap[2])));
        links.push((href, text));
    }

    // Readable text: turn block/break tags into newlines, strip the rest.
    let block_re = Regex::new(
        r"(?i)</?(p|div|br|li|tr|table|ul|ol|h[1-6]|section|article|header|footer|nav|blockquote|pre|hr)[^>]*>",
    )
    .unwrap();
    let with_breaks = block_re.replace_all(&cleaned, "\n");
    let text_raw = strip_tags(&with_breaks);
    let text = normalize_text(&decode_entities(&text_raw));

    ParsedHtml {
        title,
        text,
        headings,
        links,
    }
}

/// Remove every `<...>` tag, leaving the text between them.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Collapse all runs of whitespace to single spaces and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalize block text: collapse spaces/tabs within a line, drop blank-line
/// runs to at most one, trim each line, and trim the whole.
fn normalize_text(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blank_run = 0;
    for raw in s.lines() {
        let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !lines.is_empty() {
                lines.push(String::new());
            }
        } else {
            blank_run = 0;
            lines.push(line);
        }
    }
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.join("\n")
}

/// Decode the HTML entities that actually show up in page text — the five
/// predefined ones, common named entities, and numeric (`&#NN;` / `&#xHH;`).
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '&'
            && let Some(semi) = bytes[i + 1..].iter().position(|&c| c == ';')
        {
            let entity: String = bytes[i + 1..i + 1 + semi].iter().collect();
            if let Some(ch) = decode_one_entity(&entity) {
                out.push_str(&ch);
                i += semi + 2;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn decode_one_entity(entity: &str) -> Option<String> {
    let named = match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        "copy" => Some('©'),
        "reg" => Some('®'),
        "trade" => Some('™'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "rsquo" | "rsquor" => Some('’'),
        "lsquo" => Some('‘'),
        "ldquo" => Some('“'),
        "rdquo" => Some('”'),
        "middot" => Some('·'),
        "bull" => Some('•'),
        "deg" => Some('°'),
        "euro" => Some('€'),
        "pound" => Some('£'),
        _ => None,
    };
    if let Some(c) = named {
        return Some(c.to_string());
    }
    // Numeric: &#123; or &#x1F600;
    if let Some(rest) = entity.strip_prefix('#') {
        let code = if let Some(hex) = rest.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            rest.parse::<u32>().ok()?
        };
        return char::from_u32(code).map(|c| c.to_string());
    }
    None
}

/// Take at most `n` characters (code points) from `s`.
fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_chunks_respects_byte_cap_and_char_boundaries() {
        let big = "é".repeat(PAGE_CHUNK_BYTES); // 2 bytes each → spans many chunks
        let chunks = split_chunks(&big);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= PAGE_CHUNK_BYTES);
        }
        assert_eq!(chunks.concat(), big);
    }

    #[test]
    fn strip_and_decode() {
        assert_eq!(strip_tags("a<b>c</b>d"), "acd");
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &#65;"), "a & b <c> A");
        assert_eq!(decode_entities("&#x41;&#x42;"), "AB");
    }

    #[test]
    fn parse_html_extracts_structure() {
        let html = r#"
            <html><head><title>Hello &amp; World</title>
            <style>.x{color:red}</style></head>
            <body>
              <h1>Main Heading</h1>
              <p>Some <b>bold</b> text with a <a href="https://example.com/page">link</a>.</p>
              <script>var x = '<h2>not a heading</h2>';</script>
              <h2>Sub</h2>
            </body></html>"#;
        let p = parse_html(html);
        assert_eq!(p.title, "Hello & World");
        assert!(p.headings.contains(&(1, "Main Heading".to_string())));
        assert!(p.headings.contains(&(2, "Sub".to_string())));
        // Script content must not leak into headings or text.
        assert!(!p.headings.iter().any(|(_, t)| t.contains("not a heading")));
        assert!(p.text.contains("bold"));
        assert!(!p.text.to_lowercase().contains("var x"));
        assert_eq!(
            p.links,
            vec![("https://example.com/page".to_string(), "link".to_string())]
        );
    }

    #[test]
    fn percent_encode_keeps_unreserved_escapes_rest() {
        assert_eq!(percent_encode("rust async"), "rust%20async");
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("a&b=c/d"), "a%26b%3Dc%2Fd");
    }

    #[test]
    fn percent_decode_handles_escapes_plus_and_bad_input() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fwww.rust-lang.org%2F"),
            "https://www.rust-lang.org/"
        );
        assert_eq!(percent_decode("a+b"), "a b");
        // A dangling/invalid escape is passed through literally, not dropped.
        assert_eq!(percent_decode("100%done"), "100%done");
    }

    #[test]
    fn normalize_result_url_unwraps_ddg_redirect() {
        assert_eq!(
            normalize_result_url(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc"
            ),
            "https://example.com/page"
        );
        // Protocol-relative direct link gets a scheme.
        assert_eq!(
            normalize_result_url("//example.org/x"),
            "https://example.org/x"
        );
        // Absolute direct link is left alone.
        assert_eq!(
            normalize_result_url("https://example.org/"),
            "https://example.org/"
        );
    }

    #[test]
    fn parse_ddg_results_extracts_title_url_snippet() {
        // Trimmed-down shape of a DuckDuckGo HTML results page: a result__a
        // anchor (title + /l/?uddg= redirect) paired with a result__snippet.
        let html = r##"
            <div class="result results_links">
              <a rel="nofollow" class="result__a"
                 href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=z">
                 The Rust Programming Language</a>
              <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">
                 A language empowering everyone to build <b>reliable</b> software.</a>
            </div>
            <div class="result results_links">
              <a class="result__a"
                 href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fbook%2F&amp;rut=y">
                 The Book</a>
              <a class="result__snippet" href="#">The official Rust book.</a>
            </div>"##;

        let results = parse_ddg_results(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["title"], "The Rust Programming Language");
        assert_eq!(results[0]["url"], "https://www.rust-lang.org/");
        assert_eq!(
            results[0]["snippet"],
            "A language empowering everyone to build reliable software."
        );
        assert_eq!(results[1]["url"], "https://doc.rust-lang.org/book/");

        // max_results caps the output.
        assert_eq!(parse_ddg_results(html, 1).len(), 1);
        // A page with no result anchors yields an empty list, not an error.
        assert!(parse_ddg_results("<html><body>nothing</body></html>", 10).is_empty());
    }
}
