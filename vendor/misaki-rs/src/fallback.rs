use thiserror::Error;

// Peckboard patch: the optional espeak-ng fallback is removed; callers
// inject their own [`Fallback`] through `G2P::load_with_fallback`.

#[derive(Error, Debug)]
pub enum FallbackError {
    #[error("espeak error for '{word}': {error}")]
    Espeak {
        word: String,
        error: String,
    },
    #[error("no phonemes matched for '{word}'")]
    NoPhonemes {
        word: String,
    },
    #[error("mutex poisoned: {0}")]
    MutexPoisoned(String),
}

/// Trait for OOV (out-of-vocabulary) word fallback mechanisms
pub trait Fallback: Send + Sync {
    /// Convert unknown word to phonemes
    /// Returns phonemes
    fn phonemize(&self, word: &str) -> Result<String, FallbackError>;
}
