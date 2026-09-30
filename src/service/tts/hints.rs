//! Inline pronunciation hints in the voice assistant's replies, in misaki's
//! native markup: `[Peckboard](/pˈɛkbɔɹd/)`. The chat shows the plain word;
//! TTS speaks the hinted phonemes. A hint whose phonemes fall outside
//! Kokoro's vocab is dropped (the word is phonemized normally), and broken
//! markup is read literally minus the brackets.

use super::kokoro::{fold_joined, unknown_symbols};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Plain(String),
    Hinted { text: String, phonemes: String },
}

/// Hint phonemes → Kokoro alphabet, or `None` when empty, holding a symbol
/// Kokoro doesn't know, or plain printable ASCII — real phonemes carry a
/// stress mark, IPA symbol or space, so `[docs](/docs/)` is a link, not a
/// hint (same rule as the web's `stripPronunciationHints`).
fn valid_phonemes(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    let ph = fold_joined(raw).replace('g', "ɡ");
    (!ph.is_empty() && unknown_symbols(&ph).is_empty()).then_some(ph)
}

/// Split `sentence` into plain runs and hinted words.
pub fn parse(sentence: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut plain = String::new();
    let mut rest = sentence;
    while let Some(open) = rest.find('[') {
        plain.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        // `text` runs to the first `]`; a nested `[` or a newline first
        // means this `[` is not markup.
        let close = after
            .find(']')
            .filter(|&c| !after[..c].contains(['[', '\n']));
        let Some(close) = close else {
            // Unclosed `[`: drop the bracket, keep going.
            rest = after;
            continue;
        };
        let text = &after[..close];
        let tail = &after[close + 1..];
        let hint = tail.strip_prefix("(/").and_then(|t| {
            let end = t.find("/)")?;
            Some((&t[..end], &t[end + 2..]))
        });
        match hint {
            Some((raw, next)) if !raw.contains('\n') => {
                match valid_phonemes(raw).filter(|_| !text.trim().is_empty()) {
                    Some(phonemes) => {
                        if !plain.is_empty() {
                            out.push(Segment::Plain(std::mem::take(&mut plain)));
                        }
                        out.push(Segment::Hinted {
                            text: text.to_string(),
                            phonemes,
                        });
                    }
                    None => plain.push_str(text),
                }
                rest = next;
            }
            _ => {
                // `[text]` without a hint: the text, minus brackets.
                plain.push_str(text);
                rest = tail;
            }
        }
    }
    plain.push_str(rest);
    if !plain.is_empty() {
        out.push(Segment::Plain(plain));
    }
    out
}

/// `sentence` with every hint replaced by its plain word.
pub fn strip(sentence: &str) -> String {
    parse(sentence)
        .into_iter()
        .map(|s| match s {
            Segment::Plain(t) | Segment::Hinted { text: t, .. } => t,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Segment {
        Segment::Plain(s.into())
    }

    fn hinted(t: &str, p: &str) -> Segment {
        Segment::Hinted {
            text: t.into(),
            phonemes: p.into(),
        }
    }

    #[test]
    fn parses_hints_between_plain_text() {
        assert_eq!(
            parse("Open [Peckboard](/pˈɛkbɔɹd/) and [Kokoro](/kOkˈOɹO/)."),
            vec![
                plain("Open "),
                hinted("Peckboard", "pˈɛkbɔɹd"),
                plain(" and "),
                hinted("Kokoro", "kOkˈOɹO"),
                plain("."),
            ]
        );
        assert_eq!(parse("no hints here"), vec![plain("no hints here")]);
        assert_eq!(strip("Say [Grok](/ɡɹˈɑk/)!"), "Say Grok!");
    }

    #[test]
    fn joined_diphthongs_are_folded() {
        assert_eq!(parse("[say](/se\u{200d}ɪ/)"), vec![hinted("say", "sA")]);
    }

    #[test]
    fn invalid_hint_keeps_the_word_as_plain_text() {
        // `#` is not in Kokoro's vocab.
        assert_eq!(parse("a [Grok](/ɡɹ#ɑk/) b"), vec![plain("a Grok b")]);
        // ASCII `g` is folded to `ɡ`, as the lexicon does.
        assert_eq!(parse("[Grok](/gɹˈɑk/)"), vec![hinted("Grok", "ɡɹˈɑk")]);
        assert_eq!(parse("a [Grok](//) b"), vec![plain("a Grok b")]);
        // Plain-ASCII "phonemes" are a relative link, not a hint.
        assert_eq!(parse("see [docs](/docs/)"), vec![plain("see docs")]);
        assert_eq!(parse("[](/pɛk/)"), Vec::<Segment>::new());
    }

    #[test]
    fn malformed_markup_is_read_literally_minus_brackets() {
        assert_eq!(
            parse("an [unclosed bracket"),
            vec![plain("an unclosed bracket")]
        );
        assert_eq!(
            parse("a [link](https://x.y) c"),
            vec![plain("a link(https://x.y) c")]
        );
        assert_eq!(parse("half [Peck](/pɛk"), vec![plain("half Peck(/pɛk")]);
        assert_eq!(
            parse("[a [b](/bˈi/)"),
            vec![plain("a "), hinted("b", "bˈi")]
        );
    }
}

/// Hints, lexicon, misaki and the letter-to-sound fallback together, as the
/// TTS pipeline runs them ([`super::kokoro::phonemize_with`]).
#[cfg(test)]
mod pipeline_tests {
    use crate::db::Db;
    use crate::service::tts::kokoro::phonemize_with;
    use crate::service::tts::l2s;
    use crate::service::tts::lexicon::{self, LexG2p};

    #[tokio::test]
    async fn precedence_lexicon_then_hint_then_misaki_then_guess() {
        let db = Db::in_memory().unwrap();
        let lex = lexicon::store(&db).await.unwrap();
        let g = LexG2p::new(false);
        let ph = |t: &str| phonemize_with(&g, Some(&lex), t, false).unwrap();

        // A valid hint is spoken as written, and is not an unknown word.
        let out = ph("Ask [Zorblax](/zˈɔɹbləks/) now.");
        assert!(out.contains("zˈɔɹbləks"), "{out}");
        // The user lexicon beats the model's hint.
        let out = ph("Open [Peckboard](/pˈɛk/) now.");
        assert!(out.contains("pˈɛkbˌɔːɹd"), "{out}");
        // An invalid hint (ASCII `#`) is dropped: the word reads as if unhinted.
        assert_eq!(ph("[hello](/hˈ#/) there"), ph("hello there"));
        assert_eq!(ph("[Quibbix](/kw#/)"), ph("Quibbix"));
        lex.flush_unknown().await.unwrap();
        let unknown: Vec<String> = db
            .list_tts_unknown()
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.word)
            .collect();
        assert!(!unknown.contains(&"zorblax".to_string()), "{unknown:?}");

        // An unknown word is guessed from its spelling, not spelled out, and
        // tallied as unknown.
        let out = ph("Quibbix");
        assert_eq!(out.trim(), l2s::guess("Quibbix").unwrap(), "{out}");
        lex.flush_unknown().await.unwrap();
        let unknown = db.list_tts_unknown().await.unwrap();
        assert!(unknown.iter().any(|u| u.word == "quibbix"), "{unknown:?}");
    }

    #[tokio::test]
    async fn short_all_caps_token_is_spelled() {
        let db = Db::in_memory().unwrap();
        let lex = lexicon::store(&db).await.unwrap();
        let out = phonemize_with(&LexG2p::new(false), Some(&lex), "QXZ", false).unwrap();
        assert_eq!(out.trim(), l2s::spell_letters("QXZ"), "{out}");
    }
}
