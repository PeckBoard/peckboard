//! Imports images a provider CLI generated into the session's folder.
//!
//! Codex writes generated images to `<CODEX_HOME>/generated_images/<thread>/`
//! and Grok to `<GROK_HOME>/sessions/<enc-cwd>/<session>/images/`. A stored
//! account's home lives under the data dir, which the agent sandbox denies
//! to every other spawn (and the folder-scoped file tools can't reach), so
//! the host copies each new image into [`IMPORT_DIR`] inside the folder.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::provider::stream::ToolImage;

/// Folder-relative directory imported images land in.
pub const IMPORT_DIR: &str = ".peckboard/generated-images";

/// Images larger than this are imported but not inlined into the chat.
const MAX_INLINE_BYTES: u64 = 8 * 1024 * 1024;

/// Clock slack when comparing a file's mtime against the turn start.
const MTIME_SLACK: Duration = Duration::from_secs(2);

/// Whether `plugin_id`'s CLI writes generated images we know how to find.
pub fn supports(plugin_id: &str) -> bool {
    matches!(plugin_id, "codex" | "grok")
}

/// The env key carrying `plugin_id`'s per-account home.
pub fn home_env_key(plugin_id: &str) -> Option<&'static str> {
    match plugin_id {
        "codex" => Some("CODEX_HOME"),
        "grok" => Some("GROK_HOME"),
        _ => None,
    }
}

/// Per-turn import state: where to look and what was already copied.
#[derive(Debug)]
pub struct Watch {
    /// The CLI's home (account dir); `None` = the user's default home.
    pub home: Option<PathBuf>,
    pub conversation_id: Option<String>,
    /// Only files modified at/after this are this turn's output.
    pub since: SystemTime,
    pub imported: HashSet<PathBuf>,
}

impl Watch {
    pub fn new() -> Self {
        Self {
            home: None,
            conversation_id: None,
            since: SystemTime::now(),
            imported: HashSet::new(),
        }
    }
}

impl Default for Watch {
    fn default() -> Self {
        Self::new()
    }
}

/// One image copied into the folder.
#[derive(Debug)]
pub struct Imported {
    /// Folder-relative path, e.g. `.peckboard/generated-images/x.png`.
    pub rel_path: String,
    /// Inline copy for the chat; `None` when over [`MAX_INLINE_BYTES`].
    pub image: Option<ToolImage>,
}

/// Conversation ids become path components: allow the UUID alphabet only.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn default_home(plugin_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    let dir = match plugin_id {
        "codex" => ".codex",
        "grok" => ".grok",
        _ => return None,
    };
    Some(PathBuf::from(home).join(dir))
}

/// Directories the CLI writes this conversation's images to.
fn source_dirs(plugin_id: &str, home: &Path, conversation_id: &str) -> Vec<PathBuf> {
    match plugin_id {
        "codex" => vec![home.join("generated_images").join(conversation_id)],
        // `sessions/<percent-encoded cwd>/<session id>/images`: the id is
        // unique, so scan every cwd dir rather than re-encoding the path.
        "grok" => std::fs::read_dir(home.join("sessions"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path().join(conversation_id).join("images"))
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// Copy every image this turn generated (and not yet imported) into
/// `folder/IMPORT_DIR`. Best-effort: unreadable files are skipped.
pub fn import(plugin_id: &str, watch: &mut Watch, folder: &Path) -> Vec<Imported> {
    let Some(conv) = watch.conversation_id.clone().filter(|c| safe_id(c)) else {
        return Vec::new();
    };
    let Some(home) = watch.home.clone().or_else(|| default_home(plugin_id)) else {
        return Vec::new();
    };
    let since = watch.since.checked_sub(MTIME_SLACK).unwrap_or(watch.since);
    let mut out = Vec::new();
    for dir in source_dirs(plugin_id, &home, &conv) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<(PathBuf, std::fs::Metadata)> = rd
            .flatten()
            .filter_map(|e| Some((e.path(), e.metadata().ok()?)))
            .filter(|(p, m)| m.is_file() && image_mime(p).is_some())
            .filter(|(_, m)| m.modified().is_ok_and(|t| t >= since))
            .filter(|(p, _)| !watch.imported.contains(p))
            .collect();
        files.sort_by_key(|(_, m)| m.modified().ok());
        for (src, meta) in files {
            let Some(name) = src.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Grok numbers images per session (`1.jpg`), so prefix the id.
            let dest_name = match plugin_id {
                "grok" => format!("grok-{conv}-{name}"),
                _ => name.to_string(),
            };
            let dest_dir = folder.join(IMPORT_DIR);
            if std::fs::create_dir_all(&dest_dir).is_err() {
                return out;
            }
            if let Err(e) = std::fs::copy(&src, dest_dir.join(&dest_name)) {
                tracing::warn!(src = %src.display(), "importing generated image failed: {e}");
                continue;
            }
            watch.imported.insert(src.clone());
            let image = (meta.len() <= MAX_INLINE_BYTES)
                .then(|| std::fs::read(&src).ok())
                .flatten()
                .map(|bytes| {
                    use base64::Engine;
                    ToolImage {
                        mime_type: image_mime(&src).unwrap_or("image/png").to_string(),
                        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    }
                });
            out.push(Imported {
                rel_path: format!("{IMPORT_DIR}/{dest_name}"),
                image,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(home: &Path, conv: &str) -> Watch {
        Watch {
            home: Some(home.to_path_buf()),
            conversation_id: Some(conv.into()),
            ..Watch::new()
        }
    }

    #[test]
    fn imports_codex_and_grok_images_once_into_the_folder() {
        let home = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();

        let codex_dir = home.path().join("generated_images/thread-1");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(codex_dir.join("exec-a.png"), b"png").unwrap();
        std::fs::write(codex_dir.join("notes.txt"), b"x").unwrap();
        let other = home.path().join("generated_images/thread-2");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("exec-b.png"), b"png").unwrap();

        let mut w = watch(home.path(), "thread-1");
        let got = import("codex", &mut w, folder.path());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].rel_path, ".peckboard/generated-images/exec-a.png");
        assert_eq!(got[0].image.as_ref().unwrap().mime_type, "image/png");
        assert!(folder.path().join(&got[0].rel_path).is_file());
        assert!(import("codex", &mut w, folder.path()).is_empty());

        let grok_dir = home.path().join("sessions/%2Fsome%2Fcwd/sid-9/images");
        std::fs::create_dir_all(&grok_dir).unwrap();
        std::fs::write(grok_dir.join("1.jpg"), b"jpg").unwrap();
        let mut w = watch(home.path(), "sid-9");
        let got = import("grok", &mut w, folder.path());
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].rel_path,
            ".peckboard/generated-images/grok-sid-9-1.jpg"
        );
    }

    #[test]
    fn skips_images_older_than_the_turn_and_unsafe_ids() {
        let home = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let dir = home.path().join("generated_images/t");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.png"), b"png").unwrap();

        let mut w = watch(home.path(), "t");
        w.since = SystemTime::now() + Duration::from_secs(60);
        assert!(import("codex", &mut w, folder.path()).is_empty());

        let mut w = watch(home.path(), "../t");
        assert!(import("codex", &mut w, folder.path()).is_empty());
    }
}
