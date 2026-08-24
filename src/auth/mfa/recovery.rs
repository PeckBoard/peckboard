//! One-time recovery codes. Shown once at enroll; stored as Argon2 hashes.

use rand::Rng;

use crate::auth::password::{hash_password, verify_password};

use super::MfaError;

pub const KIND: &str = "recovery";
const COUNT: usize = 10;
const CHARS: usize = 8;
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Normalize user input: strip hyphens/spaces, uppercase.
pub fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn looks_like_recovery(normalized: &str) -> bool {
    normalized.len() == CHARS && normalized.bytes().all(|b| ALPHABET.contains(&b))
}

/// `xxxx-xxxx` display form.
pub fn format_display(normalized: &str) -> String {
    if normalized.len() == CHARS {
        format!("{}-{}", &normalized[..4], &normalized[4..])
    } else {
        normalized.to_string()
    }
}

pub fn generate_plain() -> Vec<String> {
    let mut rng = rand::thread_rng();
    let mut out = Vec::with_capacity(COUNT);
    for _ in 0..COUNT {
        let mut raw = String::with_capacity(CHARS);
        for _ in 0..CHARS {
            let i = rng.gen_range(0..ALPHABET.len());
            raw.push(ALPHABET[i] as char);
        }
        out.push(format_display(&raw));
    }
    out
}

pub fn hash_plain(display: &str) -> Result<String, MfaError> {
    hash_password(&normalize(display)).map_err(MfaError::Internal)
}

/// First unused hash that matches `code`. Returns the row id conceptually
/// via index into `hashes` (id, hash) pairs.
pub fn find_match<'a>(code: &str, unused: &'a [(String, String)]) -> Result<&'a str, MfaError> {
    let norm = normalize(code);
    if !looks_like_recovery(&norm) {
        return Err(MfaError::InvalidCode);
    }
    for (id, hash) in unused {
        if verify_password(&norm, hash) {
            return Ok(id);
        }
    }
    Err(MfaError::InvalidCode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_hashes_and_match_once() {
        let plains = generate_plain();
        assert_eq!(plains.len(), COUNT);
        let hashed: Vec<(String, String)> = plains
            .iter()
            .enumerate()
            .map(|(i, p)| (i.to_string(), hash_plain(p).unwrap()))
            .collect();
        let id = find_match(&plains[0], &hashed).unwrap();
        assert_eq!(id, "0");
        // Same code still *would* match until the caller marks it used;
        // that persistence is the DB layer's job.
        assert!(find_match("AAAA-AAAA", &hashed).is_err());
    }

    #[test]
    fn normalize_strips_hyphen_and_case() {
        assert_eq!(normalize("ab-cd ef12"), "ABCDEF12");
    }
}
