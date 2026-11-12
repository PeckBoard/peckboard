//! Runtime loading of the lexicons and POS-tagger data.
//!
//! Peckboard patch: upstream `include_str!`s these files (~37 MB). Here they
//! are read from a directory the caller supplies, holding the files under
//! the names below (byte-identical to the published crate's `data/*.json`
//! and `src/resources/tagger/*`). Every failure is an error, never a panic.

use crate::language::Language;
use crate::lexicon::PhonemeEntry;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const US_GOLD: &str = "us_gold.json";
pub const US_SILVER: &str = "us_silver.json";
pub const GB_GOLD: &str = "gb_gold.json";
pub const GB_SILVER: &str = "gb_silver.json";
pub const TAGGER_WEIGHTS: &str = "weights.json";
pub const TAGGER_CLASSES: &str = "classes.txt";
pub const TAGGER_TAGS: &str = "tags.json";

/// Every data file, for every language.
pub const ALL_FILES: [&str; 7] = [
    US_GOLD,
    US_SILVER,
    GB_GOLD,
    GB_SILVER,
    TAGGER_WEIGHTS,
    TAGGER_CLASSES,
    TAGGER_TAGS,
];

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// The (gold, silver) lexicon file names for `lang`.
pub fn lexicon_files(lang: Language) -> [&'static str; 2] {
    match lang {
        Language::EnglishUS => [US_GOLD, US_SILVER],
        Language::EnglishGB => [GB_GOLD, GB_SILVER],
    }
}

/// The files [`crate::G2P::load`] reads for `lang`.
pub fn required_files(lang: Language) -> [&'static str; 5] {
    let [gold, silver] = lexicon_files(lang);
    [gold, silver, TAGGER_WEIGHTS, TAGGER_CLASSES, TAGGER_TAGS]
}

pub fn read(dir: &Path, name: &str) -> Result<String, DataError> {
    let path = dir.join(name);
    std::fs::read_to_string(&path).map_err(|source| DataError::Io { path, source })
}

pub fn load_json<T: DeserializeOwned>(dir: &Path, name: &str) -> Result<T, DataError> {
    let text = read(dir, name)?;
    serde_json::from_str(&text).map_err(|source| DataError::Parse {
        path: dir.join(name),
        source,
    })
}

pub type Table = HashMap<String, PhonemeEntry>;

/// The (gold, silver) lexicons for `lang` from `dir`.
pub fn load_lexicon(dir: &Path, lang: Language) -> Result<(Table, Table), DataError> {
    let [gold, silver] = lexicon_files(lang);
    Ok((load_json(dir, gold)?, load_json(dir, silver)?))
}
