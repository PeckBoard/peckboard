//! Real `screenshot` capability: cross-platform display capture via
//! `xcap` 0.4 (macOS / Windows / Linux X11 + wlroots-Wayland via
//! libwayshot; GNOME/KDE Wayland go through the D-Bus screenshot portal).
//!
//! Pinned to xcap 0.4 with the `vendored` feature deliberately: 0.5+
//! hard-requires libpipewire at build time (pkg-config), which breaks
//! builds on machines without the PipeWire dev packages; 0.4 needs no
//! system libraries beyond X11's.
//!
//! Result payload uses the `image_base64` + `mime` keys that the Phase 3
//! bridge (`src/service/mcp_server/handlers/remote_agent.rs::image_result`)
//! maps onto the `_image_base64` MCP image convention.
//!
//! Gating (kill-switch + per-capability flag) happens in the client loop
//! before dispatch, like every other capability, and the client loop also
//! audits every request centrally (`crate::client::serve_request`) — no
//! per-executor audit calls needed here.

use std::io::Cursor;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use xcap::Monitor;

use crate::executor::{CapabilityExecutor, ExecContext};
use crate::windows::{WindowQuery, live_windows};

/// Captures a display or a single window and returns a base64 PNG.
///
/// Args:
/// - `{"list": true}` — don't capture; return the monitor table so a
///   caller can pick a display.
/// - `{"list_windows": true}` — don't capture; return every on-screen
///   window with its absolute screen bounds.
/// - `{"monitor": <index>}` — capture that monitor (index into the
///   `list` output). Omitted ⇒ the primary monitor (first, if none is
///   marked primary).
/// - `{"window_id": <id>}` / `{"app": "..", "title": ".."}` — capture
///   one window (see [`WindowQuery`]). The result's `window` block holds
///   the window's absolute top-left, so image pixel `(px, py)` sits at
///   screen `(window.x + px / window.scale, window.y + py / window.scale)`.
pub struct ScreenshotExecutor;

#[async_trait]
impl CapabilityExecutor for ScreenshotExecutor {
    async fn execute(&self, _ctx: &ExecContext, args: Value) -> Result<Value, String> {
        let list_only = args.get("list").and_then(Value::as_bool).unwrap_or(false);
        let list_windows = args
            .get("list_windows")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let window = WindowQuery::from_args(&args)?;
        let monitor_index = match args.get("monitor") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .ok_or_else(|| "'monitor' must be a non-negative integer index".to_string())?
                    as usize,
            ),
        };
        if window.is_some() && monitor_index.is_some() {
            return Err("pass either 'monitor' or a window target, not both".to_string());
        }

        // xcap is synchronous (xcb / dbus / CoreGraphics calls); keep it
        // off the async runtime threads.
        tokio::task::spawn_blocking(move || {
            if list_only {
                list_monitors()
            } else if list_windows {
                list_all_windows()
            } else if let Some(query) = window {
                capture_window(&query)
            } else {
                capture(monitor_index)
            }
        })
        .await
        .map_err(|e| format!("screenshot task panicked: {e}"))?
    }
}

fn list_all_windows() -> Result<Value, String> {
    let rows: Vec<Value> = live_windows()?
        .iter()
        .map(|(info, _)| info.to_json())
        .collect();
    Ok(json!({ "windows": rows }))
}

fn capture_window(query: &WindowQuery) -> Result<Value, String> {
    let windows = live_windows()?;
    let infos: Vec<_> = windows.iter().map(|(i, _)| i.clone()).collect();
    let (info, window) = &windows[query.select(&infos)?];
    let image = window
        .capture_image()
        .map_err(|e| capture_error(e.to_string()))?;
    let mut out = encode_png(&image)?;
    // Measured pixels-per-screen-unit, so callers can map image pixels
    // back to screen coordinates even when the platform reports geometry
    // in points (macOS Retina).
    let mut window_json = info.to_json();
    if info.width > 0 {
        window_json["scale"] = json!(image.width() as f32 / info.width as f32);
    }
    out["window"] = window_json;
    Ok(out)
}

/// PNG-encode a capture into the shared `image_base64`/`mime`/size shape.
fn encode_png(image: &image::RgbaImage) -> Result<Value, String> {
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode failed: {e}"))?;
    if png.is_empty() {
        return Err("capture produced an empty image".to_string());
    }
    Ok(json!({
        "image_base64": base64::engine::general_purpose::STANDARD.encode(&png),
        "mime": "image/png",
        "width": image.width(),
        "height": image.height(),
    }))
}

fn list_monitors() -> Result<Value, String> {
    let monitors = monitors()?;
    let rows: Vec<Value> = monitors
        .iter()
        .enumerate()
        .map(|(index, m)| {
            json!({
                "index": index,
                "name": m.name().ok(),
                "width": m.width().ok(),
                "height": m.height().ok(),
                "x": m.x().ok(),
                "y": m.y().ok(),
                "scale_factor": m.scale_factor().ok(),
                "is_primary": m.is_primary().unwrap_or(false),
            })
        })
        .collect();
    Ok(json!({ "monitors": rows }))
}

fn capture(monitor_index: Option<usize>) -> Result<Value, String> {
    let monitors = monitors()?;
    let (index, monitor) = match monitor_index {
        Some(i) => (
            i,
            monitors.get(i).ok_or_else(|| {
                format!(
                    "monitor index {i} out of range ({} available; use {{\"list\": true}})",
                    monitors.len()
                )
            })?,
        ),
        None => monitors
            .iter()
            .enumerate()
            .find(|(_, m)| m.is_primary().unwrap_or(false))
            .or_else(|| monitors.iter().enumerate().next())
            .ok_or_else(no_monitors_error)?,
    };

    let image = monitor
        .capture_image()
        .map_err(|e| capture_error(e.to_string()))?;
    let mut out = encode_png(&image)?;
    out["monitor"] = json!({
        "index": index,
        "name": monitor.name().ok(),
        "x": monitor.x().ok(),
        "y": monitor.y().ok(),
        "scale_factor": monitor.scale_factor().ok(),
    });
    Ok(out)
}

/// Enumerate monitors, translating the platform failure modes into
/// actionable errors instead of raw library messages.
fn monitors() -> Result<Vec<Monitor>, String> {
    if let Some(err) = headless_error() {
        return Err(err);
    }
    let monitors = Monitor::all().map_err(|e| capture_error(e.to_string()))?;
    if monitors.is_empty() {
        return Err(no_monitors_error());
    }
    Ok(monitors)
}

fn no_monitors_error() -> String {
    if cfg!(target_os = "macos") {
        macos_permission_hint("no displays visible to the agent")
    } else {
        "no monitors detected".to_string()
    }
}

/// Wrap a raw capture/enumeration error with the platform-specific fix.
pub(crate) fn capture_error(raw: String) -> String {
    if cfg!(target_os = "macos") {
        // CoreGraphics reports permission failures opaquely (empty lists,
        // blank captures, CGError). Screen Recording consent is by far the
        // most common cause, so always include the pointer.
        return macos_permission_hint(&raw);
    }
    if cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return format!(
            "screen capture failed: {raw}. On Wayland, capture uses the wlr \
             screencopy protocol or the desktop screenshot portal \
             (xdg-desktop-portal) — make sure one is available and that the \
             screenshot permission prompt was not denied."
        );
    }
    format!("screen capture failed: {raw}")
}

fn macos_permission_hint(raw: &str) -> String {
    format!(
        "screen capture failed: {raw}. If this Mac has not granted the agent \
         Screen Recording access, open System Settings → Privacy & Security → \
         Screen Recording, enable it for peckboard-agent (or the terminal that \
         launched it), then restart the agent."
    )
}

/// Linux without any display server can never capture — say so directly
/// instead of surfacing an xcb/portal connection error.
pub(crate) fn headless_error() -> Option<String> {
    if cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        return Some(
            "no display server available (DISPLAY and WAYLAND_DISPLAY are both \
             unset) — screen capture requires a graphical session"
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::tests_support::test_ctx;
    use serde_json::json;

    /// True when this machine can plausibly capture at all. CI and other
    /// headless boxes skip the live-capture assertions gracefully.
    fn display_available() -> bool {
        headless_error().is_none()
    }

    #[tokio::test]
    async fn capture_returns_nonempty_png() {
        if !display_available() {
            eprintln!("skipping: headless environment, no display server");
            return;
        }
        let out = match ScreenshotExecutor.execute(&test_ctx(), json!({})).await {
            Ok(v) => v,
            Err(e) if e.contains("no monitors") || e.contains("no displays") => {
                eprintln!("skipping: display server present but no monitors: {e}");
                return;
            }
            Err(e) => panic!("capture failed: {e}"),
        };
        let b64 = out["image_base64"].as_str().expect("image_base64 present");
        let png = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("valid base64");
        assert!(png.len() > 8, "png suspiciously small: {} bytes", png.len());
        assert_eq!(&png[..4], b"\x89PNG", "missing PNG magic");
        assert_eq!(out["mime"], "image/png");
        assert!(out["width"].as_u64().unwrap_or(0) > 0);
    }

    #[tokio::test]
    async fn list_mode_returns_monitor_table() {
        if !display_available() {
            eprintln!("skipping: headless environment, no display server");
            return;
        }
        let out = match ScreenshotExecutor
            .execute(&test_ctx(), json!({"list": true}))
            .await
        {
            Ok(v) => v,
            Err(e) if e.contains("no monitors") || e.contains("no displays") => {
                eprintln!("skipping: display server present but no monitors: {e}");
                return;
            }
            Err(e) => panic!("list failed: {e}"),
        };
        let rows = out["monitors"].as_array().expect("monitors array");
        assert!(!rows.is_empty());
        assert_eq!(rows[0]["index"], 0);
    }

    #[tokio::test]
    async fn list_windows_then_capture_one_by_id() {
        if !display_available() {
            eprintln!("skipping: headless environment, no display server");
            return;
        }
        let out = match ScreenshotExecutor
            .execute(&test_ctx(), json!({"list_windows": true}))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping: window enumeration unavailable: {e}");
                return;
            }
        };
        let rows = out["windows"].as_array().expect("windows array");
        eprintln!("{} windows listed", rows.len());
        let Some(target) = rows.iter().find(|w| w["is_minimized"] == false) else {
            eprintln!("skipping: no visible windows");
            return;
        };
        let id = target["window_id"].as_u64().expect("window_id");
        let shot = match ScreenshotExecutor
            .execute(&test_ctx(), json!({"window_id": id}))
            .await
        {
            Ok(v) => v,
            // Some X servers refuse to read unmapped / override windows.
            Err(e) => {
                eprintln!("skipping: window {id} not capturable: {e}");
                return;
            }
        };
        assert_eq!(shot["window"]["window_id"], id);
        assert_eq!(shot["window"]["x"], target["x"]);
        assert!(shot["width"].as_u64().unwrap_or(0) > 0);
    }

    #[tokio::test]
    async fn window_and_monitor_targets_are_exclusive() {
        let err = ScreenshotExecutor
            .execute(&test_ctx(), json!({"monitor": 0, "app": "firefox"}))
            .await
            .unwrap_err();
        assert!(err.contains("not both"), "got: {err}");
    }

    #[tokio::test]
    async fn bad_monitor_arg_is_rejected() {
        let err = ScreenshotExecutor
            .execute(&test_ctx(), json!({"monitor": "zero"}))
            .await
            .unwrap_err();
        assert!(err.contains("non-negative integer"), "got: {err}");
    }

    #[tokio::test]
    async fn out_of_range_monitor_errors_cleanly() {
        if !display_available() {
            eprintln!("skipping: headless environment, no display server");
            return;
        }
        let err = ScreenshotExecutor
            .execute(&test_ctx(), json!({"monitor": 9999}))
            .await
            .unwrap_err();
        assert!(
            err.contains("out of range") || err.contains("no monitors"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn headless_linux_error_is_actionable() {
        if display_available() {
            eprintln!("skipping: display present, headless path untestable");
            return;
        }
        let err = ScreenshotExecutor
            .execute(&test_ctx(), json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("no display server available"), "got: {err}");
    }
}
