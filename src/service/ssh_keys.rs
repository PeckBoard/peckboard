//! Encryption + key-handling for the SSH key vault (`db::crud::ssh_keys`).
//!
//! Unlike `service::env_vars`, the AES-256-GCM key comes straight from a
//! server-held vault key (see [`load_or_create_vault_key`]) rather than a
//! user password + Argon2id derivation: SSH keys are used headlessly by
//! agents, workers, and repeating tasks, so a password-bound scheme would
//! stall unattended work waiting on an unlock prompt every session. The
//! vault key's on-disk lifecycle mirrors `auth::token::load_or_create_jwt_secret`
//! (generate on first use, persist in the data dir, `0600`, regenerate if
//! truncated).

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rand::RngCore;
use rand::rngs::OsRng;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, decode_secret_key};

use crate::db::Db;

const VAULT_KEY_LEN: usize = 32;

/// Load the vault key from disk, or generate + persist one on first run.
/// Stored at `<data_dir>/ssh_vault_key` with `0600` permissions on Unix.
/// Regenerated (with a warning) if found truncated, so a partially written
/// key can't silently brick every stored SSH key.
pub fn load_or_create_vault_key(data_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let path = data_dir.join("ssh_vault_key");

    if path.exists() {
        let key = std::fs::read(&path)?;
        if key.len() == VAULT_KEY_LEN {
            return Ok(key);
        }
        tracing::warn!(
            "SSH vault key at {} has unexpected length {}, regenerating",
            path.display(),
            key.len(),
        );
    }

    std::fs::create_dir_all(data_dir)?;
    let mut key = vec![0u8; VAULT_KEY_LEN];
    OsRng.fill_bytes(&mut key);
    std::fs::write(&path, &key)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    tracing::info!("Generated new SSH vault key at {}", path.display());
    Ok(key)
}

/// Encrypt `plaintext` under the vault key. A fresh random nonce is drawn
/// from the OS RNG on every call. Returns `(ciphertext_b64, nonce_hex)`.
pub fn encrypt(vault_key: &[u8], plaintext: &str) -> anyhow::Result<(String, String)> {
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(vault_key)
        .map_err(|_| anyhow::anyhow!("invalid vault key length"))?;
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;

    Ok((B64.encode(ciphertext), hex::encode(nonce_bytes)))
}

/// Decrypt a value sealed by [`encrypt`]. Fails on malformed hex/base64, a
/// wrong-length nonce, or a GCM tag mismatch (corrupt ciphertext or the
/// vault key changed out from under it).
pub fn decrypt(vault_key: &[u8], nonce_hex: &str, ciphertext_b64: &str) -> anyhow::Result<String> {
    let nonce_bytes = hex::decode(nonce_hex)?;
    if nonce_bytes.len() != 12 {
        anyhow::bail!("invalid nonce length");
    }
    let ciphertext = B64.decode(ciphertext_b64)?;

    let cipher = Aes256Gcm::new_from_slice(vault_key)
        .map_err(|_| anyhow::anyhow!("invalid vault key length"))?;
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let plaintext = cipher
        .decrypt(&nonce, ciphertext.as_ref())
        .map_err(|_| anyhow::anyhow!("decryption failed"))?;
    String::from_utf8(plaintext).map_err(Into::into)
}

/// Parsed public-facing metadata for an SSH key — everything that's safe
/// to store/return unencrypted.
pub struct ParsedKey {
    pub key_type: String,
    pub public_key: String,
    pub fingerprint: String,
}

/// Map an algorithm's canonical SSH wire name (e.g. `ssh-ed25519`,
/// `rsa-sha2-256`, `ecdsa-sha2-nistp256`) down to the short family name
/// the vault stores in `key_type`.
fn key_type_name(algorithm: &Algorithm) -> String {
    let name = algorithm.to_string();
    if name.contains("ed25519") {
        "ed25519".to_string()
    } else if name.contains("rsa") {
        "rsa".to_string()
    } else if name.contains("ecdsa") {
        "ecdsa".to_string()
    } else if name.contains("dss") || name.contains("dsa") {
        "dsa".to_string()
    } else {
        name
    }
}

fn parse(key: &PrivateKey) -> anyhow::Result<ParsedKey> {
    let public = key.public_key();
    Ok(ParsedKey {
        key_type: key_type_name(&key.algorithm()),
        public_key: public
            .to_openssh()
            .map_err(|e| anyhow::anyhow!("failed to encode public key: {e}"))?,
        fingerprint: public.fingerprint(HashAlg::Sha256).to_string(),
    })
}

/// Parse + validate a pasted private key PEM before it's ever stored. The
/// key must actually decode (and, if passphrase-protected, decrypt) or the
/// import is rejected.
pub fn import_private_key(pem: &str, passphrase: Option<&str>) -> anyhow::Result<ParsedKey> {
    let key = decode_secret_key(pem, passphrase)
        .map_err(|e| anyhow::anyhow!("invalid private key: {e}"))?;
    parse(&key)
}

/// Generate a fresh keypair, returning its private key PEM and parsed
/// public metadata. Only `ed25519` is supported today (the card asks for
/// "at minimum ed25519"); other types are rejected rather than silently
/// substituted.
pub fn generate_keypair(key_type: &str) -> anyhow::Result<(String, ParsedKey)> {
    if key_type != "ed25519" {
        anyhow::bail!("unsupported key type for generation: {key_type} (only ed25519 today)");
    }
    // ssh-key 0.7's `PrivateKey::random` needs a `rand_core` 0.10 `CryptoRng`,
    // which `getrandom::SysRng` (fallible) doesn't satisfy directly --
    // `UnwrapErr` upgrades it to the infallible `Rng`/`CryptoRng` traits.
    let mut rng = rand_core::UnwrapErr(getrandom::SysRng);
    let key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|e| anyhow::anyhow!("key generation failed: {e}"))?;
    let parsed = parse(&key)?;
    let pem = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| anyhow::anyhow!("failed to encode private key: {e}"))?
        .to_string();
    Ok((pem, parsed))
}

/// Decrypt a stored key's private material for use. The next card's SSH
/// host functions call this to resolve connection credentials by key id
/// without a plugin ever seeing the material pass through it. Kept
/// `pub(crate)` since only core call sites should ever touch raw key
/// material — see `SecretMasker` at the caller for console-output safety.
#[allow(dead_code)] // wired up by the SSH host-functions card, not this one
pub(crate) async fn resolve_for_use(
    db: &Db,
    vault_key: &[u8],
    key_id: &str,
) -> anyhow::Result<(String, Option<String>)> {
    let row = db
        .get_ssh_key(key_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("SSH key not found"))?;
    let pem = decrypt(
        vault_key,
        &row.private_key_nonce,
        &row.private_key_ciphertext,
    )?;
    let passphrase = match (&row.passphrase_ciphertext, &row.passphrase_nonce) {
        (Some(ct), Some(nonce)) => Some(decrypt(vault_key, nonce, ct)?),
        _ => None,
    };
    Ok((pem, passphrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Vec<u8> {
        vec![7u8; VAULT_KEY_LEN]
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let k = key();
        let (ct, nonce) = encrypt(&k, "ssh-private-key-material").unwrap();
        let pt = decrypt(&k, &nonce, &ct).unwrap();
        assert_eq!(pt, "ssh-private-key-material");
    }

    #[test]
    fn wrong_key_fails_decrypt() {
        let (ct, nonce) = encrypt(&key(), "secret").unwrap();
        let wrong = vec![9u8; VAULT_KEY_LEN];
        assert!(decrypt(&wrong, &nonce, &ct).is_err());
    }

    #[test]
    fn generated_ed25519_key_has_stable_fingerprint() {
        let (pem, parsed) = generate_keypair("ed25519").unwrap();
        assert_eq!(parsed.key_type, "ed25519");
        assert!(parsed.fingerprint.starts_with("SHA256:"));

        // Re-parsing the generated PEM must reproduce the same fingerprint.
        let reparsed = import_private_key(&pem, None).unwrap();
        assert_eq!(reparsed.fingerprint, parsed.fingerprint);
    }

    #[test]
    fn unsupported_key_type_rejected_on_generate() {
        assert!(generate_keypair("rsa").is_err());
    }

    #[test]
    fn malformed_pem_rejected_on_import() {
        assert!(import_private_key("not a real key", None).is_err());
    }
}
