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
}
