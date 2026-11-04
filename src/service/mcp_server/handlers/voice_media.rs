//! `show_media` / `media_control` / `hide_media` — the voice assistant shows
//! an image, video, or 3D model above the orb on the Assistant page and drives
//! it. Voice-session-only, like `show_view`.
//!
//! Commands reach the browser as a per-user `assistant-media` frame. Every tab
//! gets it, so whichever one holds the page stays in step; tabs that aren't
//! visible keep the state without playing. The browser applies the command and
//! clamps it, so results report what was sent, not what happened.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::shared_files::{self, Origin};
use crate::service::voice_relay::VOICE_EXPERT_KIND;
use crate::ws::broadcaster::{UserAudience, WsEvent};

/// Per-user WS frame `web/src/store/assistantMedia.ts` applies.
pub(crate) const ASSISTANT_MEDIA_EVENT: &str = "assistant-media";

const MAX_ROTATE_DEGREES: f64 = 3600.0;
const MIN_ZOOM: f64 = 0.1;
const MAX_ZOOM: f64 = 10.0;

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif"];
const VIDEO_EXTS: &[&str] = &["mp4", "webm", "mov", "m4v", "ogv"];
/// `web/src/components/model/formats.ts`.
const MODEL_EXTS: &[&str] = &[
    "stl", "3mf", "obj", "ply", "amf", "gcode", "glb", "gltf", "fbx", "dae", "usdz",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
    Model,
}

impl MediaKind {
    fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Model => "model",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "image" => Some(MediaKind::Image),
            "video" => Some(MediaKind::Video),
            "model" => Some(MediaKind::Model),
            _ => None,
        }
    }

    /// From a file name or a URL's path.
    fn from_name(name: &str) -> Option<Self> {
        let path = name.split(['?', '#']).next().unwrap_or(name);
        let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
        let ext = ext.as_str();
        if IMAGE_EXTS.contains(&ext) {
            Some(MediaKind::Image)
        } else if VIDEO_EXTS.contains(&ext) {
            Some(MediaKind::Video)
        } else if MODEL_EXTS.contains(&ext) {
            Some(MediaKind::Model)
        } else {
            None
        }
    }
}

/// What each voice session has on screen, so a command with nothing showing
/// (or the wrong kind) is an error the assistant can't report as a success.
/// In memory: the embed itself doesn't survive a reload or restart either.
static OPEN: LazyLock<Mutex<HashMap<String, MediaKind>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn open_kind(session_id: &str) -> Option<MediaKind> {
    OPEN.lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(session_id)
        .copied()
}

fn set_open(session_id: &str, kind: Option<MediaKind>) {
    let mut open = OPEN.lock().unwrap_or_else(|p| p.into_inner());
    match kind {
        Some(k) => open.insert(session_id.to_string(), k),
        None => open.remove(session_id),
    };
}

/// A finite number argument, if present.
fn number(args: &Value, key: &str) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| format!("'{key}' must be a number")),
    }
}

/// Validate a `media_control` call against what's showing and build the frame.
fn control_frame(kind: MediaKind, args: &Value) -> Result<Value, String> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("media_control requires 'action'")?;
    let allowed: &[&str] = match kind {
        MediaKind::Video => &["play", "pause", "seek", "mute", "unmute"],
        MediaKind::Image | MediaKind::Model => &["rotate", "zoom", "reset"],
    };
    if !allowed.contains(&action) {
        return Err(format!(
            "'{action}' doesn't apply to the {} showing now. It takes: {}.",
            kind.as_str(),
            allowed.join(", ")
        ));
    }
    match action {
        "seek" => match (number(args, "seconds")?, number(args, "by")?) {
            (Some(seconds), None) if seconds >= 0.0 => {
                Ok(json!({ "action": "seek", "seconds": seconds }))
            }
            (None, Some(by)) if by != 0.0 => Ok(json!({ "action": "seek", "by": by })),
            _ => Err(
                "seek takes either 'seconds' (a position, 0 or more) or 'by' (a non-zero offset, \
                 negative to go back)."
                    .into(),
            ),
        },
        "rotate" => {
            let degrees = number(args, "degrees")?
                .filter(|d| *d != 0.0 && d.abs() <= MAX_ROTATE_DEGREES)
                .ok_or("rotate requires non-zero 'degrees' (negative reverses direction).")?;
            let axis = args
                .get("axis")
                .and_then(|v| v.as_str())
                .unwrap_or("vertical");
            if !matches!(axis, "vertical" | "horizontal") {
                return Err("'axis' must be vertical or horizontal.".into());
            }
            Ok(json!({ "action": "rotate", "degrees": degrees, "axis": axis }))
        }
        "zoom" => {
            let factor = number(args, "factor")?
                .filter(|f| *f > 0.0)
                .ok_or("zoom requires a positive 'factor': above 1 zooms in, below 1 zooms out.")?;
            Ok(json!({ "action": "zoom", "factor": factor.clamp(MIN_ZOOM, MAX_ZOOM) }))
        }
        _ => Ok(json!({ "action": action })),
    }
}

/// Resolve `path` to a file the voice session may show: relative paths from
/// its own folder, absolute ones from inside any registered folder (the voice
/// session is global, admin-only).
async fn resolve_path(ctx: &ToolCallContext, path: &str) -> anyhow::Result<(PathBuf, String)> {
    let roots: Vec<String> = if Path::new(path).is_absolute() {
        ctx.db
            .list_folders()
            .await?
            .into_iter()
            .map(|f| f.path)
            .collect()
    } else {
        let folder = ctx
            .db
            .get_folder(&ctx.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the assistant's folder was not found"))?;
        vec![folder.path]
    };
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut last = "the path isn't inside a registered folder".to_string();
        for root in roots {
            let Ok(root) = std::fs::canonicalize(&root) else {
                continue;
            };
            match shared_files::resolve_in_folder(&root, &path) {
                Ok(found) => return Ok(found),
                Err(e) => last = e,
            }
        }
        Err(anyhow::anyhow!(last))
    })
    .await?
}

impl McpToolRegistry {
    async fn require_voice(&self, ctx: &ToolCallContext, tool: &str) -> anyhow::Result<()> {
        let is_voice = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .is_some_and(|s| s.expert_kind.as_deref() == Some(VOICE_EXPERT_KIND));
        if !is_voice {
            anyhow::bail!("{tool} is only available to the voice assistant session");
        }
        Ok(())
    }

    /// Send one frame to all the owner's tabs; how many received it.
    async fn send_media_frame(&self, ctx: &ToolCallContext, data: Value) -> anyhow::Result<usize> {
        let caller = self.ui_caller(ctx).await?;
        Ok(ctx.broadcaster.send_to_user(
            &caller.user_id,
            UserAudience::AllTabs,
            WsEvent {
                event_type: ASSISTANT_MEDIA_EVENT.into(),
                session_id: caller.session_id.clone(),
                data,
            },
        ))
    }

    /// `show_media` — see the module docs.
    pub(crate) async fn handle_show_media(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, ?args, "MCP tool: show_media");
        self.require_voice(ctx, "show_media").await?;
        let text = |key: &str| {
            args.get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let (path, url) = (text("path"), text("url"));
        let named = match (path, url) {
            (Some(p), None) => p,
            (None, Some(u)) => u,
            _ => anyhow::bail!("show_media takes exactly one of 'path' or 'url'"),
        };
        let kind = match text("kind") {
            Some(k) => MediaKind::parse(k)
                .ok_or_else(|| anyhow::anyhow!("'kind' must be image, video, or model"))?,
            None => MediaKind::from_name(named).ok_or_else(|| {
                anyhow::anyhow!(
                    "can't tell what kind of media '{named}' is from its name; pass 'kind' (image, video, or model)"
                )
            })?,
        };
        let title = text("title");

        let mut frame = json!({ "action": "show", "kind": kind.as_str() });
        if let Some(url) = url {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                anyhow::bail!("'url' must be http or https; use 'path' for a local file");
            }
            frame["url"] = url.into();
            frame["title"] = title.unwrap_or(url).into();
        } else if let Some(path) = path {
            let data_dir = ctx
                .data_dir
                .clone()
                .ok_or_else(|| anyhow::anyhow!("files can't be shown in this context"))?;
            let (canon, rel) = resolve_path(ctx, path).await?;
            let shared = shared_files::share(
                &ctx.db,
                &ctx.broadcaster,
                &data_dir,
                &ctx.session_id,
                &canon,
                &rel,
                Origin::Share,
                title,
            )
            .await?;
            let filename = shared["filename"].as_str().unwrap_or(path).to_string();
            frame["file"] = json!({
                "session_id": ctx.session_id,
                "id": shared["id"],
                "name": filename,
            });
            frame["title"] = title.unwrap_or(&filename).into();
        }

        let delivered = self.send_media_frame(ctx, frame).await?;
        if delivered == 0 {
            return Ok(json!({
                "status": "no_browser",
                "message": "The user has no Peckboard tab open, so nothing was shown.",
            }));
        }
        set_open(&ctx.session_id, Some(kind));
        Ok(json!({
            "status": "ok",
            "kind": kind.as_str(),
            "message": "Sent to the Assistant page, above the orb. It shows when the user is on that page.",
        }))
    }

    /// `media_control` — see the module docs.
    pub(crate) async fn handle_media_control(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, ?args, "MCP tool: media_control");
        self.require_voice(ctx, "media_control").await?;
        let kind = open_kind(&ctx.session_id)
            .ok_or_else(|| anyhow::anyhow!("nothing is showing; call show_media first"))?;
        let command = control_frame(kind, &args).map_err(|e| anyhow::anyhow!(e))?;
        let delivered = self
            .send_media_frame(ctx, json!({ "action": "control", "command": command }))
            .await?;
        if delivered == 0 {
            return Ok(json!({
                "status": "no_browser",
                "message": "The user has no Peckboard tab open, so nothing changed.",
            }));
        }
        Ok(json!({ "status": "ok", "sent": command }))
    }

    /// `hide_media` — see the module docs.
    pub(crate) async fn handle_hide_media(
        &self,
        _args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: hide_media");
        self.require_voice(ctx, "hide_media").await?;
        set_open(&ctx.session_id, None);
        self.send_media_frame(ctx, json!({ "action": "hide" }))
            .await?;
        Ok(json!({ "status": "ok", "message": "Closed." }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession};
    use crate::ws::broadcaster::Broadcaster;
    use std::sync::Arc;

    #[test]
    fn kind_comes_from_the_name_and_ignores_url_extras() {
        assert_eq!(MediaKind::from_name("a/b.GLB"), Some(MediaKind::Model));
        assert_eq!(
            MediaKind::from_name("https://x.test/clip.mp4?t=3#y"),
            Some(MediaKind::Video)
        );
        assert_eq!(MediaKind::from_name("shot.png"), Some(MediaKind::Image));
        assert_eq!(MediaKind::from_name("notes.txt"), None);
        assert_eq!(MediaKind::from_name("https://x.test/watch"), None);
    }

    #[test]
    fn control_is_checked_against_what_is_showing() {
        let video = MediaKind::Video;
        assert_eq!(
            control_frame(video, &json!({"action":"seek","seconds":30})).unwrap()["seconds"],
            30.0
        );
        assert_eq!(
            control_frame(video, &json!({"action":"seek","by":-10})).unwrap()["by"],
            -10.0
        );
        // Both, neither, or a negative position is a mistake, not a guess.
        for bad in [
            json!({"action":"seek","seconds":1,"by":1}),
            json!({"action":"seek"}),
            json!({"action":"seek","seconds":-1}),
        ] {
            assert!(control_frame(video, &bad).is_err(), "{bad}");
        }
        let err = control_frame(video, &json!({"action":"rotate","degrees":90})).unwrap_err();
        assert!(err.contains("play, pause"), "{err}");

        let model = MediaKind::Model;
        let turn = control_frame(model, &json!({"action":"rotate","degrees":90})).unwrap();
        assert_eq!(turn["axis"], "vertical");
        assert!(control_frame(model, &json!({"action":"rotate","degrees":0})).is_err());
        assert!(control_frame(model, &json!({"action":"rotate","degrees":9,"axis":"z"})).is_err());
        assert!(control_frame(model, &json!({"action":"pause"})).is_err());
        assert_eq!(
            control_frame(model, &json!({"action":"zoom","factor":999})).unwrap()["factor"],
            MAX_ZOOM
        );
        assert!(control_frame(model, &json!({"action":"zoom","factor":0})).is_err());
    }

    #[tokio::test]
    async fn show_control_hide_reach_the_owner_and_paths_stay_in_folders() {
        let db = Arc::new(Db::in_memory().unwrap());
        let folder_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(folder_dir.path().join("clip.mp4"), b"mp4").unwrap();
        std::fs::write(outside.path().join("secret.png"), b"png").unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: folder_dir.path().to_string_lossy().to_string(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        for (id, kind) in [
            ("media-voice", Some(VOICE_EXPERT_KIND)),
            ("media-chat", None),
        ] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                created_at: now.clone(),
                last_activity: now.clone(),
                is_expert: kind.is_some(),
                expert_kind: kind.map(str::to_string),
                user_id: Some("u1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let broadcaster = Broadcaster::new();
        let mut user_rx = broadcaster.subscribe_user_frames();
        broadcaster.register_connection(1, "u1");
        let reg = McpToolRegistry::new();
        let ctx = |session: &str| ToolCallContext {
            session_id: session.into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: db.clone(),
            broadcaster: broadcaster.clone(),
            provider_registry: None,
            data_dir: Some(data_dir.path().to_path_buf()),
            device_registry: None,
            background: None,
        };
        let voice = ctx("media-voice");

        // Voice-only, whatever the model asks for by name.
        assert!(
            reg.handle_show_media(json!({"path":"clip.mp4"}), &ctx("media-chat"))
                .await
                .is_err()
        );
        // Nothing showing yet: not a silent success.
        let err = reg
            .handle_media_control(json!({"action":"pause"}), &voice)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nothing is showing"), "{err}");
        // Outside every registered folder.
        let secret = outside.path().join("secret.png");
        assert!(
            reg.handle_show_media(json!({"path": secret.to_string_lossy()}), &voice)
                .await
                .is_err()
        );

        let shown = reg
            .handle_show_media(json!({"path":"clip.mp4"}), &voice)
            .await
            .unwrap();
        assert_eq!(shown["status"], "ok");
        let frame = user_rx.try_recv().unwrap();
        assert!(frame.is_for(1), "the owner's browser");
        assert_eq!(frame.event().event_type, ASSISTANT_MEDIA_EVENT);
        assert_eq!(frame.event().data["kind"], "video");
        assert_eq!(frame.event().data["file"]["name"], "clip.mp4");

        reg.handle_media_control(json!({"action":"seek","by":-10}), &voice)
            .await
            .unwrap();
        assert_eq!(
            user_rx.try_recv().unwrap().event().data["command"]["by"],
            -10.0
        );

        reg.handle_hide_media(json!({}), &voice).await.unwrap();
        assert_eq!(user_rx.try_recv().unwrap().event().data["action"], "hide");
        assert!(
            reg.handle_media_control(json!({"action":"play"}), &voice)
                .await
                .is_err()
        );
    }
}
