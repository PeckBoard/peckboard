//! Custom pronunciations layered over misaki's dictionary, plus a tally of
//! the words misaki doesn't know.
//!
//! Entries live in the `tts_lexicon` table and are mirrored in memory by
//! [`LexiconStore`]; every write goes through the store, which updates the
//! cache synchronously, so an edit applies to the very next sentence.
//! [`LexiconStore::apply`] rewrites every lexicon word in a sentence
//! (case-insensitive, whole word, with `'s` / `s` suffixes) into misaki's own
//! `[word](/phonemes/)` override syntax, so misaki still tokenizes, tags and
//! punctuates the sentence but emits our phonemes verbatim for that word.
//! Words misaki can't find reach its OOV fallback ([`LexG2p`]), which
//! guesses them from their spelling; the caller logs each once per process
//! and tallies it in `tts_unknown_words` (batched, off the synthesis path).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use std::time::Duration;

use misaki_rs::fallback::{Fallback, FallbackError};
use misaki_rs::{G2P, Language};
use regex::Regex;

use super::{kokoro, l2s};
use crate::db::Db;
use crate::db::models::{TtsLexiconEntry, TtsUnknownWord};

pub const SOURCE_DEFAULT: &str = "default";
pub const SOURCE_USER: &str = "user";

/// How often buffered unknown-word sightings are written to the DB.
const FLUSH_EVERY: Duration = Duration::from_secs(5);

/// misaki's vowel symbols (Kokoro alphabet): stress marks go right before one.
const VOWELS: &str = "AIOQWYaiuæɑɒɔəɛɜɪʊʌᵻɚ";

/// Words with an optional possessive, matched the way misaki subtokenizes.
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+(?:['’]\p{L}+)?").expect("word regex"));

/// A lexicon key: letters and digits only.
static KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}]+$").expect("key regex"));

#[derive(Debug, thiserror::Error)]
pub enum LexiconError {
    /// Bad input — maps to HTTP 400.
    #[error("{0}")]
    Invalid(String),
    #[error("{0:#}")]
    Failed(#[from] anyhow::Error),
}

/// Built-in defaults: one `display phonemes` pair per line, `#` comments.
pub fn defaults() -> Vec<(String, String)> {
    include_str!("lexicon_defaults.tsv")
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let (word, ph) = l.split_once(char::is_whitespace)?;
            Some((word.to_string(), ph.trim().to_string()))
        })
        .collect()
}

/// misaki's `Lexicon::append_s`: `-s` after voiceless stops/fricatives,
/// `-ᵻz`/`-ɪz` after sibilants, `-z` otherwise.
fn append_s(ph: &str, british: bool) -> String {
    match ph.chars().last() {
        Some(c) if "ptkfθ".contains(c) => format!("{ph}s"),
        Some(c) if "szʃʒʧʤ".contains(c) => format!("{ph}{}z", if british { "ɪ" } else { "ᵻ" }),
        _ => format!("{ph}z"),
    }
}

/// Lowercased, curly apostrophe folded, possessive stripped.
fn unknown_key(word: &str) -> String {
    let lower = word.to_lowercase().replace('’', "'");
    let lower = lower.strip_suffix("'s").unwrap_or(&lower);
    lower.trim_end_matches('\'').to_string()
}

/// Validate raw phonemes: fold misaki's joined pairs (and ASCII `g`), then
/// require every symbol to be in Kokoro's vocab.
pub fn validate_phonemes(raw: &str) -> Result<String, LexiconError> {
    let ph = kokoro::fold_joined(raw.trim()).replace('g', "ɡ");
    if ph.is_empty() {
        return Err(LexiconError::Invalid("phonemes are empty".into()));
    }
    let bad = kokoro::unknown_symbols(&ph);
    if !bad.is_empty() {
        let list: Vec<String> = bad.iter().map(|c| format!("'{c}'")).collect();
        return Err(LexiconError::Invalid(format!(
            "phonemes contain symbols Kokoro doesn't know: {}",
            list.join(", ")
        )));
    }
    Ok(ph)
}

fn strip_stress(ph: &str) -> String {
    ph.chars().filter(|c| !matches!(c, 'ˈ' | 'ˌ')).collect()
}

fn has_stress(ph: &str) -> bool {
    ph.contains(['ˈ', 'ˌ'])
}

/// Put `mark` right before the first vowel (misaki's placement).
fn stress_at(ph: &str, mark: char) -> String {
    match ph.char_indices().find(|(_, c)| VOWELS.contains(*c)) {
        Some((i, _)) => format!("{}{mark}{}", &ph[..i], &ph[i..]),
        None => ph.to_string(),
    }
}

/// Respelling syllables → phonemes, longest match first (Wikipedia-style
/// pronunciation respelling: `koh`, `ih`, `ay`, `oo`, `sh`, …).
const PHONICS: &[(&str, &str)] = &[
    ("eye", "I"),
    ("igh", "I"),
    ("air", "ɛɹ"),
    ("ear", "ɪɹ"),
    ("eer", "ɪɹ"),
    ("oor", "ʊɹ"),
    ("ah", "ɑ"),
    ("ar", "ɑɹ"),
    ("aw", "ɔ"),
    ("ay", "A"),
    ("ai", "A"),
    ("eh", "ɛ"),
    ("ee", "i"),
    ("er", "ɜɹ"),
    ("ur", "ɜɹ"),
    ("ih", "ɪ"),
    ("oh", "O"),
    ("oa", "O"),
    ("oo", "u"),
    ("or", "ɔɹ"),
    ("ow", "W"),
    ("oy", "Y"),
    ("oi", "Y"),
    ("uh", "ʌ"),
    ("ew", "ju"),
    ("ch", "ʧ"),
    ("sh", "ʃ"),
    ("th", "θ"),
    ("dh", "ð"),
    ("zh", "ʒ"),
    ("ng", "ŋ"),
    ("ck", "k"),
    ("ph", "f"),
    ("a", "æ"),
    ("b", "b"),
    ("c", "k"),
    ("d", "d"),
    ("e", "ɛ"),
    ("f", "f"),
    ("g", "ɡ"),
    ("h", "h"),
    ("i", "ɪ"),
    ("j", "ʤ"),
    ("k", "k"),
    ("l", "l"),
    ("m", "m"),
    ("n", "n"),
    ("o", "ɑ"),
    ("p", "p"),
    ("q", "k"),
    ("r", "ɹ"),
    ("s", "s"),
    ("t", "t"),
    ("u", "ʌ"),
    ("v", "v"),
    ("w", "w"),
    ("x", "ks"),
    ("z", "z"),
];

/// Sound out one lowercase respelling syllable. A final `y` after a
/// consonant is /aɪ/ (`fy`), otherwise /j/.
fn phonics(part: &str) -> String {
    let chars: Vec<char> = part.chars().filter(|c| *c != '\'').collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'y' {
            out.push_str(if i + 1 == chars.len() && i > 0 {
                "I"
            } else {
                "j"
            });
            i += 1;
            continue;
        }
        let hit = (1..=3).rev().find_map(|n| {
            let s: String = chars.get(i..i + n)?.iter().collect();
            PHONICS.iter().find(|(k, _)| *k == s).map(|(_, v)| (n, *v))
        });
        match hit {
            Some((n, v)) => {
                out.push_str(v);
                i += n;
            }
            None => i += 1,
        }
    }
    out
}

/// misaki G2P with a spelling-based OOV fallback that records what it guessed.
pub struct LexG2p {
    g2p: G2P,
    /// Words the fallback handled since the last [`LexG2p::g2p`] call.
    unknown: Arc<Mutex<Vec<String>>>,
    /// Uppercase letter → phonemes (misaki's gold letter names).
    letters: Arc<OnceLock<HashMap<char, String>>>,
}

/// misaki's OOV fallback: note the word, then guess its pronunciation from
/// the spelling ([`l2s::guess`]; short ALL-CAPS tokens are spelled there).
/// Only what the guess can't handle (digits, non-ASCII) is spelled out
/// letter by letter, as misaki does without a fallback.
struct SpellFallback {
    unknown: Arc<Mutex<Vec<String>>>,
    letters: Arc<OnceLock<HashMap<char, String>>>,
}

impl Fallback for SpellFallback {
    fn phonemize(&self, word: &str) -> Result<String, FallbackError> {
        self.unknown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(word.to_string());
        if let Some(ph) = l2s::guess(word) {
            return Ok(ph);
        }
        // An `Err` would fail the whole sentence; unspellable chars are
        // dropped instead, like misaki's own per-char `❓`.
        let Some(letters) = self.letters.get() else {
            return Ok(String::new());
        };
        let spelled: Vec<&str> = word
            .chars()
            .filter_map(|c| letters.get(&c.to_ascii_uppercase()).map(String::as_str))
            .collect();
        Ok(spelled.join(" "))
    }
}

impl LexG2p {
    pub fn new(british: bool) -> Self {
        let unknown = Arc::new(Mutex::new(Vec::new()));
        let letters = Arc::new(OnceLock::new());
        let lang = if british {
            Language::EnglishGB
        } else {
            Language::EnglishUS
        };
        let g2p = G2P::with_fallback(
            lang,
            Some(Box::new(SpellFallback {
                unknown: unknown.clone(),
                letters: letters.clone(),
            })),
        );
        let table = ('A'..='Z')
            .filter_map(|c| {
                let (ph, _) = g2p.lexicon.lookup(&c.to_string(), "NN", None, None)?;
                Some((c, ph))
            })
            .collect();
        let _ = letters.set(table);
        Self {
            g2p,
            unknown,
            letters,
        }
    }

    /// Raw misaki phonemes for `text`, plus the words misaki didn't know.
    pub fn g2p(&self, text: &str) -> anyhow::Result<(String, Vec<String>)> {
        self.unknown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let (ph, _) = self
            .g2p
            .g2p(text)
            .map_err(|e| anyhow::anyhow!("g2p: {e:?}"))?;
        let unknown = std::mem::take(&mut *self.unknown.lock().unwrap_or_else(|e| e.into_inner()));
        Ok((ph, unknown))
    }

    fn letter(&self, c: char) -> Option<&str> {
        self.letters
            .get()?
            .get(&c.to_ascii_uppercase())
            .map(String::as_str)
    }
}

/// Shared US G2P for respelling conversion (independent of the model).
static RESPELL_G2P: LazyLock<Mutex<LexG2p>> = LazyLock::new(|| Mutex::new(LexG2p::new(false)));

/// Convert a respelling to phonemes: words split on spaces, syllables on
/// hyphens. A syllable misaki knows as a word (`board`, `trade`) uses its
/// dictionary pronunciation; anything else is sounded out (`koh`, `ih`);
/// a single letter is said as the letter. An ALL-CAPS syllable carries the
/// primary stress; other dictionary syllables keep a secondary stress,
/// sounded-out ones none. A word without an ALL-CAPS syllable stresses its
/// first syllable (a lone dictionary word keeps misaki's stress).
///
/// `PECK-board` → `pˈɛkbˌɔːɹd`, `koh-KOH-roh` → `kOkˈOɹO`.
pub fn respell(respelling: &str) -> Result<String, LexiconError> {
    struct Part {
        ph: String,
        dict: bool,
        caps: bool,
    }
    let g = RESPELL_G2P.lock().unwrap_or_else(|e| e.into_inner());
    let mut words = Vec::new();
    for group in respelling.split_whitespace() {
        let mut parts = Vec::new();
        for raw in group.split('-').filter(|p| !p.is_empty()) {
            if !raw.chars().all(|c| c.is_alphabetic() || c == '\'') {
                return Err(LexiconError::Invalid(format!(
                    "respelling may only contain letters, spaces and hyphens (got '{raw}')"
                )));
            }
            let caps = raw.chars().any(char::is_alphabetic) && raw == raw.to_uppercase();
            let mut letters = raw.chars().filter(|c| c.is_alphabetic());
            let part = match (letters.next(), letters.next()) {
                (Some(c), None) => Part {
                    ph: strip_stress(g.letter(c).unwrap_or_default()),
                    dict: false,
                    caps,
                },
                _ => {
                    let lower = raw.to_lowercase();
                    let (ph, unknown) = g.g2p(&lower)?;
                    let ph = kokoro::normalize_phonemes(ph.trim());
                    if unknown.is_empty() && !ph.trim().is_empty() {
                        Part {
                            ph: ph.trim().to_string(),
                            dict: true,
                            caps,
                        }
                    } else {
                        Part {
                            ph: phonics(&lower),
                            dict: false,
                            caps,
                        }
                    }
                }
            };
            parts.push(part);
        }
        if parts.is_empty() {
            continue;
        }
        let any_caps = parts.iter().any(|p| p.caps);
        if !any_caps && parts.len() == 1 && parts[0].dict {
            words.push(parts.remove(0).ph);
            continue;
        }
        let mut word = String::new();
        for (i, p) in parts.iter().enumerate() {
            let base = strip_stress(&p.ph);
            let primary = if any_caps { p.caps } else { i == 0 };
            let mark = if primary {
                Some('ˈ')
            } else if p.dict && has_stress(&p.ph) {
                Some('ˌ')
            } else {
                None
            };
            word.push_str(&match mark {
                Some(m) => stress_at(&base, m),
                None => base,
            });
        }
        words.push(word);
    }
    let ph = words.join(" ");
    if ph.trim().is_empty() {
        return Err(LexiconError::Invalid("respelling is empty".into()));
    }
    validate_phonemes(&ph)
}

/// Phonemes for an entry given exactly one of a respelling or raw phonemes.
pub fn resolve_phonemes(
    respelling: Option<&str>,
    phonemes: Option<&str>,
) -> Result<(Option<String>, String), LexiconError> {
    let respelling = respelling.map(str::trim).filter(|s| !s.is_empty());
    let phonemes = phonemes.map(str::trim).filter(|s| !s.is_empty());
    match (respelling, phonemes) {
        (Some(r), None) => Ok((Some(r.to_string()), respell(r)?)),
        (None, Some(p)) => Ok((None, validate_phonemes(p)?)),
        _ => Err(LexiconError::Invalid(
            "give exactly one of respelling or phonemes".into(),
        )),
    }
}

/// Log `word` the first time it is seen in this process.
fn log_unknown_once(key: &str) {
    static LOGGED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
    if LOGGED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string())
    {
        tracing::info!("tts: no pronunciation for '{key}' — add it in Settings → Voice");
    }
}

#[derive(Clone)]
struct Sighting {
    count: i64,
    first: String,
    last: String,
}

/// In-memory mirror of `tts_lexicon` plus the pending unknown-word tally.
pub struct LexiconStore {
    db: Db,
    entries: RwLock<HashMap<String, TtsLexiconEntry>>,
    pending: Mutex<HashMap<String, Sighting>>,
}

type StoreCell = Arc<tokio::sync::OnceCell<Arc<LexiconStore>>>;

/// One store per database (tests open many), created on first use.
static STORES: LazyLock<Mutex<HashMap<usize, StoreCell>>> = LazyLock::new(Default::default);

/// The lexicon store for `db`: seeds defaults and loads the cache on first
/// call, and starts the unknown-word flusher.
pub async fn store(db: &Db) -> anyhow::Result<Arc<LexiconStore>> {
    let cell = STORES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(db.instance_key())
        .or_default()
        .clone();
    cell.get_or_try_init(|| async {
        let store = Arc::new(LexiconStore::load(db.clone()).await?);
        let weak = Arc::downgrade(&store);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(FLUSH_EVERY).await;
                let Some(store) = weak.upgrade() else { break };
                if let Err(e) = store.flush_unknown().await {
                    tracing::warn!(error = %e, "tts: unknown-word flush failed");
                }
            }
        });
        anyhow::Ok(store)
    })
    .await
    .cloned()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl LexiconStore {
    async fn load(db: Db) -> anyhow::Result<Self> {
        let ts = now();
        let defaults = defaults()
            .into_iter()
            .map(|(display, phonemes)| TtsLexiconEntry {
                word: display.to_lowercase(),
                display,
                respelling: None,
                phonemes,
                source: SOURCE_DEFAULT.into(),
                updated_at: ts.clone(),
            })
            .collect();
        let seeded = db.seed_tts_lexicon(defaults, ts).await?;
        if seeded > 0 {
            tracing::info!(count = seeded, "tts: seeded default pronunciations");
        }
        let entries = db
            .list_tts_lexicon()
            .await?
            .into_iter()
            .map(|e| (e.word.clone(), e))
            .collect();
        Ok(Self {
            db,
            entries: RwLock::new(entries),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Every entry, sorted by display (case-insensitive).
    pub fn list(&self) -> Vec<TtsLexiconEntry> {
        let mut out: Vec<_> = self.entries.read().unwrap().values().cloned().collect();
        out.sort_by_key(|e| (e.display.to_lowercase(), e.display.clone()));
        out
    }

    /// Phonemes for `word` including `'s` / plural `s` forms.
    pub fn lookup(&self, word: &str, british: bool) -> Option<String> {
        let entries = self.entries.read().unwrap();
        let lower = word.to_lowercase().replace('’', "'");
        if let Some(e) = entries.get(&lower) {
            return Some(e.phonemes.clone());
        }
        let stem = lower
            .strip_suffix("'s")
            .or_else(|| lower.strip_suffix('s'))?;
        entries.get(stem).map(|e| append_s(&e.phonemes, british))
    }

    /// Wrap every lexicon word in `text` as `[word](/phonemes/)`.
    pub fn apply(&self, text: &str, british: bool) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for m in WORD.find_iter(text) {
            if let Some(ph) = self.lookup(m.as_str(), british) {
                out.push_str(&text[last..m.start()]);
                out.push_str(&format!("[{}](/{ph}/)", m.as_str()));
                last = m.end();
            }
        }
        out.push_str(&text[last..]);
        out
    }

    /// Save (insert or replace) a user pronunciation for `word`. The cache
    /// is updated before returning, so the next sentence uses it.
    pub async fn put(
        &self,
        word: &str,
        display: Option<&str>,
        respelling: Option<&str>,
        phonemes: Option<&str>,
    ) -> Result<TtsLexiconEntry, LexiconError> {
        let word = word.trim();
        if !KEY.is_match(word) {
            return Err(LexiconError::Invalid(format!(
                "word must be a single word of letters or digits (got '{word}')"
            )));
        }
        let display = display
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or(word);
        if display.to_lowercase() != word.to_lowercase() {
            return Err(LexiconError::Invalid(format!(
                "display '{display}' must be the same word as '{word}'"
            )));
        }
        let (respelling, phonemes) = resolve_phonemes(respelling, phonemes)?;
        let entry = TtsLexiconEntry {
            word: word.to_lowercase(),
            display: display.to_string(),
            respelling,
            phonemes,
            source: SOURCE_USER.into(),
            updated_at: now(),
        };
        self.db.upsert_tts_lexicon(entry.clone()).await?;
        self.pending.lock().unwrap().remove(&entry.word);
        self.entries
            .write()
            .unwrap()
            .insert(entry.word.clone(), entry.clone());
        Ok(entry)
    }

    /// Delete `word`'s pronunciation; true if it existed.
    pub async fn delete(&self, word: &str) -> anyhow::Result<bool> {
        let key = word.trim().to_lowercase();
        let existed = self.db.delete_tts_lexicon(key.clone()).await?;
        let cached = self.entries.write().unwrap().remove(&key).is_some();
        Ok(existed || cached)
    }

    /// Note words misaki didn't know: logged once per process, tallied in
    /// memory and written by the periodic flush.
    pub fn record_unknown(&self, words: &[String]) {
        if words.is_empty() {
            return;
        }
        let ts = now();
        let mut pending = self.pending.lock().unwrap();
        for w in words {
            let key = unknown_key(w);
            if !key.chars().any(char::is_alphabetic) || self.lookup(&key, false).is_some() {
                continue;
            }
            log_unknown_once(&key);
            pending
                .entry(key)
                .and_modify(|s| {
                    s.count += 1;
                    s.last = ts.clone();
                })
                .or_insert_with(|| Sighting {
                    count: 1,
                    first: ts.clone(),
                    last: ts.clone(),
                });
        }
    }

    /// Write buffered unknown-word sightings to the DB.
    pub async fn flush_unknown(&self) -> anyhow::Result<()> {
        let batch: Vec<(String, Sighting)> = {
            let mut pending = self.pending.lock().unwrap();
            let entries = self.entries.read().unwrap();
            pending
                .drain()
                .filter(|(w, _)| !entries.contains_key(w))
                .collect()
        };
        let rows = batch
            .into_iter()
            .map(|(w, s)| (w, s.count, s.first, s.last))
            .collect();
        self.db.record_tts_unknown(rows).await
    }

    /// Unknown words, most spoken first (pending sightings flushed first).
    pub async fn list_unknown(&self) -> anyhow::Result<Vec<TtsUnknownWord>> {
        self.flush_unknown().await?;
        self.db.list_tts_unknown().await
    }

    /// Dismiss an unknown word; true if it existed.
    pub async fn dismiss_unknown(&self, word: &str) -> anyhow::Result<bool> {
        let key = word.trim().to_lowercase();
        let pending = self.pending.lock().unwrap().remove(&key).is_some();
        Ok(self.db.delete_tts_unknown(key).await? || pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tts::kokoro::phonemize_with;

    async fn fresh() -> (Db, Arc<LexiconStore>) {
        let db = Db::in_memory().unwrap();
        let s = store(&db).await.unwrap();
        (db, s)
    }

    fn ph(s: &LexiconStore, text: &str) -> String {
        phonemize_with(&LexG2p::new(false), Some(s), text, false).unwrap()
    }

    #[tokio::test]
    async fn seed_words_use_lexicon_phonemes() {
        let (_db, s) = fresh().await;
        assert_eq!(ph(&s, "Peckboard").trim(), "pˈɛkbˌɔːɹd");
        assert_eq!(ph(&s, "Kokoro").trim(), "kOkˈOɹO");
        assert_eq!(ph(&s, "peckBOARD").trim(), "pˈɛkbˌɔːɹd");
        assert_eq!(ph(&s, "API").trim(), "ˌApˌiˈI");
        let list = s.list();
        let pb = list.iter().find(|e| e.word == "peckboard").unwrap();
        assert_eq!(
            (pb.display.as_str(), pb.source.as_str()),
            ("Peckboard", "default")
        );
    }

    #[tokio::test]
    async fn possessives_plurals_and_prosody() {
        let (_db, s) = fresh().await;
        let base = "pˈɛkbˌɔːɹd";
        assert_eq!(ph(&s, "Peckboard's").trim(), format!("{base}z"));
        assert_eq!(ph(&s, "Peckboards").trim(), format!("{base}z"));
        assert_eq!(s.lookup("SSH's", false).unwrap(), "ˌɛsˌɛsˈAʧᵻz");
        assert_eq!(s.lookup("grok’s", false).unwrap(), "ɡɹˈɑks");
        assert_eq!(s.lookup("Peckboardish", false), None);
        let out = ph(&s, "Welcome to Peckboard, powered by Kokoro.");
        assert!(out.contains(&format!("{base} ,")), "{out}");
        assert!(out.trim_end().ends_with("kOkˈOɹO ."), "{out}");
        assert!(!out.contains("  "), "{out}");
    }

    #[test]
    fn every_default_is_in_the_kokoro_vocab() {
        for (word, p) in defaults() {
            assert_eq!(validate_phonemes(&p).unwrap(), p, "{word}");
            assert!(p.contains('ˈ'), "{word}: no primary stress");
        }
    }

    #[test]
    fn respelling_to_phonemes() {
        assert_eq!(respell("PECK-board").unwrap(), "pˈɛkbˌɔːɹd");
        assert_eq!(respell("koh-KOH-roh").unwrap(), "kOkˈOɹO");
        // `fy` is a dictionary word to misaki: secondary stress.
        assert_eq!(respell("stash-ih-fy").unwrap(), "stˈæʃɪfˌI");
        // A syllable misaki doesn't know is sounded out, unstressed.
        assert_eq!(respell("ZORB-lih").unwrap(), "zˈɔɹblɪ");
        assert_eq!(
            respell("T C G Trade G G").unwrap(),
            "tˈi sˈi ʤˈi tɹˈAd ʤˈi ʤˈi"
        );
        assert!(matches!(respell("R2-D2"), Err(LexiconError::Invalid(_))));
        assert!(matches!(
            resolve_phonemes(Some("a"), Some("b")),
            Err(LexiconError::Invalid(_))
        ));
    }

    #[test]
    fn invalid_phonemes_are_rejected() {
        let err = validate_phonemes("pˈɛk#bɔ$d").unwrap_err().to_string();
        assert!(err.contains("'#'") && err.contains("'$'"), "{err}");
        // misaki's joined pairs and ASCII g are folded, not rejected.
        assert_eq!(validate_phonemes("ge\u{200d}ɪm").unwrap(), "ɡAm");
    }

    #[tokio::test]
    async fn edits_apply_immediately_and_clear_unknowns() {
        let (db, s) = fresh().await;
        let g = LexG2p::new(false);
        let before = phonemize_with(&g, Some(&s), "Vercel", false).unwrap();
        s.flush_unknown().await.unwrap();
        let unknown = db.list_tts_unknown().await.unwrap();
        assert_eq!(unknown[0].word, "vercel");
        assert_eq!(unknown[0].count, 1);

        s.put("Vercel", None, Some("ver-SELL"), None).await.unwrap();
        let after = phonemize_with(&g, Some(&s), "Vercel", false).unwrap();
        assert_ne!(before, after);
        // misaki knows "ver" as a word, so it keeps a secondary stress.
        assert_eq!(after.trim(), "vˌɜːsˈɛl");
        assert!(db.list_tts_unknown().await.unwrap().is_empty());

        // Lexicon words never count as unknown.
        phonemize_with(&g, Some(&s), "Vercel's", false).unwrap();
        assert!(s.list_unknown().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleted_default_is_not_reseeded() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let s = LexiconStore::load(db.clone()).await.unwrap();
        assert!(s.delete("Kokoro").await.unwrap());
        s.put("Grok", Some("GROK"), None, Some("ɡɹˈOk"))
            .await
            .unwrap();
        drop(s);
        drop(db);

        let db = Db::open(dir.path()).unwrap();
        let s = LexiconStore::load(db).await.unwrap();
        assert!(s.lookup("kokoro", false).is_none());
        let grok = s.list().into_iter().find(|e| e.word == "grok").unwrap();
        assert_eq!(
            (grok.phonemes.as_str(), grok.source.as_str()),
            ("ɡɹˈOk", "user")
        );
        assert!(s.lookup("peckboard", false).is_some());
    }
}
