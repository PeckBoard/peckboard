//! Credential shapes the guard looks for. A copy of the ones PeckBoard's
//! `service::redact` masks in mirrored chat, kept here so this crate has no
//! dependency on the main crate. Change both together.

use std::sync::OnceLock;

use regex::Regex;

pub(crate) fn jwt_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `eyJ` is base64url `{"` — the JWT header marker. Two dot-separated
    // base64url parts after it.
    RE.get_or_init(|| {
        Regex::new(r"\beyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{2,}\b")
            .expect("jwt regex")
    })
}

/// Well-known token prefixes: GitHub (`ghp_`, `gho_`, …, `github_pat_`),
/// OpenAI / Anthropic (`sk-`, `sk-ant-`, `sk-proj-`), Slack (`xox[abpr]-`)
/// and AWS access key ids.
pub(crate) fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}|xox[abpr]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}\b)",
        )
        .expect("token regex")
    })
}

/// A PEM private-key block; an unterminated one runs to the end.
pub(crate) fn pem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        )
        .expect("pem regex")
    })
}

/// `password=…`, `api_key: …`, `SECRET_TOKEN = "…"`; group 3 is the value.
pub(crate) fn assignment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b([A-Za-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key)[A-Za-z0-9_.-]*)(\s*[:=]\s*)("[^"\n]*"|'[^'\n]*'|[^\s,;]+)"#,
        )
        .expect("assignment regex")
    })
}

pub(crate) fn long_run_re() -> &'static Regex {
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
pub(crate) fn looks_random(run: &str) -> bool {
    let has_digit = run.bytes().any(|b| b.is_ascii_digit());
    let has_alpha = run.bytes().any(|b| b.is_ascii_alphabetic());
    if !has_digit || !has_alpha || uuid_re().is_match(run) {
        return false;
    }
    let hex = run.bytes().all(|b| b.is_ascii_hexdigit());
    entropy(run) >= if hex { 3.0 } else { 4.0 }
}
