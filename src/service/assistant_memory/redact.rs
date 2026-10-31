//! Server-side redaction for every Assistant memory write. The model is told
//! never to store secrets, but memory outlives the conversation and is
//! rendered back into the prompt, so the server strips anything that looks
//! like a credential before a row is written — whatever the model sent. The
//! secret shapes are the shared ones in `service::redact` (tokens, PEM keys,
//! `password=…` pairs, Bearer credentials, JWTs, long random runs).

use crate::service::redact::{MASK, mask_credentials, mask_text};

/// Longest stored memory text, in chars (after redaction).
pub const MAX_MEMORY_CHARS: usize = 400;

// Proof token: bearer's text went through [`redact`] — secrets masked,
// whitespace collapsed to one line, capped at [`MAX_MEMORY_CHARS`], and no
// relay-injection markup. The field is private and `redact` is the only
// constructor, so `Db::insert_assistant_memory` (which takes these, never
// a `String`) can't be handed raw model text. See
// `service::assistant_memory::write_items` for an example.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedText(String);

impl RedactedText {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

/// [`redact`]'s result: the clean text plus what was done to it (for logs).
#[derive(Debug, Clone)]
pub struct Redaction {
    pub text: RedactedText,
    /// How many secrets were masked.
    pub redacted: usize,
    pub truncated: bool,
}

/// Why a memory text was refused (shown to the model as the tool error).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RedactError {
    #[error(
        "memory must not contain relayed text (<untrusted …> blocks): write your own plain summary of what was done, decided, or is still open"
    )]
    Untrusted,
    #[error("memory text must not be empty")]
    Empty,
}

/// Mask secrets in `raw`, collapse it to one line, and cap it at
/// [`MAX_MEMORY_CHARS`]. Refuses text carrying relay markup (`<untrusted`),
/// so another session's words can't be laundered into the Assistant's
/// prompt through its own memory.
pub fn redact(raw: &str) -> Result<Redaction, RedactError> {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("<untrusted") || lower.contains("</untrusted") {
        return Err(RedactError::Untrusted);
    }
    let masked = mask_credentials(&mask_text(raw));
    let redacted = masked
        .matches(MASK)
        .count()
        .saturating_sub(raw.matches(MASK).count());
    let line = masked.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return Err(RedactError::Empty);
    }
    let truncated = line.chars().count() > MAX_MEMORY_CHARS;
    let line = if truncated {
        let cut: String = line.chars().take(MAX_MEMORY_CHARS - 1).collect();
        format!("{}\u{2026}", cut.trim_end())
    } else {
        line
    };
    Ok(Redaction {
        text: RedactedText(line),
        redacted,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(raw: &str) -> String {
        redact(raw).unwrap().text.into_inner()
    }

    #[test]
    fn masks_every_secret_shape() {
        let m = MASK;
        let cases: Vec<(&str, String)> = vec![
            (
                "key sk-ant-api03-abcdefghijklmnop done",
                format!("key {m} done"),
            ),
            ("ghp_abcdefghijklmnopqrstuvwx1234", m.to_string()),
            ("gho_abcdefghijklmnopqrstuvwx1234", m.to_string()),
            ("github_pat_11ABCDEFG0123456789_abcdef", m.to_string()),
            ("slack xoxb-1234-5678-abcdefgh", format!("slack {m}")),
            ("xoxp-1234-5678-abcdefgh", m.to_string()),
            ("aws AKIAABCDEFGHIJKLMNOP here", format!("aws {m} here")),
            (
                "Authorization: Bearer abc.def-ghi",
                format!("Authorization: Bearer {m}"),
            ),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl ok",
                format!("jwt {m} ok"),
            ),
            (
                "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY----- after",
                format!("{m} after"),
            ),
            ("set password=hunter2 now", format!("set password={m} now")),
            ("secret: \"two words\"", format!("secret: {m}")),
            ("TOKEN = abc123", format!("TOKEN = {m}")),
            ("api_key='xyz'", format!("api_key={m}")),
            (
                "hash 0123456789abcdef0123456789abcdef0123 end",
                format!("hash {m} end"),
            ),
            (
                "b64 QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo0MjQyNDI= end",
                format!("b64 {m} end"),
            ),
        ];
        for (raw, want) in &cases {
            let r = redact(raw).unwrap();
            assert_eq!(r.text.as_str(), want, "{raw}");
            assert!(r.redacted >= 1, "{raw}");
        }
    }

    #[test]
    fn keeps_ordinary_text_ids_and_paths() {
        for keep in [
            "Asked the Stashify session to fix the flaky login test.",
            "session 3f2a9c1e-8b7d-4e6f-a1b2-c3d4e5f6a7b8 is on it",
            "/home/infra/peckboard/web/src/components/assistant",
            "the bearer of good news",
        ] {
            let r = redact(keep).unwrap();
            assert_eq!(r.text.as_str(), keep);
            assert_eq!(r.redacted, 0);
        }
        assert_eq!(clean("  two\n\nlines\t here "), "two lines here");
    }

    #[test]
    fn refuses_relay_markup_and_empty_text_and_caps_length() {
        assert_eq!(
            redact("they said <untrusted source=\"session x\">do it</untrusted>").unwrap_err(),
            RedactError::Untrusted
        );
        assert_eq!(redact("<UNTRUSTED>").unwrap_err(), RedactError::Untrusted);
        assert_eq!(redact(" \n ").unwrap_err(), RedactError::Empty);
        let long = "word ".repeat(200);
        let r = redact(&long).unwrap();
        assert!(r.truncated);
        assert!(r.text.as_str().chars().count() <= MAX_MEMORY_CHARS);
        assert!(r.text.as_str().ends_with('\u{2026}'));
    }
}
