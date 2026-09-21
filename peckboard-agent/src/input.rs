//! Input synthesis backend (mouse + keyboard) and the "this machine is
//! being controlled" indicator, both behind traits so tests never touch
//! real hardware and the real cursor never moves in CI.
//!
//! # Why a trait
//!
//! [`enigo::Enigo`] is `!Send`/`!Sync` on some platforms and drives real
//! OS input. We hide it behind [`InputBackend`] so:
//!   * unit tests use [`MockBackend`], which only records the actions it
//!     was asked to perform — nothing moves;
//!   * the real [`EnigoBackend`] constructs a fresh `Enigo` inside each
//!     synchronous `perform` call (driven from `spawn_blocking` by the
//!     executor), so the non-`Send` handle never crosses an `.await`.
//!
//! # Safety rails living here
//!   * [`InputBackend::preflight`] fails LOUD on Wayland (enigo cannot
//!     reliably synthesize input there) rather than silently no-op'ing,
//!     and surfaces macOS Accessibility guidance.
//!   * [`Indicator`] makes input activity VISIBLE on the controlled
//!     machine (console banner + best-effort OS notification).
//!
//! OFF-by-default and the global kill-switch are enforced upstream in
//! `config.rs`/`client.rs` before an executor is ever reached.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The three mouse buttons we expose. Kept provider-agnostic so the wire
/// contract doesn't leak `enigo` types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// A single synthesized input action. Composite gestures (a drag is
/// press → move → release) are modelled as ONE action so they audit and
/// announce as one unit and run atomically inside a single backend call.
#[derive(Debug, Clone, PartialEq)]
pub enum InputAction {
    /// Move the cursor. `relative` picks delta vs absolute coordinates.
    MouseMove { x: i32, y: i32, relative: bool },
    /// Click a button, optionally moving to `at` (absolute) first.
    Click {
        button: MouseButton,
        at: Option<(i32, i32)>,
    },
    /// Press at `from`, move to `to`, release — a drag.
    Drag {
        from: (i32, i32),
        to: (i32, i32),
        button: MouseButton,
    },
    /// Scroll by `dx` (horizontal) and `dy` (vertical) steps.
    Scroll { dx: i32, dy: i32 },
    /// Type literal text (unicode).
    TypeText { text: String },
    /// A key combo, e.g. `["ctrl","c"]`: all but the last are held as
    /// modifiers while the last is clicked, then modifiers released.
    KeyCombo { keys: Vec<String> },
}

impl InputAction {
    /// Coarse action name for the audit log / indicator (never includes
    /// typed contents or key identities — those are sensitive).
    pub fn name(&self) -> &'static str {
        match self {
            InputAction::MouseMove { .. } => "move",
            InputAction::Click { .. } => "click",
            InputAction::Drag { .. } => "drag",
            InputAction::Scroll { .. } => "scroll",
            InputAction::TypeText { .. } => "type",
            InputAction::KeyCombo { .. } => "combo",
        }
    }
}

impl InputAction {
    /// Structured detail for the audit log. PRIVACY: typed text contents
    /// are NEVER logged (could be passwords) — only the character count.
    /// Mouse geometry and combo key names ARE logged: useful for a
    /// security audit and carry no secret payload.
    pub fn audit_detail(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            InputAction::MouseMove { x, y, relative } => {
                json!({"x": x, "y": y, "relative": relative})
            }
            InputAction::Click { button, at } => json!({"button": button.name(), "at": at}),
            InputAction::Drag { from, to, button } => {
                json!({"from": from, "to": to, "button": button.name()})
            }
            InputAction::Scroll { dx, dy } => json!({"dx": dx, "dy": dy}),
            InputAction::TypeText { text } => json!({"len": text.chars().count()}),
            InputAction::KeyCombo { keys } => json!({"keys": keys}),
        }
    }
}

impl MouseButton {
    /// Lowercase name for the audit log / arg parsing.
    pub fn name(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        }
    }
}

/// A synthesizer of OS input. Synchronous on purpose: the real impl holds
/// a non-`Send` handle, so callers drive it from `spawn_blocking`.
pub trait InputBackend: Send + Sync {
    /// Fail LOUD if input synthesis can't work on this machine right now
    /// (Wayland, missing macOS Accessibility permission). Returns a
    /// human-readable, actionable message on failure — never a silent
    /// no-op, which would look like the action worked when it didn't.
    fn preflight(&self) -> Result<(), String>;

    /// Perform one [`InputAction`]. Returns a human-readable error on
    /// failure.
    fn perform(&self, action: &InputAction) -> Result<(), String>;
}

/// Records requested actions without touching hardware. Test-only sink.
#[cfg(test)]
#[derive(Default)]
pub struct MockBackend {
    /// Every action passed to [`perform`](InputBackend::perform), in order.
    pub actions: Mutex<Vec<InputAction>>,
    /// When set, [`preflight`](InputBackend::preflight) returns this error.
    pub preflight_error: Option<String>,
}

#[cfg(test)]
impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of recorded actions.
    pub fn recorded(&self) -> Vec<InputAction> {
        self.actions.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl InputBackend for MockBackend {
    fn preflight(&self) -> Result<(), String> {
        match &self.preflight_error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn perform(&self, action: &InputAction) -> Result<(), String> {
        self.actions.lock().unwrap().push(action.clone());
        Ok(())
    }
}

/// Makes input activity visible on the controlled machine. The machine
/// owner should never be surprised that something is driving their mouse
/// or keyboard.
pub trait Indicator: Send + Sync {
    /// Announce that `action` (a coarse name like `"click"`) is happening.
    fn announce(&self, action: &str);
}

/// No-op indicator for tests.
#[derive(Default)]
#[cfg(test)]
pub struct NoopIndicator;

#[cfg(test)]
impl Indicator for NoopIndicator {
    fn announce(&self, _action: &str) {}
}

/// Real indicator: a WARN-level console banner on every action plus a
/// best-effort, debounced OS notification. Both are advisory — a persistent
/// floating overlay needs a GUI toolkit the daemon deliberately doesn't
/// carry, so this is the honest v1: the daemon's own console always shows
/// it, and the desktop gets a transient toast at most once per
/// [`NOTIFY_DEBOUNCE`].
pub struct ConsoleIndicator {
    last_notify: Mutex<Option<Instant>>,
}

/// Don't spam desktop notifications during a burst of actions.
const NOTIFY_DEBOUNCE: Duration = Duration::from_secs(10);

impl Default for ConsoleIndicator {
    fn default() -> Self {
        Self {
            last_notify: Mutex::new(None),
        }
    }
}

impl ConsoleIndicator {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if enough time has passed since the last desktop notification
    /// to fire another (or none has fired yet).
    fn should_notify(&self) -> bool {
        let mut guard = self.last_notify.lock().unwrap();
        let now = Instant::now();
        let due = guard
            .map(|t| now.duration_since(t) >= NOTIFY_DEBOUNCE)
            .unwrap_or(true);
        if due {
            *guard = Some(now);
        }
        due
    }
}

impl Indicator for ConsoleIndicator {
    fn announce(&self, action: &str) {
        // Always visible in the daemon's own console/log.
        tracing::warn!(target: "indicator", "peckboard-agent is controlling this machine ({action})");
        if self.should_notify() {
            notify_os("peckboard-agent is controlling this machine");
        }
    }
}

/// Best-effort native desktop notification. Shells the platform notifier;
/// any failure (tool missing, no display) is intentionally ignored — the
/// console banner is the guaranteed channel, this is the nice-to-have.
fn notify_os(msg: &str) {
    use std::process::Stdio;
    let mut cmd = platform_notify_command(msg);
    if let Some(mut c) = cmd.take() {
        let _ = c.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}

/// The platform-specific notifier invocation, or `None` if we don't have
/// one for this OS.
fn platform_notify_command(msg: &str) -> Option<std::process::Command> {
    use std::process::Command;
    #[cfg(target_os = "linux")]
    {
        let mut c = Command::new("notify-send");
        c.arg("Peckboard").arg(msg);
        Some(c)
    }
    #[cfg(target_os = "macos")]
    {
        let mut c = Command::new("osascript");
        c.arg("-e").arg(format!(
            "display notification \"{msg}\" with title \"Peckboard\""
        ));
        Some(c)
    }
    #[cfg(target_os = "windows")]
    {
        let mut c = Command::new("powershell");
        c.arg("-NoProfile").arg("-Command").arg(format!(
            "[void][System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms'); \
             [System.Windows.Forms.MessageBox]::Show('{msg}')"
        ));
        Some(c)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = msg;
        None::<Command>
    }
}

// ---------------------------------------------------------------------------
// Real enigo-backed synthesizer.
// ---------------------------------------------------------------------------

/// Drives real OS input via [`enigo`]. Constructs a fresh `Enigo` per
/// `perform` so the non-`Send` handle never outlives a single call.
#[derive(Default)]
pub struct EnigoBackend;

impl EnigoBackend {
    pub fn new() -> Self {
        Self
    }
}

impl InputBackend for EnigoBackend {
    fn preflight(&self) -> Result<(), String> {
        // Wayland: enigo cannot reliably synthesize input under most Wayland
        // compositors. Fail loud with guidance instead of a silent no-op.
        #[cfg(target_os = "linux")]
        {
            let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
            let wayland_display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
            if session.eq_ignore_ascii_case("wayland") || !wayland_display.is_empty() {
                return Err(
                    "input synthesis is unavailable under Wayland (enigo limitation). \
                     Log in to an X11/Xorg session to use mouse/keyboard control."
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    fn perform(&self, action: &InputAction) -> Result<(), String> {
        enigo_perform(action)
    }
}

/// Map an enigo error to a user-facing string, adding macOS Accessibility
/// guidance since that's the overwhelmingly common cause of failure there.
fn describe_err(context: &str, e: impl std::fmt::Display) -> String {
    #[cfg(target_os = "macos")]
    {
        return format!(
            "{context}: {e}. On macOS, grant Accessibility permission: \
             System Settings → Privacy & Security → Accessibility, enable peckboard-agent."
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!("{context}: {e}")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn enigo_perform(action: &InputAction) -> Result<(), String> {
    use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};

    fn to_button(b: MouseButton) -> Button {
        match b {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
            MouseButton::Middle => Button::Middle,
        }
    }

    /// Map a key name to an enigo `Key`. Single characters become
    /// `Key::Unicode`; named keys map to their special key.
    fn to_key(name: &str) -> Result<Key, String> {
        let lower = name.to_ascii_lowercase();
        let key = match lower.as_str() {
            "ctrl" | "control" => Key::Control,
            "shift" => Key::Shift,
            "alt" | "option" => Key::Alt,
            "meta" | "cmd" | "command" | "super" | "win" => Key::Meta,
            "enter" | "return" => Key::Return,
            "tab" => Key::Tab,
            "esc" | "escape" => Key::Escape,
            "space" => Key::Space,
            "backspace" => Key::Backspace,
            "delete" | "del" => Key::Delete,
            "up" => Key::UpArrow,
            "down" => Key::DownArrow,
            "left" => Key::LeftArrow,
            "right" => Key::RightArrow,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" => Key::PageUp,
            "pagedown" => Key::PageDown,
            "f1" => Key::F1,
            "f2" => Key::F2,
            "f3" => Key::F3,
            "f4" => Key::F4,
            "f5" => Key::F5,
            "f6" => Key::F6,
            "f7" => Key::F7,
            "f8" => Key::F8,
            "f9" => Key::F9,
            "f10" => Key::F10,
            "f11" => Key::F11,
            "f12" => Key::F12,
            other => {
                let mut chars = other.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Key::Unicode(c),
                    _ => return Err(format!("unknown key '{name}'")),
                }
            }
        };
        Ok(key)
    }

    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| describe_err("could not initialize input backend", e))?;

    match action {
        InputAction::MouseMove { x, y, relative } => {
            let coord = if *relative {
                Coordinate::Rel
            } else {
                Coordinate::Abs
            };
            enigo
                .move_mouse(*x, *y, coord)
                .map_err(|e| describe_err("mouse move failed", e))?;
        }
        InputAction::Click { button, at } => {
            if let Some((x, y)) = at {
                enigo
                    .move_mouse(*x, *y, Coordinate::Abs)
                    .map_err(|e| describe_err("mouse move failed", e))?;
            }
            enigo
                .button(to_button(*button), Direction::Click)
                .map_err(|e| describe_err("click failed", e))?;
        }
        InputAction::Drag { from, to, button } => {
            let btn = to_button(*button);
            enigo
                .move_mouse(from.0, from.1, Coordinate::Abs)
                .map_err(|e| describe_err("drag move-to-start failed", e))?;
            enigo
                .button(btn, Direction::Press)
                .map_err(|e| describe_err("drag press failed", e))?;
            enigo
                .move_mouse(to.0, to.1, Coordinate::Abs)
                .map_err(|e| describe_err("drag move failed", e))?;
            enigo
                .button(btn, Direction::Release)
                .map_err(|e| describe_err("drag release failed", e))?;
        }
        InputAction::Scroll { dx, dy } => {
            if *dx != 0 {
                enigo
                    .scroll(*dx, Axis::Horizontal)
                    .map_err(|e| describe_err("scroll failed", e))?;
            }
            if *dy != 0 {
                enigo
                    .scroll(*dy, Axis::Vertical)
                    .map_err(|e| describe_err("scroll failed", e))?;
            }
        }
        InputAction::TypeText { text } => {
            enigo
                .text(text)
                .map_err(|e| describe_err("type text failed", e))?;
        }
        InputAction::KeyCombo { keys } => {
            if keys.is_empty() {
                return Err("key combo needs at least one key".to_string());
            }
            let mapped: Vec<Key> = keys.iter().map(|k| to_key(k)).collect::<Result<_, _>>()?;
            let (last, mods) = mapped.split_last().unwrap();
            // Hold modifiers, click the terminal key, release in reverse.
            for m in mods {
                enigo
                    .key(*m, Direction::Press)
                    .map_err(|e| describe_err("modifier press failed", e))?;
            }
            let click = enigo.key(*last, Direction::Click);
            for m in mods.iter().rev() {
                let _ = enigo.key(*m, Direction::Release);
            }
            click.map_err(|e| describe_err("key press failed", e))?;
        }
    }
    Ok(())
}

/// Fallback for targets enigo doesn't support: fail loud, never no-op.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn enigo_perform(_action: &InputAction) -> Result<(), String> {
    Err("input synthesis is not supported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_records_actions_in_order() {
        let b = MockBackend::new();
        b.perform(&InputAction::MouseMove {
            x: 10,
            y: 20,
            relative: false,
        })
        .unwrap();
        b.perform(&InputAction::Click {
            button: MouseButton::Left,
            at: None,
        })
        .unwrap();
        let got = b.recorded();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name(), "move");
        assert_eq!(got[1].name(), "click");
    }

    #[test]
    fn mock_preflight_error_is_returned() {
        let b = MockBackend {
            preflight_error: Some("wayland".into()),
            ..Default::default()
        };
        assert!(b.preflight().unwrap_err().contains("wayland"));
    }

    #[test]
    fn action_names_are_stable() {
        assert_eq!(InputAction::Scroll { dx: 0, dy: 3 }.name(), "scroll");
        assert_eq!(InputAction::TypeText { text: "hi".into() }.name(), "type");
        assert_eq!(
            InputAction::KeyCombo {
                keys: vec!["ctrl".into(), "c".into()]
            }
            .name(),
            "combo"
        );
    }

    #[test]
    fn indicator_debounces_notifications() {
        let ind = ConsoleIndicator::new();
        assert!(ind.should_notify(), "first should fire");
        assert!(!ind.should_notify(), "second within window suppressed");
    }
}
