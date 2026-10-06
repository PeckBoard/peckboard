//! Native half of the PeckBoard mobile app.
//!
//! - **Secure storage** for pairing secrets: iOS Keychain
//!   (`AfterFirstUnlockThisDeviceOnly`, never synced) / an Android Keystore
//!   AES-GCM key wrapping values in app-private prefs / macOS Keychain and
//!   Windows Credential Manager (`keyring`). Linux: a dev-only 0600 file
//!   (Linux desktop is not shipped).
//! - **WebView config**, applied natively when the WebView loads: media
//!   autoplay without a gesture, back-swipe on iOS.
//! - **Microphone**: capture is granted only to the active box's loopback
//!   origin, from its main frame, after the user allowed it once for that
//!   box ([`Native::set_mic_policy`] / [`Native::watch_mic_decisions`]).
//! - **Website data**: a removed box's loopback origin is cleared as far as
//!   the platform allows ([`Native::clear_site_data`]).
//! - **Lifecycle**: foreground/background notifications so the core can stop
//!   the tunnel while backgrounded; on desktop, a wake from sleep.
//! - **Network**: default-network changes (Wi-Fi ↔ cellular) on iOS and
//!   Android, so the core reconnects the tunnel at once.
//! - **App lock**: which biometric the device offers
//!   ([`Native::biometric_kind`]), a system biometric prompt
//!   ([`Native::authenticate`]), and a privacy cover that hides the box page
//!   in the app switcher while the app is backgrounded
//!   ([`Native::set_privacy_cover`] / [`Native::lower_privacy_cover`]). The
//!   lock itself (code / pattern, backoff, gating) lives in the app core.
//!
//! Nothing here is callable from JavaScript (`COMMANDS` is empty in
//! `build.rs`); only the app's Rust core uses [`PeckboardNativeExt`].

use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};

#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

#[cfg(desktop)]
pub use desktop::Native;
#[cfg(mobile)]
pub use mobile::Native;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("native: {0}")]
    Native(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(mobile)]
impl From<tauri::plugin::mobile::PluginInvokeError> for Error {
    fn from(e: tauri::plugin::mobile::PluginInvokeError) -> Self {
        Error::Native(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// App visibility as reported by the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Foreground,
    Background,
}

/// The default network changed (iOS `NWPathMonitor`, Android default-network
/// callback). `detail` is for logs only.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct NetworkChange {
    #[serde(default)]
    pub detail: String,
}

/// Who may capture the microphone right now: the active box's UI at
/// `origin` (`http://127.0.0.1:<port>`), and only if the user allowed it for
/// that box. `allowed: None` means not asked yet — the native side then asks
/// "Allow <box_name> to use the microphone?" once and reports the answer
/// through [`Native::watch_mic_decisions`]. No policy (`None` in
/// [`Native::set_mic_policy`]) denies every request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicPolicy {
    pub origin: String,
    pub box_id: String,
    pub box_name: String,
    pub allowed: Option<bool>,
}

/// The user's answer to the microphone prompt for a box.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicDecision {
    pub box_id: String,
    pub allowed: bool,
}

/// The biometric the OS can verify right now (enrolled and not locked out).
/// `None` also covers "hardware present but nothing enrolled".
/// `Biometric` is Android's generic class (BiometricPrompt doesn't say
/// whether it will use a fingerprint or a face).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BiometricKind {
    None,
    FaceId,
    TouchId,
    OpticId,
    Biometric,
    Hello,
}

/// Outcome of [`Native::authenticate`]. `Cancelled` = the user (or the
/// system) dismissed the prompt; `Unavailable` = no usable biometric
/// (not enrolled, locked out, changed). Neither is a failed attempt for the
/// lock's backoff — only the code / pattern counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthOutcome {
    Success,
    Cancelled,
    Unavailable,
}

pub trait PeckboardNativeExt<R: Runtime> {
    fn native(&self) -> &Native<R>;
}

impl<R: Runtime, T: Manager<R>> PeckboardNativeExt<R> for T {
    fn native(&self) -> &Native<R> {
        self.state::<Native<R>>().inner()
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("peckboard-native")
        .setup(|app, api| {
            #[cfg(mobile)]
            let native = mobile::init(app, api)?;
            #[cfg(desktop)]
            let native = desktop::init(app, api)?;
            app.manage(native);
            Ok(())
        })
        .build()
}
