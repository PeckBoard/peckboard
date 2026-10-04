//! Native half of the PeckBoard mobile app.
//!
//! - **Secure storage** for pairing secrets: iOS Keychain
//!   (`AfterFirstUnlockThisDeviceOnly`, never synced) / an Android Keystore
//!   AES-GCM key wrapping values in app-private prefs / macOS Keychain and
//!   Windows Credential Manager (`keyring`). Linux: a dev-only 0600 file
//!   (Linux desktop is not shipped).
//! - **WebView config**, applied natively when the WebView loads: media
//!   autoplay without a gesture, mic capture auto-granted for the loopback
//!   origin only, back-swipe on iOS.
//! - **Lifecycle**: foreground/background notifications so the core can stop
//!   the tunnel while backgrounded; on desktop, a wake from sleep.
//!
//! Nothing here is callable from JavaScript (`COMMANDS` is empty in
//! `build.rs`); only the app's Rust core uses [`PeckboardNativeExt`].

use serde::Deserialize;
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
