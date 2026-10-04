//! Sensitive-data masking for recorded browser traffic (`browser_runs`).
//!
//! Everything recorded from a page — request/response headers, bodies, URLs,
//! console lines, typed text — passes through here BEFORE it is persisted, so
//! secrets never reach disk. Two complementary passes:
//!
//! - **Key-based**: header names / JSON keys / form and query parameter names
//!   matching a denylist have their values replaced wholesale.
//! - **Value-based**: any remaining free text is scanned for secret-shaped
//!   values (Bearer/Basic credentials, JWTs, Luhn-valid card numbers).
//!
//! This is a best-effort denylist, not a DLP guarantee — an API that returns
//! a secret under an innocuous key with no recognizable shape will slip
//! through.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

/// Replacement for a fully masked value.
pub const MASK: &str = "«masked»";

// ── key-based masking ───────────────────────────────────────────────────

/// Substring matches — long enough to be unambiguous anywhere in a key.
const KEY_SUBSTRINGS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "credential",
    "private_key",
    "privatekey",
    "client_secret",
    "access_key",
    "secret_key",
    "card_number",
    "cardnumber",
    "cvv",
    "cvc",
    "authorization",
    "session_id",
    "sessionid",
];

/// Whole-segment matches (key split on `_`/`-`/`.`) — short or ambiguous
/// words where substring matching would over-fire ("author", "shipping").
const KEY_SEGMENTS: &[&str] = &[
    "auth", "cookie", "session", "sid", "ssn", "otp", "pin", "pan", "jwt", "bearer", "key", "pwd",
];

/// Should the value under this header/JSON/form/query key be masked?
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    if KEY_SUBSTRINGS.iter().any(|s| k.contains(s)) {
        return true;
    }
    k.split(['_', '-', '.'])
        .any(|seg| KEY_SEGMENTS.contains(&seg))
}

/// Header names always masked regardless of the generic key rules.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "authentication",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "x-access-token",
    "x-session-token",
    "x-csrf-token",
    "x-xsrf-token",
    "x-amz-security-token",
    "x-goog-api-key",
];

// ── value-based masking ─────────────────────────────────────────────────

fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}").expect("bearer regex")
    })
}

fn jwt_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `eyJ` is base64url `{"` — the JWT header marker. Two dot-separated
    // base64url parts after it.
    RE.get_or_init(|| {
        Regex::new(r"\beyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{2,}\b")
            .expect("jwt regex")
    })
}

fn card_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // 13–19 digits allowing single space/dash separators.
    RE.get_or_init(|| Regex::new(r"\b\d(?:[ -]?\d){12,18}\b").expect("card regex"))
}

fn luhn_valid(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for &d in digits.iter().rev() {
        let mut v = u32::from(d);
        if double {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
        double = !double;
    }
    sum % 10 == 0
}

/// Mask secret-shaped values inside free text: `Bearer`/`Basic` credentials,
/// JWTs, and Luhn-valid card numbers (which keep their last 4 digits).
pub fn mask_text(text: &str) -> String {
    let s = bearer_re().replace_all(text, |c: &regex::Captures| format!("{} {MASK}", &c[1]));
    let s = jwt_re().replace_all(&s, MASK);
    card_re()
        .replace_all(&s, |c: &regex::Captures| {
            let m = &c[0];
            let digits: Vec<u8> = m
                .bytes()
                .filter(u8::is_ascii_digit)
                .map(|b| b - b'0')
                .collect();
            if luhn_valid(&digits) {
                let last4: String = m
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>()
                    .chars()
                    .skip(digits.len() - 4)
                    .collect();
                format!("•••• {last4}")
            } else {
                m.to_string()
            }
        })
        .into_owned()
}
// ── credential shapes (outbound chat mirroring) ─────────────────────────

/// Well-known token prefixes: GitHub (`ghp_`, `gho_`, …, `github_pat_`),
/// OpenAI / Anthropic (`sk-`, `sk-ant-`, `sk-proj-`), Slack (`xox[abpr]-`)
/// and AWS access key ids.
fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}|xox[abpr]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}\b)",
        )
        .expect("token regex")
    })
}

/// A PEM private-key block; an unterminated one is masked to the end.
fn pem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        )
        .expect("pem regex")
    })
}

/// `password=…`, `api_key: …`, `SECRET_TOKEN = "…"`: the label is kept, the
/// value (quoted, or up to whitespace / `,` / `;`) is masked.
fn assignment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b([A-Za-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key)[A-Za-z0-9_.-]*)(\s*[:=]\s*)("[^"\n]*"|'[^'\n]*'|[^\s,;]+)"#,
        )
        .expect("assignment regex")
    })
}

fn long_run_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9+/_=-]{32,}").expect("run regex"))
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
            .expect("uuid regex")
    })
}

/// Shannon entropy in bits per char.
fn entropy(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / n;
            -p * p.log2()
        })
        .sum()
}

/// A 32+ char base64/hex run that looks random: it mixes letters and
/// digits (so identifiers and words are spared), isn't a UUID, and has
/// high entropy (hex ≥ 3.0 bits/char, anything else ≥ 4.0).
fn looks_random(run: &str) -> bool {
    let has_digit = run.bytes().any(|b| b.is_ascii_digit());
    let has_alpha = run.bytes().any(|b| b.is_ascii_alphabetic());
    if !has_digit || !has_alpha || uuid_re().is_match(run) {
        return false;
    }
    let hex = run.bytes().all(|b| b.is_ascii_hexdigit());
    entropy(run) >= if hex { 3.0 } else { 4.0 }
}

/// Mask credential-shaped values in free text that leaves the host (chat
/// mirroring): known token prefixes, PEM private keys, `password=…`-style
/// assignments, and long high-entropy base64/hex runs. Complements
/// [`mask_text`]; run both.
pub fn mask_credentials(text: &str) -> String {
    let s = pem_re().replace_all(text, MASK);
    let s = token_re().replace_all(&s, MASK);
    let s = assignment_re().replace_all(&s, |c: &regex::Captures| {
        format!("{}{}{MASK}", &c[1], &c[2])
    });
    long_run_re()
        .replace_all(&s, |c: &regex::Captures| {
            if looks_random(&c[0]) {
                MASK.to_string()
            } else {
                c[0].to_string()
            }
        })
        .into_owned()
}

// ── structured surfaces ─────────────────────────────────────────────────

/// Mask a header map: sensitive names lose their value entirely; every other
/// value gets the free-text pass.
pub fn mask_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            let masked = if SENSITIVE_HEADERS.contains(&lower.as_str()) || is_sensitive_key(&lower)
            {
                MASK.to_string()
            } else {
                mask_text(value)
            };
            (name.clone(), masked)
        })
        .collect()
}

/// Mask query-string (and fragment) values in a URL. The path is untouched.
pub fn mask_url(url: &str) -> String {
    let (base, frag) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let masked_base = match base.split_once('?') {
        Some((path, query)) => format!("{path}?{}", mask_pairs(query)),
        None => base.to_string(),
    };
    match frag {
        Some(f) => format!("{masked_base}#{}", mask_text(f)),
        None => masked_base,
    }
}

/// Mask an `a=b&c=d` pair list by key, value-pass the rest.
fn mask_pairs(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => {
                if is_sensitive_key(k) {
                    format!("{k}={MASK}")
                } else {
                    format!("{k}={}", mask_text(v))
                }
            }
            None => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Mask a request/response body. JSON bodies (by content type or shape) are
/// masked recursively by key; form bodies by pair; everything else gets the
/// free-text pass.
pub fn mask_body(content_type: Option<&str>, body: &str) -> String {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let trimmed = body.trim_start();
    if ct.contains("json") || trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(body) {
            mask_json(&mut v);
            return v.to_string();
        }
    }
    if ct.contains("x-www-form-urlencoded") {
        return mask_pairs(body);
    }
    mask_text(body)
}

/// Recursively mask a JSON value: sensitive keys have scalar values replaced,
/// string leaves get the free-text pass.
pub fn mask_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_sensitive_key(k) && !v.is_object() && !v.is_array() {
                    *v = serde_json::Value::String(MASK.to_string());
                } else {
                    mask_json(v);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for v in items.iter_mut() {
                mask_json(v);
            }
        }
        serde_json::Value::String(s) => {
            let masked = mask_text(s);
            if masked != *s {
                *s = masked;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_keys_match_by_substring_and_segment() {
        for k in [
            "password",
            "user_password",
            "ACCESS_TOKEN",
            "x-api-key",
            "client_secret",
            "auth",
            "my_auth",
            "session-id",
            "Cookie",
            "otp_code",
        ] {
            assert!(is_sensitive_key(k), "{k} should be sensitive");
        }
        for k in [
            "author", "shipping", "pinned", "keyboard", "username", "email",
        ] {
            assert!(!is_sensitive_key(k), "{k} should NOT be sensitive");
        }
    }

    #[test]
    fn bearer_jwt_and_cards_are_masked_in_text() {
        let t = mask_text("Authorization: Bearer abcdef123456.xyz done");
        assert!(!t.contains("abcdef123456"), "got: {t}");
        assert!(t.contains(MASK));

        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.dBjftJeZ4CVP";
        let t = mask_text(&format!("token={jwt}"));
        assert!(!t.contains("dBjftJeZ4CVP"), "got: {t}");

        // Valid Visa test number keeps last 4 only.
        let t = mask_text("card 4111 1111 1111 1111 ok");
        assert!(t.contains("•••• 1111"), "got: {t}");
        assert!(!t.contains("4111 1111"), "got: {t}");

        // Non-Luhn digit runs (order ids) survive.
        let t = mask_text("order 1234567890123 shipped");
        assert!(t.contains("1234567890123"), "got: {t}");
    }

    #[test]
    fn headers_are_masked_by_name() {
        let mut h = BTreeMap::new();
        h.insert("Authorization".to_string(), "Bearer shh".to_string());
        h.insert("Cookie".to_string(), "sid=123".to_string());
        h.insert("Content-Type".to_string(), "application/json".to_string());
        let m = mask_headers(&h);
        assert_eq!(m["Authorization"], MASK);
        assert_eq!(m["Cookie"], MASK);
        assert_eq!(m["Content-Type"], "application/json");
    }

    #[test]
    fn urls_mask_query_values_by_key() {
        let u = mask_url("https://api.x.com/v1/user?id=7&access_token=shhh&x=1#frag");
        assert_eq!(
            u,
            format!("https://api.x.com/v1/user?id=7&access_token={MASK}&x=1#frag")
        );
        // No query — untouched.
        assert_eq!(mask_url("https://x.com/a/b"), "https://x.com/a/b");
    }

    #[test]
    fn json_bodies_mask_recursively() {
        let body = r#"{"user":{"name":"jo","password":"hunter2"},"items":[{"token":"abc"}],"note":"call me"}"#;
        let m = mask_body(Some("application/json"), body);
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["user"]["password"], MASK);
        assert_eq!(v["items"][0]["token"], MASK);
        assert_eq!(v["user"]["name"], "jo");
        assert_eq!(v["note"], "call me");
    }

    #[test]
    fn form_bodies_mask_by_pair_key() {
        let m = mask_body(
            Some("application/x-www-form-urlencoded"),
            "user=jo&password=hunter2&plan=pro",
        );
        assert_eq!(m, format!("user=jo&password={MASK}&plan=pro"));
    }

    #[test]
    fn json_shaped_bodies_mask_even_without_content_type() {
        let m = mask_body(None, r#"{"refresh_token":"r1","ok":true}"#);
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["refresh_token"], MASK);
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn credential_shapes_are_masked() {
        for secret in [
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "gho_ABCDEFGHIJKLMNOPQRSTUVWX12",
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
            "sk-ant-api03-abcdefghijklmnopqrstuv",
            "sk-proj-abcdefghijklmnopqrstuvwx",
            "sk-abcdefghijklmnopqrstuvwx",
            "xoxb-1234567890-abcdefghij",
            "xoxp-1234567890-abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            let t = mask_credentials(&format!("use {secret} now"));
            assert_eq!(t, format!("use {MASK} now"), "{secret}");
        }
    }

    #[test]
    fn pem_private_keys_are_masked_whole() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\nabc\n-----END RSA PRIVATE KEY-----";
        let t = mask_credentials(&format!("key:\n{pem}\ndone"));
        assert!(!t.contains("MIIEpAIBAAKCAQEA"), "{t}");
        assert!(t.ends_with("done"), "{t}");
        // A truncated block (no END line) is masked to the end.
        let t = mask_credentials("-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg");
        assert_eq!(t, MASK);
        // Public keys are not secrets.
        let public = "-----BEGIN PUBLIC KEY-----";
        assert_eq!(mask_credentials(public), public);
    }

    #[test]
    fn sensitive_assignments_keep_the_label() {
        assert_eq!(
            mask_credentials("DB_PASSWORD=hunter22 ok"),
            format!("DB_PASSWORD={MASK} ok")
        );
        assert_eq!(
            mask_credentials("api-key: \"abc def\""),
            format!("api-key: {MASK}")
        );
        assert_eq!(
            mask_credentials("token = 1234abcd"),
            format!("token = {MASK}")
        );
        // No assignment, no mask.
        let prose = "the password policy changed and the token expired";
        assert_eq!(mask_credentials(prose), prose);
    }

    #[test]
    fn high_entropy_runs_are_masked_but_words_and_uuids_are_not() {
        let b64 = "Zm9vYmFyQmF6UXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbA9x";
        assert_eq!(mask_credentials(b64), MASK);
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert_eq!(mask_credentials(hex), MASK);
        for keep in [
            "550e8400-e29b-41d4-a716-446655440000",
            "assistant_mirror_delivery_queue_capacity_limit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1",
            "internationalization-and-localization-work",
            "short1234abc",
        ] {
            assert_eq!(mask_credentials(keep), keep, "{keep}");
        }
    }
}
