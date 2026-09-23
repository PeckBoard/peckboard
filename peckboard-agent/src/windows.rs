//! Window enumeration + targeting, shared by the `screenshot` capability
//! (capture one app window) and the `mouse` capability (window-relative
//! coordinates).
//!
//! A window is addressed by a [`WindowQuery`]: an exact `window_id` from
//! `{"list_windows": true}`, or case-insensitive `app` / `title`
//! substrings. Selection is a pure function over [`WindowInfo`] rows so it
//! is unit-testable without a display.
//!
//! Coordinates: `x`/`y` are the window's top-left in absolute screen
//! coordinates — the same space the mouse executor feeds to enigo.
//! Window-relative points are in *screenshot pixels*; `scale` is the
//! pixels-per-screen-unit ratio (the monitor scale factor on macOS, where
//! xcap reports window geometry in points; 1.0 elsewhere).

use serde_json::{Value, json};
use xcap::Window;

/// One on-screen window, as reported to callers.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub id: u32,
    pub pid: Option<u32>,
    pub app_name: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_focused: bool,
    pub is_minimized: bool,
    pub monitor: Option<String>,
    pub scale: f32,
}

impl WindowInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "window_id": self.id,
            "pid": self.pid,
            "app_name": self.app_name,
            "title": self.title,
            "x": self.x,
            "y": self.y,
            "width": self.width,
            "height": self.height,
            "scale": self.scale,
            "is_focused": self.is_focused,
            "is_minimized": self.is_minimized,
            "monitor": self.monitor,
        })
    }

    /// Short human label used in error messages.
    fn label(&self) -> String {
        format!(
            "{} — \"{}\" (window_id {})",
            self.app_name, self.title, self.id
        )
    }

    /// Map a window-relative point (screenshot pixels, origin at the
    /// window's top-left) to absolute screen coordinates. Points outside
    /// the window are refused so a stale screenshot can't click elsewhere.
    pub fn to_screen(&self, x: i32, y: i32) -> Result<(i32, i32), String> {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        let w_px = (self.width as f32 * scale).round() as i32;
        let h_px = (self.height as f32 * scale).round() as i32;
        if x < 0 || y < 0 || x >= w_px || y >= h_px {
            return Err(format!(
                "point ({x}, {y}) is outside {} which is {w_px}x{h_px} px",
                self.label()
            ));
        }
        Ok((
            self.x + (x as f32 / scale).round() as i32,
            self.y + (y as f32 / scale).round() as i32,
        ))
    }
}

/// Which window a request targets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowQuery {
    pub id: Option<u32>,
    pub app: Option<String>,
    pub title: Option<String>,
}

impl WindowQuery {
    /// Read `window_id` / `app` / `title` from request args. `None` when
    /// none are present (the request isn't window-targeted).
    pub fn from_args(args: &Value) -> Result<Option<Self>, String> {
        let id = match args.get("window_id") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| "'window_id' must be a non-negative integer".to_string())?,
            ),
        };
        let text = |key: &str| -> Result<Option<String>, String> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
                Some(_) => Err(format!("'{key}' must be a string")),
            }
        };
        let q = Self {
            id,
            app: text("app")?,
            title: text("title")?,
        };
        Ok((q != Self::default()).then_some(q))
    }

    /// Pick the target window's index. An exact `id` wins; otherwise every
    /// given substring must match (case-insensitive), minimized windows
    /// are skipped, and the focused match is preferred over list order.
    pub fn select(&self, windows: &[WindowInfo]) -> Result<usize, String> {
        if let Some(id) = self.id {
            let i = windows.iter().position(|w| w.id == id).ok_or_else(|| {
                format!("no window with window_id {id}{}", candidates_hint(windows))
            })?;
            if windows[i].is_minimized {
                return Err(format!("{} is minimized", windows[i].label()));
            }
            return Ok(i);
        }
        let contains = |hay: &str, needle: &Option<String>| {
            needle
                .as_ref()
                .is_none_or(|n| hay.to_lowercase().contains(&n.to_lowercase()))
        };
        let matches: Vec<usize> = windows
            .iter()
            .enumerate()
            .filter(|(_, w)| {
                !w.is_minimized
                    && contains(&w.app_name, &self.app)
                    && contains(&w.title, &self.title)
            })
            .map(|(i, _)| i)
            .collect();
        matches
            .iter()
            .copied()
            .find(|&i| windows[i].is_focused)
            .or_else(|| matches.first().copied())
            .ok_or_else(|| {
                format!(
                    "no visible window matches app={:?} title={:?}{}",
                    self.app.as_deref().unwrap_or(""),
                    self.title.as_deref().unwrap_or(""),
                    candidates_hint(windows)
                )
            })
    }
}

fn candidates_hint(windows: &[WindowInfo]) -> String {
    let shown: Vec<String> = windows
        .iter()
        .filter(|w| !w.is_minimized)
        .take(20)
        .map(WindowInfo::label)
        .collect();
    if shown.is_empty() {
        "; no visible windows".to_string()
    } else {
        format!("; visible windows: {}", shown.join(", "))
    }
}

fn info(w: &Window) -> Option<WindowInfo> {
    let width = w.width().ok()?;
    let height = w.height().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let monitor = w.current_monitor().ok();
    let scale = if cfg!(target_os = "macos") {
        monitor
            .as_ref()
            .and_then(|m| m.scale_factor().ok())
            .unwrap_or(1.0)
    } else {
        1.0
    };
    Some(WindowInfo {
        id: w.id().ok()?,
        pid: w.pid().ok(),
        app_name: w.app_name().unwrap_or_default(),
        title: w.title().unwrap_or_default(),
        x: w.x().ok()?,
        y: w.y().ok()?,
        width,
        height,
        is_focused: w.is_focused().unwrap_or(false),
        is_minimized: w.is_minimized().unwrap_or(false),
        monitor: monitor.and_then(|m| m.name().ok()),
        scale,
    })
}

/// Enumerate live windows (blocking — call from `spawn_blocking`). Rows
/// whose geometry can't be read are dropped.
pub fn live_windows() -> Result<Vec<(WindowInfo, Window)>, String> {
    if let Some(err) = crate::screenshot::headless_error() {
        return Err(err);
    }
    let all = Window::all().map_err(|e| {
        let raw = e.to_string();
        // X11 window listing reads the window manager's EWMH stacking list;
        // a bare X server (no WM) can't provide it.
        if raw.contains("_NET_CLIENT_LIST") {
            format!(
                "window listing failed: {raw} — window targets need a desktop \
                 session with an EWMH window manager; use a monitor capture instead"
            )
        } else {
            crate::screenshot::capture_error(raw)
        }
    })?;
    Ok(all
        .into_iter()
        .filter_map(|w| info(&w).map(|i| (i, w)))
        .collect())
}

/// Resolves a [`WindowQuery`] to current window geometry. A trait so the
/// mouse executor's tests can supply fixed windows.
pub trait WindowLocator: Send + Sync {
    fn locate(&self, query: &WindowQuery) -> Result<WindowInfo, String>;
}

/// Live locator backed by xcap.
pub struct XcapLocator;

impl WindowLocator for XcapLocator {
    fn locate(&self, query: &WindowQuery) -> Result<WindowInfo, String> {
        let infos: Vec<WindowInfo> = live_windows()?.into_iter().map(|(i, _)| i).collect();
        let idx = query.select(&infos)?;
        Ok(infos[idx].clone())
    }
}

#[cfg(test)]
pub mod tests_support {
    use super::*;

    pub fn win(id: u32, app: &str, title: &str, x: i32, y: i32) -> WindowInfo {
        WindowInfo {
            id,
            pid: None,
            app_name: app.to_string(),
            title: title.to_string(),
            x,
            y,
            width: 800,
            height: 600,
            is_focused: false,
            is_minimized: false,
            monitor: None,
            scale: 1.0,
        }
    }

    /// Locator over a fixed window table.
    pub struct FixedLocator(pub Vec<WindowInfo>);

    impl WindowLocator for FixedLocator {
        fn locate(&self, query: &WindowQuery) -> Result<WindowInfo, String> {
            let idx = query.select(&self.0)?;
            Ok(self.0[idx].clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::win;
    use super::*;

    #[test]
    fn query_parsing() {
        assert_eq!(WindowQuery::from_args(&json!({})).unwrap(), None);
        assert_eq!(WindowQuery::from_args(&json!({"app": " "})).unwrap(), None);
        let q = WindowQuery::from_args(&json!({"app": "Fire", "title": "docs"}))
            .unwrap()
            .unwrap();
        assert_eq!(q.app.as_deref(), Some("Fire"));
        assert!(WindowQuery::from_args(&json!({"window_id": -1})).is_err());
        assert!(WindowQuery::from_args(&json!({"app": 3})).is_err());
    }

    #[test]
    fn select_prefers_focused_and_skips_minimized() {
        let mut windows = vec![
            win(1, "firefox", "Mail", 0, 0),
            win(2, "firefox", "Docs", 10, 10),
            win(3, "code", "main.rs", 0, 0),
        ];
        windows[1].is_focused = true;
        let q = |app: &str| WindowQuery {
            app: Some(app.into()),
            ..Default::default()
        };
        assert_eq!(q("FIREFOX").select(&windows).unwrap(), 1);
        windows[1].is_minimized = true;
        assert_eq!(q("firefox").select(&windows).unwrap(), 0);
        let title = WindowQuery {
            app: Some("firefox".into()),
            title: Some("docs".into()),
            ..Default::default()
        };
        let err = title.select(&windows).unwrap_err();
        assert!(err.contains("visible windows: firefox"), "got: {err}");
        let by_id = WindowQuery {
            id: Some(3),
            ..Default::default()
        };
        assert_eq!(by_id.select(&windows).unwrap(), 2);
    }

    #[test]
    fn to_screen_offsets_scales_and_bounds_checks() {
        let mut w = win(1, "a", "b", 100, 50);
        assert_eq!(w.to_screen(10, 20).unwrap(), (110, 70));
        assert!(w.to_screen(800, 0).is_err());
        assert!(w.to_screen(-1, 0).is_err());
        // Retina: screenshot pixels are 2x the window's point geometry.
        w.scale = 2.0;
        assert_eq!(w.to_screen(1598, 20).unwrap(), (899, 60));
    }
}
