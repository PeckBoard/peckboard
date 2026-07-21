//! Value-based secret masking for console output returned to agents.
//!
//! The contract: agents never see secret VALUES. Custom env vars (Settings →
//! Environment Variables) are injected into the child processes of commands
//! agents run — never into the agent process itself — and anything a command
//! prints to stdout/stderr is filtered here before the agent reads it.
//!
//! Masking is value-based: wherever a secret value appears it is replaced by
//! [`MASK`]. Covered forms: the verbatim value (whatever mix of letters,
//! digits, and symbols it contains), case flips, and light obfuscation — the
//! value's characters in order with short runs of filler between them
//! (`s-e-c-r-e-t`, spaced chars, one char per line from `fold -w1`).
//! Transforms that rewrite characters (base64, `rev`, rot13) are out of
//! scope: no output filter is complete against an adversarial shell; the
//! bounds here keep honest output readable while closing the stated leaks.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

/// What a masked secret is replaced with. Fixed width so the mask reveals
/// neither the value nor its length.
pub const MASK: &str = "********";

/// Values shorter than this are never masked — too common in honest output
/// (single digits, "true", …) to filter without mangling everything.
const MIN_SECRET_CHARS: usize = 4;

/// The interleaved (obfuscated) scan needs at least this many alphanumeric
/// characters in the secret, or ordinary prose would false-positive.
const MIN_CANON_CHARS: usize = 6;

/// Max filler characters between two consecutive secret characters in an
/// obfuscated occurrence (`s---e` still matches, `s----e` no longer does).
const MAX_GAP_CHARS: usize = 3;

/// Markers that make a host env var NAME sensitive: its value is masked in
/// console output even though the var was never entered in Peckboard (covers
/// `printenv` leaking the server's own environment). Custom Peckboard env
/// vars are always masked, independent of name.
const SENSITIVE_NAME_MARKS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "CREDENTIAL",
    "AUTH",
    "PRIVATE_KEY",
    "ACCESS_KEY",
    "BEARER",
];

/// Whether a host env var's value should be treated as secret by name.
pub fn is_sensitive_env_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    SENSITIVE_NAME_MARKS.iter().any(|m| up.contains(m))
}

/// One secret to hunt for: the verbatim value plus its lowercased
/// alphanumeric skeleton for the interleaved scan (empty when the value has
/// too few alphanumeric chars to scan without false positives).
struct Secret {
    exact: String,
    canon: Vec<char>,
}

/// A set of secret values compiled for repeated masking of output text.
pub struct SecretMasker {
    secrets: Vec<Secret>,
}

impl SecretMasker {
    /// Build from raw secret values. Dedupes and drops values under
    /// [`MIN_SECRET_CHARS`]. Longest-first order so a shorter secret nested
    /// in a longer one can't split the longer match.
    pub fn new<I>(values: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let mut vals: Vec<String> = values
            .into_iter()
            .filter(|v| v.chars().count() >= MIN_SECRET_CHARS)
            .collect();
        vals.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        vals.dedup();
        let secrets = vals
            .into_iter()
            .map(|v| {
                let canon = canon_chars(&v);
                let canon = if canon.len() >= MIN_CANON_CHARS {
                    canon
                } else {
                    Vec::new()
                };
                Secret { exact: v, canon }
            })
            .collect();
        Self { secrets }
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Replace every occurrence of every secret — verbatim or interleaved —
    /// with [`MASK`]. Borrows the input unchanged when nothing matches.
    pub fn mask<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.secrets.is_empty() || text.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for s in &self.secrets {
            let mut from = 0;
            while let Some(i) = text[from..].find(s.exact.as_str()) {
                let start = from + i;
                ranges.push((start, start + s.exact.len()));
                from = start + s.exact.len();
            }
        }
        let needs_skel = self.secrets.iter().any(|s| !s.canon.is_empty());
        if needs_skel {
            let skel = skeleton(text);
            for s in &self.secrets {
                if !s.canon.is_empty() {
                    find_interleaved(&skel, &s.canon, &mut ranges);
                }
            }
        }
        if ranges.is_empty() {
            return Cow::Borrowed(text);
        }
        Cow::Owned(replace_ranges(text, ranges))
    }
}

/// One alphanumeric character of the scanned text: its byte span, its
/// ascii-lowercased char, and its position in the text's char sequence (for
/// measuring filler gaps in original characters).
struct SkelChar {
    start: usize,
    end: usize,
    ch: char,
    char_pos: usize,
}

/// A secret's scan skeleton: its alphanumeric chars, ascii-lowercased.
fn canon_chars(s: &str) -> Vec<char> {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The text's alphanumeric chars with byte spans and char positions.
fn skeleton(text: &str) -> Vec<SkelChar> {
    let mut out = Vec::new();
    for (char_pos, (start, c)) in text.char_indices().enumerate() {
        if c.is_alphanumeric() {
            out.push(SkelChar {
                start,
                end: start + c.len_utf8(),
                ch: c.to_ascii_lowercase(),
                char_pos,
            });
        }
    }
    out
}

/// Find runs of consecutive skeleton chars equal to `canon` where each pair
/// of neighbours has at most [`MAX_GAP_CHARS`] filler (non-alphanumeric)
/// chars between them in the original text. Alphanumeric noise between
/// secret chars intentionally breaks the match — only "symbols in between"
/// obfuscation is a secret's own value.
fn find_interleaved(skel: &[SkelChar], canon: &[char], ranges: &mut Vec<(usize, usize)>) {
    let m = canon.len();
    let mut i = 0;
    while i + m <= skel.len() {
        if skel[i].ch == canon[0] {
            let mut ok = true;
            for j in 1..m {
                if skel[i + j].ch != canon[j]
                    || skel[i + j].char_pos - skel[i + j - 1].char_pos - 1 > MAX_GAP_CHARS
                {
                    ok = false;
                    break;
                }
            }
            if ok {
                ranges.push((skel[i].start, skel[i + m - 1].end));
                i += m;
                continue;
            }
        }
        i += 1;
    }
}

/// Rebuild `text` with every (merged) range replaced by [`MASK`]. Ranges are
/// byte indices on char boundaries; overlaps and nesting collapse into one
/// mask.
fn replace_ranges(text: &str, mut ranges: Vec<(usize, usize)>) -> String {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match merged.last_mut() {
            Some((_, pe)) if s <= *pe => {
                if e > *pe {
                    *pe = e;
                }
            }
            _ => merged.push((s, e)),
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (s, e) in merged {
        out.push_str(&text[at..s]);
        out.push_str(MASK);
        at = e;
    }
    out.push_str(&text[at..]);
    out
}

/// Mask the conventional console fields (`stdout`, `stderr`) of a JSON tool
/// envelope in place. No-op when the masker is empty or the fields are absent.
pub fn mask_console_fields(v: &mut serde_json::Value, masker: &SecretMasker) {
    if masker.is_empty() {
        return;
    }
    if let Some(obj) = v.as_object_mut() {
        for key in ["stdout", "stderr"] {
            if let Some(field) = obj.get_mut(key)
                && let Some(s) = field.as_str()
                && let Cow::Owned(masked) = masker.mask(s)
            {
                *field = serde_json::Value::String(masked);
            }
        }
    }
}

/// Assemble what a command child process needs: the env vars to inject and
/// the masker for its console output.
///
/// - inject: every custom env var (Settings → Environment Variables) — plain
///   values from the DB, encrypted values from the unlock cache while an
///   owner's unlock is warm. Layered over the inherited host env by the
///   caller (custom wins on collision: user-configured beats ambient).
/// - masker: every injected value, plus host env values whose NAME is
///   sensitive ([`is_sensitive_env_name`]).
///
/// Blocking (DB + lock) — call from a blocking thread only.
pub fn command_env_blocking(db: &crate::db::Db) -> (Vec<(String, String)>, SecretMasker) {
    let rows = match db.list_env_vars_blocking() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("env vars: list failed ({e}) — commands run without custom env");
            Vec::new()
        }
    };
    let mut inject: BTreeMap<String, String> = BTreeMap::new();
    let mut encrypted_names: HashSet<&str> = HashSet::new();
    for r in &rows {
        if r.encrypted {
            encrypted_names.insert(r.name.as_str());
        } else if let Some(v) = &r.value {
            inject.insert(r.name.clone(), v.clone());
        }
    }
    if !encrypted_names.is_empty() {
        for (name, value) in crate::service::env_vars::unlocked_values_blocking() {
            // The cache may hold values for since-deleted vars; only rows
            // that still exist as encrypted vars are injected.
            if encrypted_names.contains(name.as_str()) {
                inject.insert(name, value);
            }
        }
    }

    let mut secrets: Vec<String> = inject.values().cloned().collect();
    for (name, value) in std::env::vars() {
        if is_sensitive_env_name(&name) {
            secrets.push(value);
        }
    }
    (inject.into_iter().collect(), SecretMasker::new(secrets))
}

/// The console masker alone (custom env values + sensitive host env), for
/// output paths that don't spawn a local child (e.g. remote `ssh_run`
/// output). Blocking — call from a blocking thread only.
pub fn masker_blocking(db: &crate::db::Db) -> SecretMasker {
    command_env_blocking(db).1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masker(vals: &[&str]) -> SecretMasker {
        SecretMasker::new(vals.iter().map(|s| s.to_string()))
    }

    #[test]
    fn verbatim_value_masked_regardless_of_charset() {
        let m = masker(&["p@ss-w0rd!42"]);
        assert_eq!(m.mask("before p@ss-w0rd!42 after"), "before ******** after");
    }

    #[test]
    fn masked_inside_key_value_line() {
        let m = masker(&["hunter2secret"]);
        assert_eq!(
            m.mask("MY_TOKEN=hunter2secret\nPATH=/usr/bin"),
            "MY_TOKEN=********\nPATH=/usr/bin"
        );
    }

    #[test]
    fn interleaved_symbols_masked() {
        let m = masker(&["secret123"]);
        assert_eq!(m.mask("x s-e-c-r-e-t-1-2-3 y"), "x ******** y");
        assert_eq!(m.mask("s e c r e t 1 2 3"), "********");
    }

    #[test]
    fn one_char_per_line_masked() {
        let m = masker(&["secret123"]);
        // `echo $X | fold -w1` output.
        assert_eq!(m.mask("s\ne\nc\nr\ne\nt\n1\n2\n3\n"), "********\n");
    }

    #[test]
    fn case_flip_masked() {
        let m = masker(&["secret123"]);
        assert_eq!(m.mask("SECRET123"), "********");
        assert_eq!(m.mask("SeCrEt123"), "********");
    }

    #[test]
    fn gap_over_limit_not_masked() {
        let m = masker(&["secret123"]);
        let spaced = "s    e    c    r    e    t    1    2    3";
        assert_eq!(m.mask(spaced), spaced);
    }

    #[test]
    fn alphanumeric_noise_between_chars_not_masked() {
        let m = masker(&["secret123"]);
        let noisy = "sXeXcXrXeXtX1X2X3";
        assert_eq!(m.mask(noisy), noisy);
    }

    #[test]
    fn short_values_never_masked() {
        let m = masker(&["ab", "1", "tru"]);
        assert!(m.is_empty());
        assert_eq!(m.mask("ab 1 tru"), "ab 1 tru");
    }

    #[test]
    fn short_value_not_interleave_scanned() {
        // 4 chars: exact masking yes, interleaved no (canon < 6).
        let m = masker(&["abcd"]);
        assert_eq!(m.mask("abcd"), "********");
        let spaced = "a b c d";
        assert_eq!(m.mask(spaced), spaced);
    }

    #[test]
    fn multiple_secrets_and_occurrences() {
        let m = masker(&["alphatoken1", "betatoken2"]);
        assert_eq!(
            m.mask("alphatoken1 x betatoken2 x alphatoken1"),
            "******** x ******** x ********"
        );
    }

    #[test]
    fn nested_secret_masks_as_one() {
        let m = masker(&["longsecretvalue", "secret"]);
        assert_eq!(m.mask("longsecretvalue"), "********");
    }

    #[test]
    fn unmatched_text_borrows_unchanged() {
        let m = masker(&["secret123"]);
        assert!(matches!(m.mask("nothing here"), Cow::Borrowed(_)));
    }

    #[test]
    fn utf8_text_survives_masking() {
        let m = masker(&["secret123"]);
        assert_eq!(m.mask("héllo secret123 wörld"), "héllo ******** wörld");
    }

    #[test]
    fn sensitive_names_detected() {
        for n in [
            "GITHUB_TOKEN",
            "npm_token",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "MY_APIKEY",
            "GOOGLE_API_KEY",
            "AUTH_HEADER",
        ] {
            assert!(is_sensitive_env_name(n), "{n} should be sensitive");
        }
        for n in ["PATH", "HOME", "LANG", "TERM", "EDITOR"] {
            assert!(!is_sensitive_env_name(n), "{n} should not be sensitive");
        }
    }

    #[test]
    fn console_fields_masked_in_envelope() {
        let m = masker(&["secret123"]);
        let mut v = serde_json::json!({
            "exit_code": 0,
            "stdout": "value is secret123",
            "stderr": "warn: s-e-c-r-e-t-1-2-3",
            "command": "printenv",
        });
        mask_console_fields(&mut v, &m);
        assert_eq!(v["stdout"], "value is ********");
        assert_eq!(v["stderr"], "warn: ********");
        assert_eq!(v["command"], "printenv");
    }
}
