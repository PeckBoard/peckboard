//! Files agents hand to the user through the chat.
//!
//! A shared file is a **snapshot**: the bytes are copied into
//! `<data_dir>/shared-files/<session_id>/<uuid>` (with a `<uuid>.meta` JSON
//! sidecar) the moment it is shared, so the chat card keeps working after the
//! agent rewrites or deletes the original. Three ways in:
//!
//! - the `share_file` MCP tool (`origin: "share"`),
//! - Peckboard's own `write_file` creating a new image/video/archive/pdf
//!   (`origin: "created"`),
//! - images a provider CLI generated (`origin: "generated"`, see
//!   `crate::provider::generated_images`).
//!
//! Each emits a `file-shared` event. Bytes are served by
//! `routes::shared_files` through short-lived signed URLs (HMAC-SHA256 keyed
//! by the server's JWT secret) so `<img>` / `<video>` / a plain download link
//! can fetch them without an `Authorization` header.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Per-file snapshot cap.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Lifetime of a signed download URL.
pub const URL_TTL_SECS: u64 = 10 * 60;

/// Event kind persisted for every shared file.
pub const EVENT_KIND: &str = "file-shared";

/// How a file reached the chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// The agent called `share_file`.
    Share,
    /// Peckboard's `write_file` created a new artifact.
    Created,
    /// A provider CLI generated it (image generation).
    Generated,
}

/// The `<uuid>.meta` sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    /// Folder-relative path the snapshot was taken from (display only).
    pub source_path: String,
    pub origin: Origin,
}

/// Snapshot directory for one session.
pub fn dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("shared-files").join(session_id)
}

/// Same safe charset as the attachment / tool-image stores.
pub fn is_safe_id(id: &str) -> bool {
    crate::service::tool_images::is_safe_id(id)
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

/// Leaf name only, no control chars / quotes / backslashes.
fn clean_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    let leaf = Path::new(&cleaned)
        .file_name()
        .map(|n| n.to_string_lossy().trim().to_string())
        .unwrap_or_default();
    if leaf.is_empty() {
        "file".to_string()
    } else {
        leaf
    }
}

/// Best-guess MIME type from the file name.
pub fn guess_mime(filename: &str) -> String {
    mime_guess::from_path(filename)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// Extensions `write_file` auto-shares when it creates a new file: things a
/// user downloads or looks at, never source code (that shows as a diff).
pub fn is_auto_share_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    const EXTS: &[&str] = &[
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".mp4", ".webm", ".mov", ".zip", ".tar", ".gz",
        ".tgz", ".bz2", ".xz", ".7z", ".pdf",
    ];
    EXTS.iter().any(|e| lower.ends_with(e))
}

/// Resolve an agent-supplied `path` to a regular file inside `root` (the
/// canonical folder root). Relative paths are jailed by
/// [`crate::service::fs_jail::resolve_read`]; an absolute path is accepted
/// only when it already lies under `root`. Returns `(canonical, rel)`.
pub fn resolve_in_folder(root: &Path, path: &str) -> Result<(PathBuf, String), String> {
    let p = Path::new(path);
    let rel: PathBuf = if p.is_absolute() {
        let canon = std::fs::canonicalize(p).map_err(|e| format!("file not found: {e}"))?;
        canon
            .strip_prefix(root)
            .map_err(|_| "path escapes the project folder".to_string())?
            .to_path_buf()
    } else {
        p.to_path_buf()
    };
    let canon = crate::service::fs_jail::resolve_read(root, &rel)?;
    let rel = canon
        .strip_prefix(root)
        .map(|r| r.to_string_lossy().to_string())
        .unwrap_or_else(|_| rel.to_string_lossy().to_string());
    Ok((canon, rel))
}

/// Snapshot `src` into the session's store. Returns the new file id and its
/// sidecar.
pub async fn store_copy(
    data_dir: &Path,
    session_id: &str,
    src: &Path,
    source_path: &str,
    origin: Origin,
) -> anyhow::Result<(String, Meta)> {
    if !is_safe_id(session_id) {
        anyhow::bail!("unsafe session id");
    }
    let meta = tokio::fs::metadata(src).await?;
    if !meta.is_file() {
        anyhow::bail!("not a regular file");
    }
    if meta.len() > MAX_FILE_BYTES {
        anyhow::bail!(
            "file is {} bytes; the share limit is {} bytes (2 GB)",
            meta.len(),
            MAX_FILE_BYTES
        );
    }
    let blob_dir = dir(data_dir, session_id);
    tokio::fs::create_dir_all(&blob_dir).await?;
    restrict(&blob_dir, 0o700);

    let id = uuid::Uuid::new_v4().to_string();
    let blob = blob_dir.join(&id);
    let size = tokio::fs::copy(src, &blob).await?;
    restrict(&blob, 0o600);

    let filename = clean_filename(
        &src.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    let sidecar = Meta {
        mime_type: guess_mime(&filename),
        filename,
        size,
        source_path: source_path.to_string(),
        origin,
    };
    let meta_path = blob_dir.join(format!("{id}.meta"));
    tokio::fs::write(&meta_path, serde_json::to_vec(&sidecar)?).await?;
    restrict(&meta_path, 0o600);
    Ok((id, sidecar))
}

/// Read a snapshot's sidecar and blob path. `None` for unsafe ids or a
/// missing file.
pub async fn lookup(data_dir: &Path, session_id: &str, file_id: &str) -> Option<(Meta, PathBuf)> {
    if !is_safe_id(session_id) || !is_safe_id(file_id) {
        return None;
    }
    let blob_dir = dir(data_dir, session_id);
    let raw = tokio::fs::read(blob_dir.join(format!("{file_id}.meta")))
        .await
        .ok()?;
    let mut meta: Meta = serde_json::from_slice(&raw).ok()?;
    meta.filename = clean_filename(&meta.filename);
    let blob = blob_dir.join(file_id);
    tokio::fs::metadata(&blob)
        .await
        .ok()?
        .is_file()
        .then_some(())?;
    Some((meta, blob))
}

/// HMAC-SHA256 (RFC 2104) — the crate already depends on `sha2`, so this
/// avoids pulling in `hmac` for one call site.
pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// Hex signature over `(session_id, file_id, exp)`.
pub fn sign(secret: &[u8], session_id: &str, file_id: &str, exp: u64) -> String {
    let msg = format!("shared-file\n{session_id}\n{file_id}\n{exp}");
    hex::encode(hmac_sha256(secret, msg.as_bytes()))
}

/// Constant-time check of a signature and its expiry against `now`.
pub fn verify(
    secret: &[u8],
    session_id: &str,
    file_id: &str,
    exp: u64,
    sig: &str,
    now: u64,
) -> bool {
    if exp < now {
        return false;
    }
    let want = sign(secret, session_id, file_id, exp);
    let (a, b) = (want.as_bytes(), sig.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Relative signed URL valid for [`URL_TTL_SECS`] from `now`.
pub fn signed_url(secret: &[u8], session_id: &str, file_id: &str, now: u64) -> String {
    let exp = now + URL_TTL_SECS;
    let sig = sign(secret, session_id, file_id, exp);
    format!("/api/shared-files/{file_id}?sid={session_id}&exp={exp}&sig={sig}")
}

/// Parse a single-range `Range: bytes=…` header against `size`. `Ok(None)`
/// = no/unsupported header (serve the whole file); `Err(())` = 416.
pub fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.split_once('-') else {
        return Ok(None);
    };
    let (a, b) = (a.trim(), b.trim());
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 || size == 0 {
            return Err(());
        }
        (size.saturating_sub(n), size - 1)
    } else {
        let start: u64 = a.parse().map_err(|_| ())?;
        let end = if b.is_empty() {
            size.saturating_sub(1)
        } else {
            b.parse::<u64>()
                .map_err(|_| ())?
                .min(size.saturating_sub(1))
        };
        (start, end)
    };
    if start >= size || start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

/// The `file-shared` event payload.
pub fn event_payload(id: &str, meta: &Meta, caption: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": id,
        "filename": meta.filename,
        "mimeType": meta.mime_type,
        "size": meta.size,
        "sourcePath": meta.source_path,
        "origin": meta.origin,
    });
    if let Some(c) = caption.map(str::trim).filter(|c| !c.is_empty()) {
        v["caption"] = c.into();
    }
    v
}

/// Persist + broadcast a `file-shared` event (same shape as `file-diff`).
pub async fn emit(
    db: &Db,
    broadcaster: &Broadcaster,
    session_id: &str,
    payload: serde_json::Value,
) {
    let event = match db
        .append_event(session_id, EVENT_KIND, payload.clone())
        .await
    {
        Ok(ev) => ev,
        Err(e) => {
            tracing::warn!("failed to persist file-shared event: {e}");
            return;
        }
    };
    broadcaster.broadcast(WsEvent {
        event_type: "event".into(),
        session_id: session_id.to_string(),
        data: serde_json::json!({
            "id": event.id,
            "seq": event.seq,
            "ts": event.ts,
            "kind": EVENT_KIND,
            "data": payload,
        }),
    });
}

/// Snapshot `src` and emit its `file-shared` event. Returns the payload.
#[allow(clippy::too_many_arguments)]
pub async fn share(
    db: &Db,
    broadcaster: &Broadcaster,
    data_dir: &Path,
    session_id: &str,
    src: &Path,
    source_path: &str,
    origin: Origin,
    caption: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let (id, meta) = store_copy(data_dir, session_id, src, source_path, origin).await?;
    let payload = event_payload(&id, &meta, caption);
    emit(db, broadcaster, session_id, payload.clone()).await;
    Ok(payload)
}

/// Drop a session's snapshots (session delete).
pub fn remove_session_dir(data_dir: &Path, session_id: &str) {
    if !is_safe_id(session_id) {
        return;
    }
    let d = dir(data_dir, session_id);
    if d.exists()
        && let Err(e) = std::fs::remove_dir_all(&d)
    {
        tracing::warn!(session_id, dir = %d.display(), "Failed to remove shared-files dir: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_lookup_sign_verify_and_range() {
        let data = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let src = folder.path().join("build.zip");
        std::fs::write(&src, b"0123456789").unwrap();

        let (id, meta) = store_copy(data.path(), "sess-1", &src, "build.zip", Origin::Share)
            .await
            .unwrap();
        assert_eq!(meta.filename, "build.zip");
        assert_eq!(meta.mime_type, "application/zip");
        assert_eq!(meta.size, 10);
        // Snapshot: rewriting the original doesn't change the stored copy.
        std::fs::write(&src, b"changed").unwrap();
        let (got, blob) = lookup(data.path(), "sess-1", &id).await.unwrap();
        assert_eq!(got, meta);
        assert_eq!(std::fs::read(&blob).unwrap(), b"0123456789");
        assert!(lookup(data.path(), "sess-2", &id).await.is_none());
        assert!(lookup(data.path(), "sess-1", "../x").await.is_none());

        let secret = b"k".repeat(32);
        let now = 1_000;
        let exp = now + URL_TTL_SECS;
        let sig = sign(&secret, "sess-1", &id, exp);
        assert!(verify(&secret, "sess-1", &id, exp, &sig, now));
        // Expired.
        assert!(!verify(&secret, "sess-1", &id, exp, &sig, exp + 1));
        // Tampered: another session, another file, a stretched expiry, a
        // flipped signature byte, another key.
        assert!(!verify(&secret, "sess-2", &id, exp, &sig, now));
        assert!(!verify(&secret, "sess-1", "other", exp, &sig, now));
        assert!(!verify(&secret, "sess-1", &id, exp + 60, &sig, now));
        let mut bad = sig.clone().into_bytes();
        bad[0] = if bad[0] == b'a' { b'b' } else { b'a' };
        assert!(!verify(
            &secret,
            "sess-1",
            &id,
            exp,
            std::str::from_utf8(&bad).unwrap(),
            now
        ));
        assert!(!verify(&b"x".repeat(32), "sess-1", &id, exp, &sig, now));
        assert!(signed_url(&secret, "sess-1", &id, now).contains(&format!("sig={sig}")));

        // Range parsing.
        assert_eq!(parse_range(None, 10), Ok(None));
        assert_eq!(parse_range(Some("bytes=2-5"), 10), Ok(Some((2, 5))));
        assert_eq!(parse_range(Some("bytes=7-"), 10), Ok(Some((7, 9))));
        assert_eq!(parse_range(Some("bytes=-3"), 10), Ok(Some((7, 9))));
        assert_eq!(parse_range(Some("bytes=4-99"), 10), Ok(Some((4, 9))));
        assert_eq!(parse_range(Some("bytes=10-"), 10), Err(()));
        assert_eq!(parse_range(Some("bytes=0-1,4-5"), 10), Ok(None));

        remove_session_dir(data.path(), "sess-1");
        assert!(!dir(data.path(), "sess-1").exists());
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn resolve_in_folder_jails_paths() {
        let folder = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(folder.path()).unwrap();
        std::fs::write(root.join("a.txt"), b"x").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"s").unwrap();

        assert_eq!(resolve_in_folder(&root, "a.txt").unwrap().1, "a.txt");
        let abs = root.join("a.txt");
        assert_eq!(
            resolve_in_folder(&root, abs.to_str().unwrap()).unwrap().1,
            "a.txt"
        );
        assert!(resolve_in_folder(&root, "../secret.txt").is_err());
        let out = outside.path().join("secret.txt");
        assert!(resolve_in_folder(&root, out.to_str().unwrap()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&out, root.join("link.txt")).unwrap();
            assert!(resolve_in_folder(&root, "link.txt").is_err());
        }
    }
}
