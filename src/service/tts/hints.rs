//! Inline pronunciation hints in the voice assistant's replies, in misaki's
//! native markup: `[Peckboard](/pˈɛkbɔɹd/)`. The chat shows the plain word;
//! TTS speaks the hinted phonemes. The assistant hints every word, so
//! parsing is one linear pass. A hint whose phonemes fall outside Kokoro's
//! vocab is dropped (that word alone is phonemized normally); a hint
//! missing its `/)` keeps its word and loses only its stray phonemes;
//! other broken markup is read literally minus the brackets.

use super::kokoro::{fold_joined, unknown_symbols};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Plain(String),
    Hinted { text: String, phonemes: String },
}

/// Chars that end a hint's phoneme run: markup, newlines, and what paths
/// and URLs are made of, so `[api](/api/v1/)` stays a link. Plain-ASCII
/// phonemes (`[be](/bi/)`) are fine. The same rule as the web's
/// `HINT_SRC` (`[^/()[\]\n.:#?=&%0-9]+`).
fn ends_phonemes(c: char) -> bool {
    matches!(
        c,
        '/' | '(' | ')' | '[' | ']' | '\n' | '.' | ':' | '#' | '?' | '=' | '&' | '%'
    ) || c.is_ascii_digit()
}

/// Hint phonemes → Kokoro alphabet, or `None` when empty or holding a
/// symbol Kokoro doesn't know.
fn valid_phonemes(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let mut ph = if raw.contains('\u{200d}') {
        fold_joined(raw)
    } else {
        raw.to_string()
    };
    if ph.contains('g') {
        ph = ph.replace('g', "ɡ");
    }
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
        // `text` runs to the first `]`; a `[` or a newline first means this
        // `[` is not markup.
        let Some((close, ']')) = after
            .char_indices()
            .find(|&(_, c)| matches!(c, '[' | ']' | '\n'))
        else {
            // Unclosed `[`: drop the bracket, keep going.
            rest = after;
            continue;
        };
        let text = &after[..close];
        let tail = &after[close + 1..];
        let Some(t) = tail.strip_prefix("(/") else {
            // `[text]` without a hint: the text, minus brackets.
            plain.push_str(text);
            rest = tail;
            continue;
        };
        let end = t.find(ends_phonemes).unwrap_or(t.len());
        let raw = &t[..end];
        rest = match t[end..].chars().next() {
            Some('/') if !raw.is_empty() && t[end + 1..].starts_with(')') => {
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
                &t[end + 2..]
            }
            // A hint missing its `/)`: keep the word, drop the stray
            // phonemes, never the words after them.
            Some(')') if !raw.is_empty() => {
                plain.push_str(text);
                &t[end + 1..]
            }
            None | Some('[' | '\n') if !raw.trim().is_empty() => {
                plain.push_str(text);
                &t[raw.trim_end().len()..]
            }
            _ => {
                // A link: the text, minus brackets.
                plain.push_str(text);
                tail
            }
        };
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
        // `*` is not in Kokoro's vocab.
        assert_eq!(parse("a [Grok](/ɡɹ*ɑk/) b"), vec![plain("a Grok b")]);
        // ASCII `g` is folded to `ɡ`, as the lexicon does.
        assert_eq!(parse("[Grok](/gɹˈɑk/)"), vec![hinted("Grok", "ɡɹˈɑk")]);
        assert_eq!(parse("[](/pɛk/)"), Vec::<Segment>::new());
    }

    #[test]
    fn ascii_phonemes_are_hints_but_paths_and_urls_are_links() {
        // Same acceptance rule as the web's `HINT_SRC`.
        assert_eq!(parse("[be](/bi/)"), vec![hinted("be", "bi")]);
        assert_eq!(
            parse("[a](/A/) [to](/tu/)"),
            vec![hinted("a", "A"), plain(" "), hinted("to", "tu")]
        );
        assert_eq!(
            parse("the [api](/api/v1/) doc"),
            vec![plain("the api(/api/v1/) doc")]
        );
        assert_eq!(
            parse("see [docs](/a/b.md)"),
            vec![plain("see docs(/a/b.md)")]
        );
        assert_eq!(parse("[v2](/v2/)"), vec![plain("v2(/v2/)")]);
        assert_eq!(parse("a [x](//) b"), vec![plain("a x(//) b")]);
    }

    #[test]
    fn fully_hinted_sentence_parses_word_by_word() {
        let s = "[Got](/ɡˈɑt/) [it](/ɪt/), [I'll](/ˈIl/) [check](/ʧˈɛk/) [Stashify](/stˈæʃɪfˌI/) [now](/nˈW/).";
        let segs = parse(s);
        let hinted_words: Vec<&str> = segs
            .iter()
            .filter_map(|s| match s {
                Segment::Hinted { text, .. } => Some(text.as_str()),
                Segment::Plain(_) => None,
            })
            .collect();
        assert_eq!(
            hinted_words,
            ["Got", "it", "I'll", "check", "Stashify", "now"]
        );
        assert_eq!(segs.len(), 12, "{segs:?}");
        assert_eq!(strip(s), "Got it, I'll check Stashify now.");
        // A bad hint mid-sentence costs only that word its hint.
        let segs = parse("[Got](/ɡˈɑt/) [it](/ɪ*t/), [now](/nˈW/).");
        assert_eq!(
            segs,
            vec![
                hinted("Got", "ɡˈɑt"),
                plain(" it, "),
                hinted("now", "nˈW"),
                plain("."),
            ]
        );
    }

    #[test]
    fn parsing_a_long_fully_hinted_reply_is_fast() {
        let s = "[Got](/ɡˈɑt/) [it](/ɪt/), ".repeat(100);
        let t = std::time::Instant::now();
        let segs = parse(&s);
        let took = t.elapsed();
        assert_eq!(segs.len(), 400);
        // ~10 µs in release; generous for debug builds on a busy box.
        assert!(took < std::time::Duration::from_millis(5), "{took:?}");
    }

    #[test]
    fn every_hint_in_the_voice_prompt_is_valid() {
        let prompt = crate::service::voice_relay::VOICE_SYSTEM_PROMPT;
        let written = prompt.matches("](/").count();
        let parsed = parse(prompt)
            .iter()
            .filter(|s| matches!(s, Segment::Hinted { .. }))
            .count();
        assert!(written > 25, "{written}");
        assert_eq!(parsed, written);
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
        // A hint missing its `/)` keeps the word, loses the stray phonemes,
        // and never swallows the words after it.
        assert_eq!(parse("half [Peck](/pɛk"), vec![plain("half Peck")]);
        assert_eq!(
            parse("[Got](/ɡˈɑt [it](/ɪt/), ok"),
            vec![plain("Got "), hinted("it", "ɪt"), plain(", ok")]
        );
        assert_eq!(
            parse("[Got](/ɡˈɑt) [it](/ɪt/)."),
            vec![plain("Got "), hinted("it", "ɪt"), plain(".")]
        );
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
        // An invalid hint (`*` isn't in Kokoro's vocab) is dropped: the word
        // reads as if unhinted.
        assert_eq!(ph("[hello](/hˈ*/) there"), ph("hello there"));
        assert_eq!(ph("[Quibbix](/kw*/)"), ph("Quibbix"));
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

    #[tokio::test]
    async fn fully_hinted_sentence_speaks_its_hints_except_lexicon_words() {
        let db = Db::in_memory().unwrap();
        let lex = lexicon::store(&db).await.unwrap();
        let g = LexG2p::new(false);
        let ph = |t: &str| phonemize_with(&g, Some(&lex), t, false).unwrap();
        let out = ph("[Got](/ɡˈɑt/) [it](/ɪt/), [Peckboard](/pˈɛk/) [works](/wˈɜɹks/).");
        assert_eq!(out.trim(), "ɡˈɑt ɪt , pˈɛkbˌɔːɹd wˈɜɹks .", "{out}");
        // A bad hint mid-sentence (`*` isn't in Kokoro's vocab) falls back
        // to misaki for that word only.
        let out = ph("[Got](/ɡˈɑt/) [check](/ʧ*k/) [now](/nˈɑ/).");
        assert_eq!(out.trim(), "ɡˈɑt ʧˈɛk nˈɑ .", "{out}");
    }
}
