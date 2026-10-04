//! Pure text side of the mirror: which voice-session events become
//! messages ([`TurnAssembler`]), scrubbing ([`scrub`]), splitting for each
//! service's size limit ([`split_message`]) and the webhook payloads.

use crate::service::secret_mask::SecretMasker;
use crate::service::voice_relay::RELAY_PREFIX;

/// Prefix the browser puts on an utterance that talked over the assistant
/// (`INTERRUPT_MARKER` in `web/src/voice/text.ts`). Stripped before posting.
pub const INTERRUPT_MARKER: &str =
    "[user interrupted; the rest of your previous reply was not heard, do not repeat it] ";

/// Discord rejects `content` over 2000 chars.
pub const DISCORD_LIMIT: usize = 2000;
/// Slack truncates a text block at ~3000 chars.
pub const SLACK_LIMIT: usize = 3000;
/// Parts per message before the rest is cut.
pub const MAX_PARTS: usize = 4;
pub const CONTINUED: &str = "… (continued in Peckboard)";
pub const CODE_OMITTED: &str = "[code omitted]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    You,
    Assistant,
}

/// One mirrored message: a user utterance or a whole assistant reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub speaker: Speaker,
    pub text: String,
}

impl Line {
    pub fn render(&self) -> String {
        let who = match self.speaker {
            Speaker::You => "You",
            Speaker::Assistant => "Assistant",
        };
        format!("{who}: {}", self.text)
    }
}

/// Turns the voice session's event stream into [`Line`]s: user utterances
/// at once, the assistant's text buffered until `agent-end`. Thinking,
/// tool, and subagent events never produce output.
#[derive(Default)]
pub struct TurnAssembler {
    reply: String,
    prev_was_text: bool,
}

impl TurnAssembler {
    pub fn feed(&mut self, kind: &str, data: &serde_json::Value) -> Option<Line> {
        if kind != "agent-text" {
            self.prev_was_text = false;
        }
        match kind {
            "user" => user_line(data),
            "agent-text" => {
                if data.get("parentToolUseId").is_some() {
                    return None;
                }
                let text = data.get("text").and_then(|t| t.as_str())?;
                // Same joining as `subagent::last_reply_text`: streamed
                // chunks concatenate, text resumed after a tool call starts
                // a new paragraph.
                if !self.prev_was_text && !self.reply.is_empty() {
                    self.reply.push_str("\n\n");
                }
                self.reply.push_str(text);
                self.prev_was_text = true;
                None
            }
            "agent-end" => {
                let reply = std::mem::take(&mut self.reply);
                let text = crate::service::tts::hints::strip(&reply);
                let text = text.trim();
                (!text.is_empty()).then(|| Line {
                    speaker: Speaker::Assistant,
                    text: text.to_string(),
                })
            }
            _ => None,
        }
    }
}

/// A user event worth mirroring: spoken or typed by the user (no source,
/// or `voice-mic` / `voice-typed`). Relay turns, action notes, and other
/// system-originated user events are dropped.
fn user_line(data: &serde_json::Value) -> Option<Line> {
    match data.get("source").and_then(|s| s.as_str()) {
        None | Some("voice-mic" | "voice-typed") => {}
        Some(_) => return None,
    }
    let text = data.get("text").and_then(|t| t.as_str())?;
    if text.starts_with(RELAY_PREFIX) {
        return None;
    }
    let text = text.strip_prefix(INTERRUPT_MARKER).unwrap_or(text).trim();
    (!text.is_empty()).then(|| Line {
        speaker: Speaker::You,
        text: text.to_string(),
    })
}

/// Everything that leaves the host goes through here, in order: the
/// value-based secret masker (env vars, server keys, mirror credentials),
/// the pattern masks, then — when on — code blocks are dropped.
pub fn scrub(text: &str, masker: &SecretMasker, redact_code_blocks: bool) -> String {
    let s = masker.mask(text);
    let s = crate::service::redact::mask_text(&s);
    let s = crate::service::redact::mask_credentials(&s);
    if redact_code_blocks {
        strip_code_blocks(&s)
    } else {
        s
    }
}

/// Replace fenced code blocks (``` / ~~~, an unclosed one runs to the end)
/// and indented blocks (4 spaces or a tab, after a blank line) with
/// [`CODE_OMITTED`].
pub fn strip_code_blocks(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut fence: Option<&str> = None;
    let mut in_indented = false;
    let mut prev_blank = true;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(f) = fence {
            if trimmed.starts_with(f) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = Some(&trimmed[..3]);
            out.push(CODE_OMITTED);
            in_indented = false;
            prev_blank = false;
            continue;
        }
        let indented = (line.starts_with("    ") || line.starts_with('\t')) && !trimmed.is_empty();
        if indented && (in_indented || prev_blank) {
            if !in_indented {
                out.push(CODE_OMITTED);
                in_indented = true;
            }
            continue;
        }
        in_indented = false;
        prev_blank = trimmed.is_empty();
        out.push(line);
    }
    out.join("\n")
}

/// Split into at most [`MAX_PARTS`] parts of ≤ `limit` chars, preferring
/// line, then word, boundaries. Past the last part the text is cut and the
/// last part ends with [`CONTINUED`].
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            parts.push(rest.to_string());
            break;
        }
        if parts.len() + 1 == MAX_PARTS {
            let budget = limit.saturating_sub(CONTINUED.chars().count() + 1);
            let (head, _) = cut(rest, budget);
            parts.push(format!("{}\n{CONTINUED}", head.trim_end()));
            break;
        }
        let (head, tail) = cut(rest, limit);
        parts.push(head.trim_end().to_string());
        rest = tail.trim_start();
    }
    parts
}

/// Split `s` at most `limit` chars in, at the last newline (else space) in
/// the second half of the window, else hard.
fn cut(s: &str, limit: usize) -> (&str, &str) {
    let end = s.char_indices().nth(limit).map_or(s.len(), |(i, _)| i);
    let window = &s[..end];
    let min = window.len() / 2;
    let at = window
        .rfind('\n')
        .filter(|&i| i > min)
        .or_else(|| window.rfind(' ').filter(|&i| i > min))
        .unwrap_or(end);
    (&s[..at], &s[at..])
}

/// Slack mrkdwn control characters.
pub fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn slack_payload(part: &str) -> serde_json::Value {
    serde_json::json!({ "text": slack_escape(part) })
}

/// `allowed_mentions.parse = []`: `@everyone`, `@here`, role and user
/// mentions in mirrored text never ping anyone.
pub fn discord_payload(part: &str) -> serde_json::Value {
    serde_json::json!({ "content": part, "allowed_mentions": { "parse": [] } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn feed_all(events: &[(&str, serde_json::Value)]) -> Vec<Line> {
        let mut a = TurnAssembler::default();
        events.iter().filter_map(|(k, d)| a.feed(k, d)).collect()
    }

    #[test]
    fn one_post_per_turn_relays_dropped_interrupt_stripped() {
        let lines = feed_all(&[
            (
                "user",
                json!({ "text": format!("{INTERRUPT_MARKER}stop, do the short one"),
                        "source": "voice-mic" }),
            ),
            ("agent-start", json!({})),
            ("agent-thinking", json!({ "text": "hmm secret plan" })),
            ("agent-text", json!({ "text": "[Sure](/ʃˈʊɹ/), " })),
            ("agent-text", json!({ "text": "on it." })),
            ("agent-tool-start", json!({ "name": "send_message" })),
            (
                "agent-text",
                json!({ "text": "inner", "parentToolUseId": "t1" }),
            ),
            ("agent-tool-end", json!({})),
            ("agent-text", json!({ "text": "Sent." })),
            ("agent-end", json!({ "status": "complete" })),
            (
                "user",
                json!({ "text": "[relay] update from dev: done", "source": "voice-relay" }),
            ),
            ("user", json!({ "text": "[relay] action confirmed" })),
            ("user", json!({ "text": "note", "source": "voice-action" })),
            ("agent-text", json!({ "text": "Dev finished." })),
            ("agent-end", json!({})),
            ("agent-end", json!({})),
        ]);
        assert_eq!(
            lines,
            vec![
                Line {
                    speaker: Speaker::You,
                    text: "stop, do the short one".into()
                },
                Line {
                    speaker: Speaker::Assistant,
                    text: "Sure, on it.\n\nSent.".into()
                },
                // The reply to a relay turn is still mirrored.
                Line {
                    speaker: Speaker::Assistant,
                    text: "Dev finished.".into()
                },
            ]
        );
        assert_eq!(lines[0].render(), "You: stop, do the short one");
    }

    #[test]
    fn split_respects_limits_and_truncates_after_four_parts() {
        assert_eq!(split_message("short", 10), vec!["short"]);
        let words = "word ".repeat(60);
        let parts = split_message(&words, 40);
        assert_eq!(parts.len(), MAX_PARTS);
        assert!(parts.iter().all(|p| p.chars().count() <= 40), "{parts:?}");
        assert!(parts[3].ends_with(CONTINUED), "{parts:?}");
        assert!(parts[0].starts_with("word"));

        let two = format!("{}\n{}", "a".repeat(15), "b".repeat(15));
        assert_eq!(
            split_message(&two, 20),
            vec!["a".repeat(15), "b".repeat(15)]
        );
        // No boundary: hard cut, multibyte-safe.
        let parts = split_message(&"é".repeat(45), 20);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].chars().count(), 20);
        // Real limits.
        let long = "x ".repeat(5000);
        for (limit, n) in [(DISCORD_LIMIT, 4), (SLACK_LIMIT, 4)] {
            let p = split_message(&long, limit);
            assert_eq!(p.len(), n);
            assert!(p.iter().all(|s| s.chars().count() <= limit));
        }
    }

    #[test]
    fn code_blocks_are_omitted() {
        let t = "Run this:\n```sh\nexport TOKEN=abc\n```\nthen\n\n    $ ls\n    out\nback";
        assert_eq!(
            strip_code_blocks(t),
            "Run this:\n[code omitted]\nthen\n\n[code omitted]\nback"
        );
        assert_eq!(strip_code_blocks("x\n~~~\nopen"), "x\n[code omitted]");
        // An indented list continuation right after text is kept.
        assert_eq!(strip_code_blocks("- a\n    more"), "- a\n    more");
    }

    #[test]
    fn scrub_runs_every_pass() {
        let masker = SecretMasker::new(["hunter2-value".to_string()]);
        let t = scrub(
            "pw hunter2-value, key ghp_abcdefghijklmnopqrstuvwxyz012345\n```\ncode\n```",
            &masker,
            true,
        );
        assert!(!t.contains("hunter2-value"), "{t}");
        assert!(!t.contains("ghp_"), "{t}");
        assert!(t.ends_with(CODE_OMITTED), "{t}");
        let kept = scrub("```\ncode\n```", &masker, false);
        assert!(kept.contains("code"));
    }

    #[test]
    fn payloads_escape_and_disable_mentions() {
        assert_eq!(
            slack_payload("a <b> & @channel")["text"],
            "a &lt;b&gt; &amp; @channel"
        );
        let d = discord_payload("@everyone hi");
        assert_eq!(d["content"], "@everyone hi");
        assert_eq!(d["allowed_mentions"]["parse"], json!([]));
    }
}
