//! AES-256-GCM vault for TOTP shared secrets.
//!
//! The key is server-held at `<data_dir>/mfa_vault_key`, same lifecycle as
//! `auth::token::load_or_create_jwt_secret` and `service::ssh_keys`:
//! generate on first use, persist `0600`, regenerate if truncated. TOTP
//! verification happens on the MFA challenge hop, after the password has
//! left memory, so a password-derived key cannot work here.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rand::RngCore;
use rand::rngs::OsRng;

const VAULT_KEY_LEN: usize = 32;

/// Load the MFA vault key from disk, or generate + persist one on first run.
pub fn load_or_create_vault_key(data_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let path = data_dir.join("mfa_vault_key");

    if path.exists() {
        let key = std::fs::read(&path)?;
        if key.len() == VAULT_KEY_LEN {
            tighten(&path)?;
            return Ok(key);
        }
        tracing::warn!(
            "MFA vault key at {} has unexpected length {}, regenerating",
            path.display(),
            key.len(),
        );
    }

    std::fs::create_dir_all(data_dir)?;
    let mut key = vec![0u8; VAULT_KEY_LEN];
    OsRng.fill_bytes(&mut key);
    write_private(&path, &key)?;

    tracing::info!("Generated new MFA vault key at {}", path.display());
    Ok(key)
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn tighten(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms).map_err(|e| {
            anyhow::anyhow!(
                "failed to set 0600 on MFA vault key {}: {e}",
                path.display()
            )
        })?;
    }
    let _ = path;
    Ok(())
}

pub fn encrypt(vault_key: &[u8], plaintext: &str) -> anyhow::Result<(String, String)> {
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(vault_key)
        .map_err(|_| anyhow::anyhow!("invalid MFA vault key length"))?;
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| anyhow::anyhow!("MFA secret encryption failed"))?;

    Ok((B64.encode(ciphertext), hex::encode(nonce_bytes)))
}

pub fn decrypt(vault_key: &[u8], nonce_hex: &str, ciphertext_b64: &str) -> anyhow::Result<String> {
    let nonce_bytes = hex::decode(nonce_hex)?;
    if nonce_bytes.len() != 12 {
        anyhow::bail!("invalid nonce length");
    }
    let ciphertext = B64.decode(ciphertext_b64)?;

    let cipher = Aes256Gcm::new_from_slice(vault_key)
        .map_err(|_| anyhow::anyhow!("invalid MFA vault key length"))?;
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let plaintext = cipher
        .decrypt(&nonce, ciphertext.as_ref())
        .map_err(|_| anyhow::anyhow!("MFA secret decryption failed"))?;
    String::from_utf8(plaintext).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let mut key = vec![0u8; VAULT_KEY_LEN];
        OsRng.fill_bytes(&mut key);
        let (ct, nonce) = encrypt(&key, "JBSWY3DPEHPK3PXP").unwrap();
        assert_eq!(decrypt(&key, &nonce, &ct).unwrap(), "JBSWY3DPEHPK3PXP");
    }

    #[test]
    fn wrong_key_fails_decrypt() {
        let mut key = vec![0u8; VAULT_KEY_LEN];
        OsRng.fill_bytes(&mut key);
        let (ct, nonce) = encrypt(&key, "secret").unwrap();
        let mut other = vec![0u8; VAULT_KEY_LEN];
        OsRng.fill_bytes(&mut other);
        assert!(decrypt(&other, &nonce, &ct).is_err());
    }

    #[test]
    fn vault_key_file_is_owner_only() {
        let dir = TempDir::new().unwrap();
        let key = load_or_create_vault_key(dir.path()).unwrap();
        assert_eq!(key.len(), VAULT_KEY_LEN);
        let again = load_or_create_vault_key(dir.path()).unwrap();
        assert_eq!(key, again);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("mfa_vault_key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
