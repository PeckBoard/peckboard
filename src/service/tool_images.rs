//! Central tool-output byte budget + tool-image blob offload.
//!
//! Every provider funnels its events through `emit_event`
//! (`crate::provider::agent`), which calls [`prepare_tool_end_payload`] on
//! `agent-tool-end` data before it is persisted or broadcast:
//!
//! - `output` / `error` strings are truncated at [`TOOL_OUTPUT_BUDGET_BYTES`]
//!   with an explicit marker; the payload gains `<field>Truncated: true` and
//!   `<field>OriginalBytes` so nothing is silently lost.
//! - inline base64 images (Playwright MCP screenshots, any image-returning
//!   MCP server) are written to `<data_dir>/tool-images/<session_id>/<uuid>`
//!   (with a `.mime` sidecar, mirroring the attachments store) and the event
//!   carries only `{mimeType, id}`. The authed
//!   `GET /api/sessions/:id/tool-images/:image_id` route serves the bytes.
//!
//! Events persisted before this existed keep their inline `dataBase64` and
//! must keep rendering — the web reader accepts both shapes (legacy-reader
//! rule).

use std::path::{Path, PathBuf};

/// Byte budget for a `ToolEnd` `output` / `error` string. Matches the cap
/// the Cursor parser used to apply locally before the cap moved here.
pub const TOOL_OUTPUT_BUDGET_BYTES: usize = 64 * 1024;

/// Ceiling for a decoded tool image. Screenshots are single-digit MB at
/// worst; anything bigger is treated as corrupt and kept inline (where the
/// oversized-frame guard still bounds the WS copy).
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// Broadcast-side backstop: a WS `event` frame whose serialized `data`
/// exceeds this gets every oversized top-level string field truncated with
/// the same marker. The DB copy stays authoritative.
pub const MAX_WS_EVENT_DATA_BYTES: usize = 1024 * 1024;

/// Truncate `s` to `budget` bytes (on a char boundary) and append the
/// truncation marker carrying the original byte count. `None` when `s`
/// already fits.
pub fn truncate_with_marker(s: &str, budget: usize) -> Option<String> {
    if s.len() <= budget {
        return None;
    }
    let mut end = budget;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!(
        "{}\n… [truncated: {} bytes total]",
        &s[..end],
        s.len()
    ))
}

/// Blob directory for one session's tool images.
pub fn dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("tool-images").join(session_id)
}

/// Same safe charset the attachments routes enforce: UUIDs and this app's
/// session ids fit; anything else can't escape the blob directory.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Keep only token-safe characters so the sidecar can never carry header
/// injection into the serving route.
fn sanitize_mime(mime: &str) -> String {
    mime.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '+' | '.'))
        .take(100)
        .collect()
}

/// Decode `data_base64` and persist it as a tool-image blob for
/// `session_id`. Returns the new blob id.
pub async fn store(
    data_dir: &Path,
    session_id: &str,
    mime_type: &str,
    data_base64: &str,
) -> anyhow::Result<String> {
    if !is_safe_id(session_id) {
        anyhow::bail!("unsafe session id");
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data_base64)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("image exceeds {MAX_IMAGE_BYTES} byte cap ({})", bytes.len());
    }

    let blob_dir = dir(data_dir, session_id);
    tokio::fs::create_dir_all(&blob_dir).await?;
    if let Err(e) = restrict_dir(&blob_dir) {
        tracing::warn!(dir = %blob_dir.display(), "Failed to restrict tool-image dir permissions: {e}");
    }

    let image_id = uuid::Uuid::new_v4().to_string();
    let file_path = blob_dir.join(&image_id);
    tokio::fs::write(&file_path, &bytes).await?;
    if let Err(e) = restrict_file(&file_path) {
        tracing::warn!(path = %file_path.display(), "Failed to restrict tool-image permissions: {e}");
    }

    let mime = sanitize_mime(mime_type);
    if !mime.is_empty() {
        let mime_path = blob_dir.join(format!("{image_id}.mime"));
        if let Err(e) = tokio::fs::write(&mime_path, &mime).await {
            tracing::warn!(image_id = %image_id, "Failed to persist tool-image mime sidecar: {e}");
        } else if let Err(e) = restrict_file(&mime_path) {
            tracing::warn!(path = %mime_path.display(), "Failed to restrict mime sidecar permissions: {e}");
        }
    }

    Ok(image_id)
}

/// Read a tool-image blob back: `(mime_type, bytes)`. `None` when either id
/// is outside the safe charset or the blob doesn't exist.
pub async fn read(data_dir: &Path, session_id: &str, image_id: &str) -> Option<(String, Vec<u8>)> {
    if !is_safe_id(session_id) || !is_safe_id(image_id) {
        return None;
    }
    let blob_dir = dir(data_dir, session_id);
    let bytes = tokio::fs::read(blob_dir.join(image_id)).await.ok()?;
    let mime = tokio::fs::read_to_string(blob_dir.join(format!("{image_id}.mime")))
        .await
        .map(|s| sanitize_mime(&s))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "image/png".to_string());
    Some((mime, bytes))
}

/// Apply the central payload budget to an `agent-tool-end` event's data:
/// cap `output` / `error`, and offload inline images to blob storage when a
/// `data_dir` is available (file-backed DB). On any offload failure the
/// image stays inline so nothing is lost.
pub async fn prepare_tool_end_payload(
    mut data: serde_json::Value,
    data_dir: Option<&Path>,
    session_id: &str,
) -> serde_json::Value {
    let Some(obj) = data.as_object_mut() else {
        return data;
    };

    for key in ["output", "error"] {
        let Some(s) = obj.get(key).and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(truncated) = truncate_with_marker(s, TOOL_OUTPUT_BUDGET_BYTES) {
            let original_bytes = s.len();
            obj.insert(key.to_string(), truncated.into());
            obj.insert(format!("{key}Truncated"), true.into());
            obj.insert(format!("{key}OriginalBytes"), original_bytes.into());
        }
    }

    let Some(data_dir) = data_dir else {
        return data;
    };
    let Some(images) = obj.get_mut("images").and_then(|v| v.as_array_mut()) else {
        return data;
    };
    for img in images.iter_mut() {
        let Some(io) = img.as_object_mut() else {
            continue;
        };
        let Some(b64) = io
            .get("dataBase64")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let mime = io
            .get("mimeType")
            .and_then(|v| v.as_str())
            .unwrap_or("image/png")
            .to_string();
        match store(data_dir, session_id, &mime, &b64).await {
            Ok(id) => {
                io.remove("dataBase64");
                io.insert("id".to_string(), id.into());
            }
            Err(e) => {
                tracing::warn!(
                    session_id = session_id,
                    "Tool image offload failed — keeping inline: {e}"
                );
            }
        }
    }
    data
}

/// Broadcast-side frame guard: truncate every oversized top-level string
/// field with the shared marker. Called only when the serialized data
/// already blew past [`MAX_WS_EVENT_DATA_BYTES`]; the stored event keeps
/// the full payload.
pub fn cap_ws_event_data(data: &mut serde_json::Value) {
    let Some(obj) = data.as_object_mut() else {
        return;
    };
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        let Some(s) = obj.get(&key).and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(truncated) = truncate_with_marker(s, TOOL_OUTPUT_BUDGET_BYTES) {
            let original_bytes = s.len();
            obj.insert(key.clone(), truncated.into());
            obj.insert(format!("{key}Truncated"), true.into());
            obj.insert(format!("{key}OriginalBytes"), original_bytes.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_none_when_within_budget() {
        assert_eq!(truncate_with_marker("hello", 5), None);
        assert_eq!(truncate_with_marker("", 0), None);
    }

    #[test]
    fn truncate_appends_marker_with_original_byte_count() {
        let s = "x".repeat(100);
        let t = truncate_with_marker(&s, 10).expect("truncates");
        assert!(t.starts_with(&"x".repeat(10)));
        assert!(t.ends_with("… [truncated: 100 bytes total]"), "{t}");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // 'é' is 2 bytes; a budget landing mid-char must back off.
        let s = "ééééé"; // 10 bytes
        let t = truncate_with_marker(s, 3).expect("truncates");
        assert!(t.starts_with('é'));
        assert!(t.contains("[truncated: 10 bytes total]"));
    }

    #[test]
    fn safe_id_charset() {
        assert!(is_safe_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_safe_id("session_42"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("../etc"));
        assert!(!is_safe_id("a/b"));
    }

    #[tokio::test]
    async fn store_and_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        use base64::Engine as _;
        let payload = b"\x89PNG\r\nfake-bytes";
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);

        let id = store(tmp.path(), "sess-1", "image/png", &b64)
            .await
            .expect("stores");
        let (mime, bytes) = read(tmp.path(), "sess-1", &id).await.expect("reads back");
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, payload);

        // Wrong session or unsafe id → nothing.
        assert!(read(tmp.path(), "sess-2", &id).await.is_none());
        assert!(read(tmp.path(), "sess-1", "../escape").await.is_none());
    }

    #[tokio::test]
    async fn prepare_offloads_images_and_caps_output() {
        let tmp = tempfile::tempdir().unwrap();
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"img-bytes");
        let big = "y".repeat(TOOL_OUTPUT_BUDGET_BYTES + 100);

        let data = serde_json::json!({
            "toolUseId": "t1",
            "output": big,
            "error": null,
            "images": [{ "mimeType": "image/jpeg", "dataBase64": b64 }],
        });
        let out = prepare_tool_end_payload(data, Some(tmp.path()), "sess-1").await;

        // Output capped with marker + original byte count in the payload.
        let output = out["output"].as_str().unwrap();
        assert!(output.len() < TOOL_OUTPUT_BUDGET_BYTES + 64);
        assert!(output.contains(&format!(
            "[truncated: {} bytes total]",
            TOOL_OUTPUT_BUDGET_BYTES + 100
        )));
        assert_eq!(out["outputTruncated"], true);
        assert_eq!(
            out["outputOriginalBytes"],
            (TOOL_OUTPUT_BUDGET_BYTES + 100) as u64
        );

        // Image replaced by a reference; blob round-trips.
        let img = &out["images"][0];
        assert!(img.get("dataBase64").is_none());
        assert_eq!(img["mimeType"], "image/jpeg");
        let id = img["id"].as_str().expect("blob id");
        let (mime, bytes) = read(tmp.path(), "sess-1", id).await.expect("blob exists");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes, b"img-bytes");
    }

    #[tokio::test]
    async fn prepare_keeps_images_inline_without_data_dir() {
        let data = serde_json::json!({
            "toolUseId": "t1",
            "output": "ok",
            "images": [{ "mimeType": "image/png", "dataBase64": "aGk=" }],
        });
        let out = prepare_tool_end_payload(data.clone(), None, "sess-1").await;
        assert_eq!(out, data);
    }

    #[test]
    fn ws_guard_truncates_oversized_string_fields() {
        let mut data = serde_json::json!({
            "text": "z".repeat(TOOL_OUTPUT_BUDGET_BYTES + 5),
            "small": "keep",
            "n": 7,
        });
        cap_ws_event_data(&mut data);
        assert!(data["text"].as_str().unwrap().contains("[truncated:"));
        assert_eq!(data["textTruncated"], true);
        assert_eq!(data["small"], "keep");
        assert_eq!(data["n"], 7);
    }
}
