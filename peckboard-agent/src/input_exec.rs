//! Mouse + keyboard capability executors — the two highest-risk
//! capabilities (they synthesize real OS input). They wrap the trait-based
//! [`InputBackend`] so CI never moves the real cursor, and layer the
//! card's safety rails on every request:
//!
//!   1. **preflight** — refuse LOUD on Wayland / missing macOS permission
//!      rather than silently no-op'ing (see [`InputBackend::preflight`]).
//!   2. **visible indicator** — announce on the controlled machine that
//!      input is being synthesized ([`Indicator`]), and also surface it to
//!      the server/panel as an `input-active` event.
//!   3. **audit** — one [`crate::audit`] entry per action (typed text
//!      contents redacted to a length; see [`InputAction::audit_detail`]).
//!
//! OFF-by-default and the global kill-switch are enforced upstream in
//! `config.rs` / `client.rs` before we're ever reached, so these executors
//! assume the machine owner has explicitly opted the capability in.
//!
//! Backend work runs inside [`tokio::task::spawn_blocking`]: the real
//! `enigo` handle is `!Send` on some platforms, so it must never be held
//! across an `.await`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::audit;
use crate::config::Config;
use crate::executor::{CapabilityExecutor, ExecContext};
use crate::input::{Indicator, InputAction, InputBackend, MouseButton};
use crate::windows::{WindowInfo, WindowLocator, WindowQuery, XcapLocator};

/// Parse a mouse button name; defaults to left when absent.
fn parse_button(args: &Value) -> Result<MouseButton, String> {
    match args.get("button").and_then(Value::as_str) {
        None => Ok(MouseButton::Left),
        Some("left") => Ok(MouseButton::Left),
        Some("right") => Ok(MouseButton::Right),
        Some("middle") => Ok(MouseButton::Middle),
        Some(other) => Err(format!("unknown mouse button '{other}'")),
    }
}

/// Read a required i32 field.
fn req_i32(args: &Value, key: &str) -> Result<i32, String> {
    args.get(key)
        .and_then(Value::as_i64)
        .map(|n| n as i32)
        .ok_or_else(|| format!("missing or non-integer field '{key}'"))
}

/// Read an `[x, y]` coordinate pair from `key`.
fn req_point(args: &Value, key: &str) -> Result<(i32, i32), String> {
    let arr = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing coordinate array '{key}' (expected [x, y])"))?;
    if arr.len() != 2 {
        return Err(format!("'{key}' must be a 2-element [x, y] array"));
    }
    let x = arr[0]
        .as_i64()
        .ok_or_else(|| format!("'{key}[0]' is not an integer"))? as i32;
    let y = arr[1]
        .as_i64()
        .ok_or_else(|| format!("'{key}[1]' is not an integer"))? as i32;
    Ok((x, y))
}

/// Turn mouse `args` into an [`InputAction`]. Shape:
/// `{op:"move"|"click"|"drag"|"scroll", ...}`.
fn parse_mouse(args: &Value) -> Result<InputAction, String> {
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .ok_or("missing 'op' (move|click|drag|scroll)")?;
    match op {
        "move" => Ok(InputAction::MouseMove {
            x: req_i32(args, "x")?,
            y: req_i32(args, "y")?,
            relative: args
                .get("relative")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "click" => {
            // Optional absolute target to move to first.
            let at = match (args.get("x"), args.get("y")) {
                (Some(_), Some(_)) => Some((req_i32(args, "x")?, req_i32(args, "y")?)),
                (None, None) => None,
                _ => return Err("click 'x' and 'y' must be provided together".to_string()),
            };
            Ok(InputAction::Click {
                button: parse_button(args)?,
                at,
            })
        }
        "drag" => Ok(InputAction::Drag {
            from: req_point(args, "from")?,
            to: req_point(args, "to")?,
            button: parse_button(args)?,
        }),
        "scroll" => Ok(InputAction::Scroll {
            dx: args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32,
            dy: args.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32,
        }),
        other => Err(format!("unknown mouse op '{other}'")),
    }
}

/// Rewrite a parsed mouse action whose coordinates are relative to
/// `window` (screenshot pixels) into absolute screen coordinates.
fn to_window_space(action: InputAction, window: &WindowInfo) -> Result<InputAction, String> {
    match action {
        InputAction::MouseMove { relative: true, .. } => {
            Err("'relative' moves can't be combined with a window target".to_string())
        }
        InputAction::MouseMove { x, y, .. } => {
            let (x, y) = window.to_screen(x, y)?;
            Ok(InputAction::MouseMove {
                x,
                y,
                relative: false,
            })
        }
        InputAction::Click { at: None, .. } => {
            Err("a click on a window target needs 'x' and 'y'".to_string())
        }
        InputAction::Click {
            button,
            at: Some((x, y)),
        } => Ok(InputAction::Click {
            button,
            at: Some(window.to_screen(x, y)?),
        }),
        InputAction::Drag { from, to, button } => Ok(InputAction::Drag {
            from: window.to_screen(from.0, from.1)?,
            to: window.to_screen(to.0, to.1)?,
            button,
        }),
        InputAction::Scroll { .. } => Err(
            "scroll has no coordinates: move into the window first, then scroll \
             without a window target"
                .to_string(),
        ),
        other => Ok(other),
    }
}

/// Turn keyboard `args` into an [`InputAction`]. Shape:
/// `{op:"type", text:"..."}` or `{op:"combo", keys:["ctrl","c"]}`.
fn parse_keyboard(args: &Value) -> Result<InputAction, String> {
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .ok_or("missing 'op' (type|combo)")?;
    match op {
        "type" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or("missing 'text' for type op")?;
            Ok(InputAction::TypeText {
                text: text.to_string(),
            })
        }
        "combo" => {
            let keys = args
                .get("keys")
                .and_then(Value::as_array)
                .ok_or("missing 'keys' array for combo op")?;
            let keys: Vec<String> = keys
                .iter()
                .map(|k| {
                    k.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "each key in 'keys' must be a string".to_string())
                })
                .collect::<Result<_, _>>()?;
            if keys.is_empty() {
                return Err("'keys' must not be empty".to_string());
            }
            Ok(InputAction::KeyCombo { keys })
        }
        other => Err(format!("unknown keyboard op '{other}'")),
    }
}

/// Run one parsed action through all the safety rails: preflight, make it
/// visible, perform it off the async runtime, and audit the outcome.
async fn run_action(
    capability: &str,
    backend: &Arc<dyn InputBackend>,
    indicator: &Arc<dyn Indicator>,
    ctx: &ExecContext,
    action: InputAction,
    config_path: Option<&Path>,
) -> Result<Value, String> {
    // 0. Instant kill-switch / disabled re-check. Re-read the on-disk config
    //    so a freshly-flipped kill-switch (or a capability the owner just
    //    turned off) takes effect on the NEXT action, not only after a daemon
    //    restart. The client also gates at dispatch against the startup
    //    config; this is the live re-check for the two highest-risk
    //    capabilities. A transient read failure falls through to that gate.
    if let Some(path) = config_path
        && let Ok(cfg) = Config::load_from(path)
        && !cfg.is_enabled(capability)
    {
        let msg = "capability disabled (kill-switch or config)".to_string();
        audit::record(capability, action.name(), false, json!({"error": &msg}));
        return Err(msg);
    }

    // 1. Preflight: refuse loud if input can't work here.
    if let Err(e) = backend.preflight() {
        audit::record(capability, action.name(), false, json!({"error": e}));
        return Err(e);
    }

    // 2. Visible: local OS indicator + a server/panel event, so the machine
    //    owner and the operator both see control is happening.
    indicator.announce(action.name());
    ctx.emit(
        "input-active",
        json!({"capability": capability, "action": action.name()}),
    )
    .await;

    // 3. Perform off the async runtime (enigo handle is !Send).
    let backend = backend.clone();
    let for_perform = action.clone();
    let result = tokio::task::spawn_blocking(move || backend.perform(&for_perform))
        .await
        .map_err(|e| format!("input task panicked: {e}"))?;

    // 4. Audit every attempt, success or failure.
    let ok = result.is_ok();
    let mut detail = action.audit_detail();
    if let (Some(obj), Err(e)) = (detail.as_object_mut(), &result) {
        obj.insert("error".to_string(), json!(e));
    }
    audit::record(capability, action.name(), ok, detail);

    result.map(|_| json!({"ok": true, "action": action.name()}))
}

/// Mouse control: move / click / drag / scroll. Coordinates are absolute
/// screen coordinates, or window-relative when the args carry a window
/// target (`window_id` / `app` / `title`) — the window's CURRENT bounds
/// are looked up per action, so a window that moved since the screenshot
/// is still hit correctly.
pub struct MouseExecutor {
    backend: Arc<dyn InputBackend>,
    indicator: Arc<dyn Indicator>,
    config_path: Option<PathBuf>,
    locator: Arc<dyn WindowLocator>,
}

impl MouseExecutor {
    pub fn new(
        backend: Arc<dyn InputBackend>,
        indicator: Arc<dyn Indicator>,
        config_path: Option<PathBuf>,
    ) -> Self {
        Self {
            backend,
            indicator,
            config_path,
            locator: Arc::new(XcapLocator),
        }
    }

    #[cfg(test)]
    fn with_locator(mut self, locator: Arc<dyn WindowLocator>) -> Self {
        self.locator = locator;
        self
    }
}

#[async_trait]
impl CapabilityExecutor for MouseExecutor {
    async fn execute(&self, ctx: &ExecContext, args: Value) -> Result<Value, String> {
        let mut action = parse_mouse(&args)?;
        let window = match WindowQuery::from_args(&args)? {
            Some(query) => {
                let locator = self.locator.clone();
                let window = tokio::task::spawn_blocking(move || locator.locate(&query))
                    .await
                    .map_err(|e| format!("window lookup panicked: {e}"))??;
                action = to_window_space(action, &window)?;
                Some(window)
            }
            None => None,
        };
        let mut out = run_action(
            "mouse",
            &self.backend,
            &self.indicator,
            ctx,
            action,
            self.config_path.as_deref(),
        )
        .await?;
        // Echo the resolved window so the server can tell a window-aware
        // agent from an old one that silently ignored the target.
        if let Some(window) = window {
            out["window"] = window.to_json();
        }
        Ok(out)
    }
}

/// Keyboard control: type text / key combos.
pub struct KeyboardExecutor {
    backend: Arc<dyn InputBackend>,
    indicator: Arc<dyn Indicator>,
    config_path: Option<PathBuf>,
}

impl KeyboardExecutor {
    pub fn new(
        backend: Arc<dyn InputBackend>,
        indicator: Arc<dyn Indicator>,
        config_path: Option<PathBuf>,
    ) -> Self {
        Self {
            backend,
            indicator,
            config_path,
        }
    }
}

#[async_trait]
impl CapabilityExecutor for KeyboardExecutor {
    async fn execute(&self, ctx: &ExecContext, args: Value) -> Result<Value, String> {
        let action = parse_keyboard(&args)?;
        run_action(
            "keyboard",
            &self.backend,
            &self.indicator,
            ctx,
            action,
            self.config_path.as_deref(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::tests_support::{collecting_ctx, drain};
    use crate::input::{MockBackend, NoopIndicator};
    use peckboard_agent_protocol::AgentFrame;

    fn mouse_with(backend: Arc<MockBackend>) -> MouseExecutor {
        MouseExecutor::new(backend, Arc::new(NoopIndicator), None)
    }
    fn keyboard_with(backend: Arc<MockBackend>) -> KeyboardExecutor {
        KeyboardExecutor::new(backend, Arc::new(NoopIndicator), None)
    }

    #[tokio::test]
    async fn mouse_window_target_maps_to_screen_and_echoes_window() {
        use crate::windows::tests_support::{FixedLocator, win};
        let backend = Arc::new(MockBackend::new());
        let locator = Arc::new(FixedLocator(vec![
            win(7, "code", "main.rs", 0, 0),
            win(9, "firefox", "Docs", 100, 50),
        ]));
        let exec = mouse_with(backend.clone()).with_locator(locator);
        let (ctx, _rx) = collecting_ctx();

        let out = exec
            .execute(&ctx, json!({"op":"click","x":10,"y":20,"app":"firefox"}))
            .await
            .unwrap();
        assert_eq!(out["window"]["window_id"], 9);
        exec.execute(
            &ctx,
            json!({"op":"drag","from":[0,0],"to":[5,5],"window_id":7}),
        )
        .await
        .unwrap();
        assert_eq!(
            backend.recorded(),
            vec![
                InputAction::Click {
                    button: MouseButton::Left,
                    at: Some((110, 70))
                },
                InputAction::Drag {
                    from: (0, 0),
                    to: (5, 5),
                    button: MouseButton::Left
                },
            ]
        );

        // Outside the window / no match / coordinate-less ops are refused
        // before the backend runs.
        for bad in [
            json!({"op":"click","x":900,"y":0,"app":"firefox"}),
            json!({"op":"click","x":1,"y":1,"app":"slack"}),
            json!({"op":"scroll","dy":1,"app":"firefox"}),
        ] {
            assert!(exec.execute(&ctx, bad).await.is_err());
        }
        assert_eq!(backend.recorded().len(), 2);
    }

    #[tokio::test]
    async fn mouse_move_absolute_and_relative() {
        let backend = Arc::new(MockBackend::new());
        let exec = mouse_with(backend.clone());
        let (ctx, _rx) = collecting_ctx();

        exec.execute(&ctx, json!({"op":"move","x":100,"y":200}))
            .await
            .unwrap();
        exec.execute(&ctx, json!({"op":"move","x":-5,"y":7,"relative":true}))
            .await
            .unwrap();

        let recorded = backend.recorded();
        assert_eq!(
            recorded[0],
            InputAction::MouseMove {
                x: 100,
                y: 200,
                relative: false
            }
        );
        assert_eq!(
            recorded[1],
            InputAction::MouseMove {
                x: -5,
                y: 7,
                relative: true
            }
        );
    }

    #[tokio::test]
    async fn mouse_click_button_and_target() {
        let backend = Arc::new(MockBackend::new());
        let exec = mouse_with(backend.clone());
        let (ctx, _rx) = collecting_ctx();

        exec.execute(&ctx, json!({"op":"click","button":"right","x":10,"y":20}))
            .await
            .unwrap();

        assert_eq!(
            backend.recorded()[0],
            InputAction::Click {
                button: MouseButton::Right,
                at: Some((10, 20))
            }
        );
    }

    #[tokio::test]
    async fn mouse_drag_records_endpoints() {
        let backend = Arc::new(MockBackend::new());
        let exec = mouse_with(backend.clone());
        let (ctx, _rx) = collecting_ctx();

        exec.execute(&ctx, json!({"op":"drag","from":[1,2],"to":[3,4]}))
            .await
            .unwrap();

        assert_eq!(
            backend.recorded()[0],
            InputAction::Drag {
                from: (1, 2),
                to: (3, 4),
                button: MouseButton::Left
            }
        );
    }

    #[tokio::test]
    async fn mouse_scroll_defaults_axes_to_zero() {
        let backend = Arc::new(MockBackend::new());
        let exec = mouse_with(backend.clone());
        let (ctx, _rx) = collecting_ctx();

        exec.execute(&ctx, json!({"op":"scroll","dy":3}))
            .await
            .unwrap();

        assert_eq!(backend.recorded()[0], InputAction::Scroll { dx: 0, dy: 3 });
    }

    #[tokio::test]
    async fn keyboard_type_and_combo() {
        let backend = Arc::new(MockBackend::new());
        let exec = keyboard_with(backend.clone());
        let (ctx, _rx) = collecting_ctx();

        exec.execute(&ctx, json!({"op":"type","text":"hello"}))
            .await
            .unwrap();
        exec.execute(&ctx, json!({"op":"combo","keys":["ctrl","c"]}))
            .await
            .unwrap();

        let recorded = backend.recorded();
        assert_eq!(
            recorded[0],
            InputAction::TypeText {
                text: "hello".into()
            }
        );
        assert_eq!(
            recorded[1],
            InputAction::KeyCombo {
                keys: vec!["ctrl".into(), "c".into()]
            }
        );
    }

    #[tokio::test]
    async fn bad_args_are_rejected_before_backend() {
        let backend = Arc::new(MockBackend::new());
        let exec = mouse_with(backend.clone());
        let (ctx, _rx) = collecting_ctx();

        let err = exec.execute(&ctx, json!({"op":"nope"})).await.unwrap_err();
        assert!(err.contains("unknown mouse op"), "got: {err}");
        assert!(backend.recorded().is_empty(), "backend must not run");
    }

    #[tokio::test]
    async fn preflight_failure_blocks_and_is_not_silent() {
        let backend = Arc::new(MockBackend {
            preflight_error: Some("input synthesis is unavailable under Wayland".into()),
            ..Default::default()
        });
        let exec = mouse_with(backend.clone());
        let (ctx, _rx) = collecting_ctx();

        let err = exec
            .execute(&ctx, json!({"op":"move","x":1,"y":1}))
            .await
            .unwrap_err();
        assert!(err.contains("Wayland"), "got: {err}");
        assert!(
            backend.recorded().is_empty(),
            "must not perform on preflight fail"
        );
    }

    #[tokio::test]
    async fn emits_input_active_event_for_visibility() {
        let backend = Arc::new(MockBackend::new());
        let exec = mouse_with(backend.clone());
        let (ctx, mut rx) = collecting_ctx();

        exec.execute(&ctx, json!({"op":"move","x":1,"y":1}))
            .await
            .unwrap();

        let events = drain(&mut rx);
        assert!(
            events.iter().any(|f| matches!(
                f,
                AgentFrame::Event { kind, .. } if kind == "input-active"
            )),
            "expected an input-active event, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn kill_switch_in_config_refuses_per_action() {
        // Flip the kill-switch on disk AFTER the executor is built, proving
        // the per-action re-read honours it without a restart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = Config::default();
        cfg.capabilities.insert("mouse".into(), true);
        cfg.kill_switch = true;
        cfg.save_to(&path).unwrap();

        let backend = Arc::new(MockBackend::new());
        let exec = MouseExecutor::new(backend.clone(), Arc::new(NoopIndicator), Some(path));
        let (ctx, _rx) = collecting_ctx();

        let err = exec
            .execute(&ctx, json!({"op":"move","x":1,"y":1}))
            .await
            .unwrap_err();
        assert!(err.contains("kill-switch"), "got: {err}");
        assert!(
            backend.recorded().is_empty(),
            "kill-switch must block before perform"
        );
    }
}
