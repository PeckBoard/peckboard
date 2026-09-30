//! English letter-to-sound fallback for words neither the user lexicon nor
//! misaki's dictionary knows. Pure spelling heuristics (digraphs, vowel
//! teams, magic-e, common suffixes, first-syllable stress) — a plausible
//! guess beats reading a name out letter by letter. Output uses only
//! symbols in Kokoro's vocab (misaki's US alphabet).

/// Longest all-caps token spelled out letter by letter (`API`, `UI`).
const MAX_SPELLED: usize = 5;

#[derive(Clone, Copy)]
struct Vowel {
    /// Pronunciation when stressed.
    full: &'static str,
    /// Pronunciation when unstressed.
    reduced: &'static str,
    /// Long vowel that keeps a secondary stress when not primary.
    sec: bool,
}

enum Unit {
    C(&'static str),
    V(Vowel),
}

const fn v(full: &'static str, reduced: &'static str) -> Unit {
    Unit::V(Vowel {
        full,
        reduced,
        sec: false,
    })
}

const fn long(ph: &'static str) -> Unit {
    Unit::V(Vowel {
        full: ph,
        reduced: ph,
        sec: true,
    })
}

fn is_vowel(c: u8) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u')
}

fn is_soft(c: u8) -> bool {
    matches!(c, b'e' | b'i' | b'y')
}

/// Guess Kokoro phonemes for `word` from its spelling. All-caps tokens of
/// up to five letters are spelled (`API`); camelCase parts are guessed
/// separately and spoken as separate words. `None` when the word has no
/// letters or contains characters the rules don't cover (digits, non-ASCII).
pub fn guess(word: &str) -> Option<String> {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if word.is_empty()
        || !word
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == '\'' || c == '-')
    {
        return None;
    }
    let mut out = Vec::new();
    for piece in word.split('-').filter(|p| !p.is_empty()) {
        for part in camel_parts(piece) {
            let letters: String = part.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if letters.is_empty() {
                continue;
            }
            let caps = letters.chars().all(|c| c.is_ascii_uppercase());
            let ph = if caps && letters.len() <= MAX_SPELLED {
                spell_letters(&letters)
            } else {
                guess_lower(&letters.to_ascii_lowercase())
                    .unwrap_or_else(|| spell_letters(&letters))
            };
            out.push(ph);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(" "))
    }
}

/// Split `TCGTrade` → `TCG`, `Trade`; `MacMini` → `Mac`, `Mini`.
fn camel_parts(word: &str) -> Vec<&str> {
    let b = word.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    for i in 1..b.len() {
        let lower_to_upper = b[i - 1].is_ascii_lowercase() && b[i].is_ascii_uppercase();
        let acronym_end = b[i - 1].is_ascii_uppercase()
            && b[i].is_ascii_uppercase()
            && b.get(i + 1).is_some_and(|c| c.is_ascii_lowercase());
        if lower_to_upper || acronym_end {
            parts.push(&word[start..i]);
            start = i;
        }
    }
    parts.push(&word[start..]);
    parts
}

/// Letter names (onset, stressable rest), misaki style.
fn letter_name(c: char) -> Option<(&'static str, &'static str)> {
    Some(match c.to_ascii_lowercase() {
        'a' => ("", "A"),
        'b' => ("b", "i"),
        'c' => ("s", "i"),
        'd' => ("d", "i"),
        'e' => ("", "i"),
        'f' => ("", "ɛf"),
        'g' => ("ʤ", "i"),
        'h' => ("", "Aʧ"),
        'i' => ("", "I"),
        'j' => ("ʤ", "A"),
        'k' => ("k", "A"),
        'l' => ("", "ɛl"),
        'm' => ("", "ɛm"),
        'n' => ("", "ɛn"),
        'o' => ("", "O"),
        'p' => ("p", "i"),
        'q' => ("kj", "u"),
        'r' => ("", "ɑɹ"),
        's' => ("", "ɛs"),
        't' => ("t", "i"),
        'u' => ("j", "u"),
        'v' => ("v", "i"),
        'w' => ("d", "ʌbəlju"),
        'x' => ("", "ɛks"),
        'y' => ("w", "I"),
        'z' => ("z", "i"),
        _ => return None,
    })
}

/// Spell `word` letter by letter, misaki's acronym style: every letter
/// secondary-stressed, the last primary (`API` → `ˌApˌiˈI`).
pub fn spell_letters(word: &str) -> String {
    let names: Vec<_> = word.chars().filter_map(letter_name).collect();
    let mut out = String::new();
    for (i, (onset, rest)) in names.iter().enumerate() {
        out.push_str(onset);
        out.push(if i + 1 == names.len() { 'ˈ' } else { 'ˌ' });
        out.push_str(rest);
    }
    out
}

/// Letter-to-sound for one lowercase ASCII word. `None` when no vowel
/// sound comes out (`npm`) — the caller spells it instead.
fn guess_lower(w: &str) -> Option<String> {
    let units = tokenize(w.as_bytes());
    let nv = units.iter().filter(|u| matches!(u, Unit::V(_))).count();
    if nv == 0 {
        return None;
    }
    let primary = stress_index(w, nv);
    let mut out = String::new();
    let mut vi = 0;
    for u in &units {
        match u {
            Unit::C(c) => out.push_str(c),
            Unit::V(v) => {
                if vi == primary {
                    out.push('ˈ');
                    out.push_str(v.full);
                } else if v.sec {
                    out.push('ˌ');
                    out.push_str(v.full);
                } else {
                    out.push_str(v.reduced);
                }
                vi += 1;
            }
        }
    }
    Some(out)
}

/// Primary-stress syllable: set by a few stress-fixing suffixes, else the
/// first syllable.
fn stress_index(w: &str, nv: usize) -> usize {
    const BEFORE_LAST: &[&str] = &[
        "tion", "tions", "sion", "sions", "cian", "cial", "tial", "cious", "tious", "ic", "ics",
    ];
    const BEFORE_TWO: &[&str] = &["ity", "ify", "ical"];
    let back = if BEFORE_LAST.iter().any(|s| w.ends_with(s)) {
        2
    } else if BEFORE_TWO.iter().any(|s| w.ends_with(s)) {
        3
    } else {
        return 0;
    };
    nv.saturating_sub(back)
}

/// Is `w[i]` a vowel whose `C e` continuation makes it long (magic e)?
/// Returns the index of the silent `e`.
fn magic_e(w: &[u8], i: usize) -> Option<usize> {
    let c = *w.get(i + 1)?;
    if is_vowel(c) || matches!(c, b'r' | b'w' | b'x' | b'y') {
        return None;
    }
    let e = i + 2;
    if w.get(e) != Some(&b'e') {
        return None;
    }
    let after = &w[e + 1..];
    let ends =
        after.is_empty() || matches!(after, b"s" | b"d" | b"ly" | b"ful" | b"ness" | b"ment");
    // Compound: `trade|line` — a consonant after the `e`, then another
    // real vowel (not just a final `e`).
    let compound = after
        .first()
        .is_some_and(|&n| !is_vowel(n) && n != b'r' && n != b'y')
        && after[..after.len() - usize::from(after.last() == Some(&b'e'))]
            .iter()
            .any(|&c| is_vowel(c) || c == b'y');
    (ends || compound).then_some(e)
}

fn tokenize(w: &[u8]) -> Vec<Unit> {
    let n = w.len();
    let at = |j: usize| w.get(j).copied().unwrap_or(0);
    let from = |j: usize, s: &str| w[j..].starts_with(s.as_bytes());
    let mut silent = vec![false; n];
    let mut units: Vec<Unit> = Vec::new();
    let mut i = 0;
    while i < n {
        if silent[i] {
            i += 1;
            continue;
        }
        let c = w[i];
        let prev = if i > 0 { w[i - 1] } else { 0 };
        let has_vowel = units.iter().any(|u| matches!(u, Unit::V(_)));

        // Suffix-like sequences.
        let seq: Option<(usize, Vec<Unit>)> = if from(i, "tion") {
            Some((4, vec![Unit::C("ʃ"), v("ə", "ə"), Unit::C("n")]))
        } else if from(i, "sion") {
            let sh = if is_vowel(prev) { "ʒ" } else { "ʃ" };
            Some((4, vec![Unit::C(sh), v("ə", "ə"), Unit::C("n")]))
        } else if from(i, "cian") || from(i, "tian") {
            Some((4, vec![Unit::C("ʃ"), v("ə", "ə"), Unit::C("n")]))
        } else if from(i, "cial") || from(i, "tial") {
            Some((4, vec![Unit::C("ʃ"), v("ə", "ə"), Unit::C("l")]))
        } else if from(i, "cious") || from(i, "tious") {
            Some((5, vec![Unit::C("ʃ"), v("ə", "ə"), Unit::C("s")]))
        } else if from(i, "ture") {
            Some((4, vec![Unit::C("ʧ"), v("ɚ", "ɚ")]))
        } else if i + 3 == n && from(i, "ous") {
            Some((3, vec![v("ə", "ə"), Unit::C("s")]))
        } else if i + 2 == n && i > 0 && from(i, "le") && !is_vowel(prev) {
            Some((2, vec![Unit::C("ᵊl")]))
        } else {
            None
        };
        if let Some((len, us)) = seq {
            units.extend(us);
            i += len;
            continue;
        }

        if is_vowel(c) || (c == b'y' && i > 0 && !is_vowel(at(i + 1))) {
            let (len, u): (usize, Vec<Unit>) = vowel(w, i, has_vowel, &mut silent);
            units.extend(u);
            i += len;
            continue;
        }

        // Consonants.
        let (len, ph): (usize, Vec<&'static str>) = if from(i, "tch") {
            (3, vec!["ʧ"])
        } else if from(i, "sch") {
            (3, vec!["s", "k"])
        } else if from(i, "chr") {
            (2, vec!["k"])
        } else if from(i, "ch") {
            (2, vec!["ʧ"])
        } else if from(i, "sh") {
            (2, vec!["ʃ"])
        } else if from(i, "th") {
            (2, vec!["θ"])
        } else if from(i, "ph") {
            (2, vec!["f"])
        } else if from(i, "wh") {
            (2, vec!["w"])
        } else if from(i, "ck") {
            (2, vec!["k"])
        } else if from(i, "ng") {
            if matches!(at(i + 2), b'e' | b'y') {
                (2, vec!["n", "ʤ"])
            } else {
                (2, vec!["ŋ"])
            }
        } else if from(i, "nk") {
            (2, vec!["ŋ", "k"])
        } else if from(i, "gh") {
            (2, if i == 0 { vec!["ɡ"] } else { vec![] })
        } else if from(i, "qu") {
            (2, vec!["k", "w"])
        } else if from(i, "dg") && is_soft(at(i + 2)) {
            (2, vec!["ʤ"])
        } else if i == 0 && (from(i, "kn") || from(i, "gn") || from(i, "pn")) {
            (2, vec!["n"])
        } else if i == 0 && from(i, "wr") {
            (2, vec!["ɹ"])
        } else if i == 0 && from(i, "ps") {
            (2, vec!["s"])
        } else if from(i, "gu") && matches!(at(i + 2), b'e' | b'i') {
            (2, vec!["ɡ"])
        } else if c == b'c' && at(i + 1) == b'c' && is_soft(at(i + 2)) {
            (2, vec!["k", "s"])
        } else if at(i + 1) == c {
            // Doubled consonant: one sound.
            (2, vec![single(c, w, i + 1, has_vowel)])
        } else {
            (1, vec![single(c, w, i, has_vowel)])
        };
        units.extend(ph.into_iter().filter(|p| !p.is_empty()).map(Unit::C));
        i += len;
    }
    units
}

/// One consonant letter at `i` (context-sensitive c/g/s/y/h/x).
fn single(c: u8, w: &[u8], i: usize, has_vowel: bool) -> &'static str {
    let next = w.get(i + 1).copied().unwrap_or(0);
    let last = i + 1 == w.len();
    match c {
        b'b' => "b",
        b'c' => {
            if is_soft(next) {
                "s"
            } else {
                "k"
            }
        }
        b'd' => "d",
        b'f' => "f",
        b'g' => {
            if is_soft(next) && i > 0 {
                "ʤ"
            } else {
                "ɡ"
            }
        }
        b'h' => {
            if last {
                ""
            } else {
                "h"
            }
        }
        b'j' => "ʤ",
        b'k' => "k",
        b'l' => "l",
        b'm' => "m",
        b'n' => "n",
        b'p' => "p",
        b'q' => "k",
        b'r' => "ɹ",
        b's' => {
            let prev = if i > 0 { w[i - 1] } else { 0 };
            if last
                && has_vowel
                && matches!(prev, b'b' | b'd' | b'g' | b'l' | b'm' | b'n' | b'v' | b'e')
            {
                "z"
            } else {
                "s"
            }
        }
        b't' => "t",
        b'v' => "v",
        b'w' => "w",
        b'x' => {
            if i == 0 {
                "z"
            } else {
                "ks"
            }
        }
        b'y' => "j",
        b'z' => "z",
        _ => "",
    }
}

/// Vowel letter(s) at `i` → (letters consumed, units). May mark a later
/// magic `e` silent.
fn vowel(w: &[u8], i: usize, has_vowel: bool, silent: &mut [bool]) -> (usize, Vec<Unit>) {
    let n = w.len();
    let at = |j: usize| w.get(j).copied().unwrap_or(0);
    let from = |s: &str| w[i..].starts_with(s.as_bytes());
    let end_after =
        |len: usize| i + len == n || (i + len + 1 == n && matches!(at(i + len), b's' | b'd'));
    let c = w[i];

    // Four/three-letter teams.
    if from("ough") {
        return if at(i + 4) == b't' {
            (4, vec![long("ɔ")])
        } else if i + 4 == n {
            (4, vec![long("O")])
        } else {
            (4, vec![long("ʌ"), Unit::C("f")])
        };
    }
    if from("augh") {
        return (4, vec![long("ɔ")]);
    }
    if from("eigh") {
        return (4, vec![long("A")]);
    }
    if from("igh") {
        return (3, vec![long("I")]);
    }
    if from("eau") {
        return (3, vec![long("O")]);
    }
    // R-coloured vowels.
    for (s, ph) in [
        ("ear", "ɪɹ"),
        ("eer", "ɪɹ"),
        ("air", "ɛɹ"),
        ("oar", "ɔɹ"),
        ("oor", "ɔɹ"),
        ("our", "ɔɹ"),
    ] {
        if from(s) && !is_vowel(at(i + 3)) {
            return (3, vec![long(ph)]);
        }
    }
    for (s, ph) in [
        ("are", "ɛɹ"),
        ("ire", "Iɚ"),
        ("ore", "ɔɹ"),
        ("ure", "jʊɹ"),
        ("ere", "ɪɹ"),
    ] {
        if from(s) && end_after(3) {
            return (3, vec![long(ph)]);
        }
    }
    // Vowel teams.
    let y_ends = !is_vowel(at(i + 2));
    let team: Option<Unit> = match (c, at(i + 1)) {
        (b'a', b'i') => Some(long("A")),
        (b'a', b'y') if y_ends => Some(long("A")),
        (b'e', b'e') | (b'e', b'a') => Some(long("i")),
        (b'i', b'e') if i + 2 == n => Some(if n <= 3 { long("I") } else { v("i", "i") }),
        (b'e', b'i') => Some(long("A")),
        (b'e', b'y') if y_ends => Some(if i + 2 == n { v("i", "i") } else { long("A") }),
        (b'o', b'a') => Some(long("O")),
        (b'o', b'o') => Some(if at(i + 2) == b'k' {
            v("ʊ", "ʊ")
        } else {
            long("u")
        }),
        (b'o', b'u') => Some(long("W")),
        (b'o', b'w') => Some(if i + 2 == n { long("O") } else { long("W") }),
        (b'o', b'i') => Some(long("Y")),
        (b'o', b'y') if y_ends => Some(long("Y")),
        (b'a', b'u') | (b'a', b'w') => Some(long("ɔ")),
        (b'e', b'w') | (b'u', b'i') | (b'e', b'u') => Some(long("u")),
        (b'u', b'e') if i + 2 == n => Some(long("u")),
        _ => None,
    };
    if let Some(u) = team {
        return (2, vec![u]);
    }
    // R-controlled single vowels (`ar`, `er`, …) when the `r` closes the
    // syllable.
    if at(i + 1) == b'r' && !is_vowel(at(i + 2)) && at(i + 2) != b'r' && at(i + 2) != b'y' {
        let u = match c {
            b'a' => v("ɑɹ", "ɚ"),
            b'o' => v("ɔɹ", "ɚ"),
            _ => v("ɜɹ", "ɚ"),
        };
        return (2, vec![u]);
    }

    let last = i + 1 == n;
    let prev = if i > 0 { w[i - 1] } else { 0 };
    // Final / inflectional `e`.
    if c == b'e' {
        if last && has_vowel {
            return (1, vec![]);
        }
        if i + 2 == n && has_vowel && at(i + 1) == b'd' {
            return if matches!(prev, b't' | b'd') {
                (1, vec![v("ɪ", "ɪ")])
            } else {
                (1, vec![])
            };
        }
        if i + 2 == n && has_vowel && at(i + 1) == b's' {
            return if matches!(prev, b's' | b'x' | b'z' | b'h' | b'c' | b'g') {
                (1, vec![v("ɪ", "ɪ")])
            } else {
                (1, vec![])
            };
        }
    }
    if c == b'y' {
        // `-ify`, `-fy`, `-ply`: long I; other final y: i; medial: ɪ.
        return if last && (prev == b'f' || w[..i].ends_with(b"pl")) {
            (1, vec![long("I")])
        } else if last {
            (1, vec![if has_vowel { v("i", "i") } else { long("I") }])
        } else if let Some(e) = magic_e(w, i) {
            silent[e] = true;
            (1, vec![long("I")])
        } else {
            (1, vec![v("ɪ", "ɪ")])
        };
    }
    // The vowel before `-tion` / `-sion` is long (`nation`, `motion`).
    if w[i + 1..].starts_with(b"tion") || w[i + 1..].starts_with(b"sion") {
        let ph = match c {
            b'a' => "A",
            b'e' => "i",
            b'o' => "O",
            b'u' => "u",
            _ => "ɪ",
        };
        return (1, vec![v(ph, ph)]);
    }
    if let Some(e) = magic_e(w, i) {
        silent[e] = true;
        let ph = match c {
            b'a' => "A",
            b'e' => "i",
            b'i' => "I",
            b'o' => "O",
            _ => {
                if matches!(prev, b'b' | b'c' | b'f' | b'h' | b'k' | b'm' | b'p' | b'v') {
                    "ju"
                } else {
                    "u"
                }
            }
        };
        return (1, vec![long(ph)]);
    }
    if last {
        return (
            1,
            vec![match c {
                b'a' => v("ɑ", "ə"),
                b'e' | b'i' => v("i", "i"),
                b'o' => v("O", "O"),
                _ => v("u", "u"),
            }],
        );
    }
    // Open syllable `o` (`Ko-ko-ro`): long.
    let next = at(i + 1);
    if c == b'o'
        && !is_vowel(next)
        && next != 0
        && is_vowel(at(i + 2))
        && !matches!(next, b'w' | b'x' | b'y')
    {
        return (1, vec![v("O", "O")]);
    }
    // `i` before another vowel (`piano`, `Ollama`'s `ia`): i.
    if c == b'i' && is_vowel(next) {
        return (1, vec![v("i", "i")]);
    }
    let e_reduced = if matches!(next, b't' | b's' | b'd' | b'k') {
        "ɪ"
    } else {
        "ə"
    };
    let u = match c {
        b'a' => v("æ", "ə"),
        b'e' => v("ɛ", e_reduced),
        b'i' => v("ɪ", "ɪ"),
        b'o' => v("ɑ", "ə"),
        _ => v("ʌ", "ə"),
    };
    (1, vec![u])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tts::kokoro::unknown_symbols;

    fn g(w: &str) -> String {
        guess(w).unwrap_or_else(|| panic!("no guess for {w}"))
    }

    #[test]
    fn plausible_guesses() {
        assert_eq!(g("Stashify"), "stˈæʃɪfˌI");
        assert_eq!(g("Grokking"), "ɡɹˈɑkɪŋ");
        assert_eq!(g("quokka"), "kwˈɑkə");
        assert_eq!(g("tradeline"), "tɹˈAdlˌIn");
        assert_eq!(g("Peckboard"), "pˈɛkbˌɔɹd");
        assert_eq!(g("Kokoro"), "kˈOkOɹO");
        assert_eq!(g("information"), "ɪnfɚmˈAʃən");
    }

    #[test]
    fn short_all_caps_are_spelled_and_camel_case_split() {
        assert_eq!(spell_letters("API"), "ˌApˌiˈI");
        assert_eq!(g("UI"), "jˌuˈI");
        assert_eq!(g("TCGTrade"), "tˌisˌiʤˈi tɹˈAd");
        // Longer all-caps words are read as words, not spelled.
        assert!(!g("KUBERNETES").contains(' '));
        assert_eq!(guess("npm"), Some(spell_letters("npm")));
        assert_eq!(guess("v2"), None);
        assert_eq!(guess(""), None);
    }

    #[test]
    fn every_output_symbol_is_in_the_kokoro_vocab() {
        let words = [
            "Stashify",
            "Grokking",
            "quokka",
            "tradeline",
            "though",
            "thought",
            "rough",
            "night",
            "weigh",
            "beautiful",
            "station",
            "vision",
            "picture",
            "famous",
            "table",
            "phone",
            "knight",
            "wrench",
            "psyche",
            "judge",
            "change",
            "finger",
            "thanks",
            "school",
            "christmas",
            "guess",
            "success",
            "boxes",
            "wanted",
            "played",
            "cube",
            "type",
            "system",
            "career",
            "chair",
            "board",
            "floor",
            "our",
            "care",
            "fire",
            "more",
            "pure",
            "here",
            "star",
            "border",
            "bird",
            "nurse",
            "rain",
            "day",
            "tree",
            "sea",
            "pie",
            "cookie",
            "vein",
            "key",
            "they",
            "boat",
            "book",
            "moon",
            "house",
            "cow",
            "snow",
            "coin",
            "toy",
            "sauce",
            "law",
            "new",
            "fruit",
            "feud",
            "blue",
            "piano",
            "xylophone",
            "quiz",
            "rhythm",
            "Zustand",
            "Ollama",
            "WASM",
            "SSH",
            "abcdefghijklmnopqrstuvwxyz",
        ];
        for w in words {
            let ph = g(w);
            assert!(
                unknown_symbols(&ph).is_empty(),
                "{w} → {ph}: {:?}",
                unknown_symbols(&ph)
            );
        }
        let all = spell_letters("abcdefghijklmnopqrstuvwxyz");
        assert!(unknown_symbols(&all).is_empty(), "{all}");
    }
}
