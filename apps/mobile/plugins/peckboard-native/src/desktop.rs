//! Desktop (macOS / Windows / Linux) half of the native plugin.
//!
//! - **Secrets**: macOS Keychain / Windows Credential Manager via the
//!   `keyring` crate, service = the bundle id. A `dev-secrets.json` left by
//!   older dev builds is imported once and deleted. Linux keeps a 0600 JSON
//!   file in the app data dir — NOT secure storage; Linux is a dev /
//!   UI-iteration target only and is not shipped.
//! - **Lifecycle**: desktop windows aren't suspended, but the machine
//!   sleeps. A wake (wall clock jumped well past a short tick) is reported
//!   as `Background` then `Foreground`, so the core restarts the tunnel the
//!   same way it does when a phone app comes back.
//! - **Network**: not watched. Desktops change networks rarely and mostly
//!   across a sleep (the wake check covers that); a dependency-free
//!   portable default-route monitor doesn't exist, so a live switch falls
//!   back to the tunnel's ping timeout.
//! - **Website data / history / microphone**: no native hooks. The app
//!   clears a removed box's data with Tauri's `clear_all_browsing_data`
//!   (profile-wide on WebView2, all WebKit data on macOS — there is no
//!   per-origin API on either); history navigations go through the
//!   allow-list on desktop; microphone prompts are the platform WebView's
//!   own (per origin, no per-box memory).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use tauri::plugin::PluginApi;
use tauri::{AppHandle, Manager, Runtime};

use crate::{AuthOutcome, BiometricKind, Lifecycle, MicDecision, MicPolicy, NetworkChange};

/// Where pairing secrets live on this platform.
trait Backend: Send + Sync {
    fn get(&self, key: &str) -> crate::Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> crate::Result<()>;
    fn delete(&self, key: &str) -> crate::Result<()>;
}

/// JSON map in a 0600 file: Linux dev store, and the legacy store that
/// macOS / Windows import from.
struct FileStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileStore {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    fn load(&self) -> crate::Result<BTreeMap<String, String>> {
        match std::fs::read(&self.path) {
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, map: &BTreeMap<String, String>) -> crate::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&self.path)?;
        std::io::Write::write_all(&mut f, &serde_json::to_vec(map)?)?;
        Ok(())
    }
}

impl Backend for FileStore {
    fn get(&self, key: &str) -> crate::Result<Option<String>> {
        let _g = self.lock.lock().unwrap();
        Ok(self.load()?.remove(key))
    }

    fn set(&self, key: &str, value: &str) -> crate::Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut map = self.load()?;
        map.insert(key.to_string(), value.to_string());
        self.save(&map)
    }

    fn delete(&self, key: &str) -> crate::Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut map = self.load()?;
        if map.remove(key).is_some() {
            self.save(&map)?;
        }
        Ok(())
    }
}

/// macOS Keychain / Windows Credential Manager, one generic password per
/// key under `service`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
struct Keyring {
    service: String,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Keyring {
    fn entry(&self, key: &str) -> crate::Result<keyring::Entry> {
        keyring::Entry::new(&self.service, key).map_err(keyring_err)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn keyring_err(e: keyring::Error) -> crate::Error {
    crate::Error::Native(format!("keyring: {e}"))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Backend for Keyring {
    fn get(&self, key: &str) -> crate::Result<Option<String>> {
        match self.entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_err(e)),
        }
    }

    fn set(&self, key: &str, value: &str) -> crate::Result<()> {
        self.entry(key)?.set_password(value).map_err(keyring_err)
    }

    fn delete(&self, key: &str) -> crate::Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keyring_err(e)),
        }
    }
}

/// Import every entry of the legacy file store into `to`, then delete the
/// file. On any failure the file is kept (and the import retried on the
/// next launch); entries already copied are simply written again.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn migrate_legacy(legacy: &FileStore, to: &dyn Backend) -> crate::Result<usize> {
    let map = legacy.load()?;
    if map.is_empty() && !legacy.path.exists() {
        return Ok(0);
    }
    for (k, v) in &map {
        to.set(k, v)?;
    }
    std::fs::remove_file(&legacy.path)?;
    Ok(map.len())
}

pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Native<R>> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| crate::Error::Native(e.to_string()))?;
    let file = FileStore::new(dir.join("dev-secrets.json"));
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let backend: Box<dyn Backend> = {
        let keyring = Keyring {
            service: app.config().identifier.clone(),
        };
        match migrate_legacy(&file, &keyring) {
            Ok(0) => {}
            Ok(n) => log::info!("moved {n} secret(s) from dev-secrets.json to the keychain"),
            Err(e) => log::warn!("dev-secrets.json import failed, retrying next launch: {e}"),
        }
        Box::new(keyring)
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let backend: Box<dyn Backend> = Box::new(file);
    Ok(Native {
        backend,
        #[cfg(target_os = "windows")]
        app: app.clone(),
        _r: std::marker::PhantomData,
    })
}

pub struct Native<R: Runtime> {
    backend: Box<dyn Backend>,
    /// Windows Hello parents its prompt to the main window.
    #[cfg(target_os = "windows")]
    app: AppHandle<R>,
    _r: std::marker::PhantomData<fn() -> R>,
}

/// Wall-clock check interval for wake detection.
const WAKE_TICK: Duration = Duration::from_secs(5);
/// A tick that took this long means the machine slept in between.
const WAKE_GAP: Duration = Duration::from_secs(30);

impl<R: Runtime> Native<R> {
    pub fn secret_get(&self, key: &str) -> crate::Result<Option<String>> {
        self.backend.get(key)
    }

    pub fn secret_set(&self, key: &str, value: &str) -> crate::Result<()> {
        self.backend.set(key, value)
    }

    pub fn secret_delete(&self, key: &str) -> crate::Result<()> {
        self.backend.delete(key)
    }

    /// Reports a wake from sleep as `Background` + `Foreground`: the
    /// tunnel's sockets and NAT mappings are stale after a sleep (often on
    /// another network), and waiting for QUIC's idle timeout plus backoff
    /// leaves the box page dead for a while. Monotonic clocks stop during
    /// sleep on macOS and Linux, so the wall clock is what shows the gap.
    pub fn watch_lifecycle(
        &self,
        f: impl Fn(Lifecycle) + Send + Sync + 'static,
    ) -> crate::Result<()> {
        std::thread::Builder::new()
            .name("peckboard-wake".into())
            .spawn(move || {
                let mut last = SystemTime::now();
                loop {
                    std::thread::sleep(WAKE_TICK);
                    let now = SystemTime::now();
                    let gap = now.duration_since(last).unwrap_or_default();
                    last = now;
                    if gap >= WAKE_GAP {
                        log::info!("wake detected ({}s gap)", gap.as_secs());
                        f(Lifecycle::Background);
                        f(Lifecycle::Foreground);
                    }
                }
            })?;
        Ok(())
    }

    /// No-op on desktop (see the module docs); `f` is never called.
    pub fn watch_network(
        &self,
        _f: impl Fn(NetworkChange) + Send + Sync + 'static,
    ) -> crate::Result<()> {
        Ok(())
    }

    /// No-op on desktop: no per-origin API (see the module docs); the app
    /// uses Tauri's `clear_all_browsing_data` when the last box goes.
    pub fn clear_site_data(&self, _origin: &str, _host_wide: bool) -> crate::Result<()> {
        Ok(())
    }

    /// No-op on desktop: history navigations pass the allow-list there.
    pub fn clear_history(&self) -> crate::Result<()> {
        Ok(())
    }

    /// No-op on desktop (see the module docs).
    pub fn set_mic_policy(&self, _policy: Option<&MicPolicy>) -> crate::Result<()> {
        Ok(())
    }

    /// No-op on desktop; `f` is never called.
    pub fn watch_mic_decisions(
        &self,
        _f: impl Fn(MicDecision) + Send + Sync + 'static,
    ) -> crate::Result<()> {
        Ok(())
    }

    /// Touch ID (macOS) / Windows Hello; `None` elsewhere.
    #[allow(unreachable_code)]
    pub fn biometric_kind(&self) -> crate::Result<BiometricKind> {
        #[cfg(target_os = "macos")]
        return Ok(macos::biometric_kind());
        #[cfg(target_os = "windows")]
        return Ok(windows_hello::biometric_kind());
        Ok(BiometricKind::None)
    }

    /// Shows the system biometric prompt and blocks until it is answered.
    #[allow(unreachable_code, unused_variables)]
    pub fn authenticate(&self, reason: &str) -> crate::Result<AuthOutcome> {
        #[cfg(target_os = "macos")]
        return Ok(macos::authenticate(reason));
        #[cfg(target_os = "windows")]
        return Ok(windows_hello::authenticate(self.main_hwnd(), reason));
        Ok(AuthOutcome::Unavailable)
    }

    /// The main window's handle, to parent the Windows Hello prompt.
    #[cfg(target_os = "windows")]
    fn main_hwnd(&self) -> Option<windows::Win32::Foundation::HWND> {
        let win = self.app.get_webview_window("main")?;
        // Through the raw pointer: tauri's `HWND` may come from another
        // `windows` release than ours.
        win.hwnd()
            .ok()
            .map(|h| windows::Win32::Foundation::HWND(h.0))
    }

    /// No-op on desktop: no app-switcher snapshot to hide; the shell's lock
    /// screen covers the box page.
    pub fn set_privacy_cover(&self, _armed: bool) -> crate::Result<()> {
        Ok(())
    }

    /// No-op on desktop (see [`Self::set_privacy_cover`]).
    pub fn lower_privacy_cover(&self) -> crate::Result<()> {
        Ok(())
    }
}

/// Touch ID through `LAContext` (`deviceOwnerAuthenticationWithBiometrics`):
/// biometrics only, no system-password fallback — the app's own code or
/// pattern is the fallback.
#[cfg(target_os = "macos")]
mod macos {
    use std::sync::mpsc;

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString};
    use objc2_local_authentication::{LAContext, LAPolicy};

    use crate::{AuthOutcome, BiometricKind};

    // `LAError` codes (LAError.h).
    const USER_CANCEL: isize = -2;
    const USER_FALLBACK: isize = -3;
    const SYSTEM_CANCEL: isize = -4;
    const APP_CANCEL: isize = -9;

    const POLICY: LAPolicy = LAPolicy::DeviceOwnerAuthenticationWithBiometrics;

    /// Usable now: hardware present, a finger enrolled, not locked out.
    pub fn biometric_kind() -> BiometricKind {
        let ctx = unsafe { LAContext::new() };
        match unsafe { ctx.canEvaluatePolicy_error(POLICY) } {
            Ok(()) => BiometricKind::TouchId,
            Err(_) => BiometricKind::None,
        }
    }

    /// Blocks on the reply block, which LocalAuthentication calls on a
    /// private queue (never the caller's thread).
    pub fn authenticate(reason: &str) -> AuthOutcome {
        let ctx = unsafe { LAContext::new() };
        if unsafe { ctx.canEvaluatePolicy_error(POLICY) }.is_err() {
            return AuthOutcome::Unavailable;
        }
        // Empty title hides the "Use Password…" button.
        unsafe { ctx.setLocalizedFallbackTitle(Some(&NSString::from_str(""))) };
        let (tx, rx) = mpsc::channel();
        let reply = RcBlock::new(move |ok: Bool, err: *mut NSError| {
            let outcome = if ok.as_bool() {
                AuthOutcome::Success
            } else {
                // SAFETY: LocalAuthentication passes a valid NSError (or nil)
                // that lives for the duration of the reply block.
                match unsafe { err.as_ref() }.map(|e| e.code()) {
                    Some(USER_CANCEL | USER_FALLBACK | SYSTEM_CANCEL | APP_CANCEL) => {
                        AuthOutcome::Cancelled
                    }
                    _ => AuthOutcome::Unavailable,
                }
            };
            let _ = tx.send(outcome);
        });
        let reason = NSString::from_str(reason);
        unsafe { ctx.evaluatePolicy_localizedReason_reply(POLICY, &reason, &reply) };
        rx.recv().unwrap_or(AuthOutcome::Unavailable)
    }
}

/// Windows Hello through `UserConsentVerifier`: whatever Hello method the
/// user set up (face, fingerprint, or the Hello PIN — Windows offers no
/// biometrics-only policy here).
#[cfg(target_os = "windows")]
mod windows_hello {
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::IUserConsentVerifierInterop;
    use windows::core::{HSTRING, factory};
    use windows_future::IAsyncOperation;

    use crate::{AuthOutcome, BiometricKind};

    pub fn biometric_kind() -> BiometricKind {
        match UserConsentVerifier::CheckAvailabilityAsync().and_then(|op| op.join()) {
            Ok(UserConsentVerifierAvailability::Available) => BiometricKind::Hello,
            Ok(_) => BiometricKind::None,
            Err(e) => {
                log::warn!("Windows Hello availability check failed: {e}");
                BiometricKind::None
            }
        }
    }

    /// Blocks until the prompt is answered; call it off the UI thread.
    /// Parented to `hwnd` when given (the prompt then comes up in front of
    /// the app instead of behind it), else the plain call.
    pub fn authenticate(hwnd: Option<HWND>, reason: &str) -> AuthOutcome {
        let message = HSTRING::from(reason);
        let op = match hwnd {
            Some(hwnd) => factory::<UserConsentVerifier, IUserConsentVerifierInterop>().and_then(
                |interop| unsafe {
                    interop.RequestVerificationForWindowAsync::<
                        IAsyncOperation<UserConsentVerificationResult>,
                    >(hwnd, &message)
                },
            ),
            None => UserConsentVerifier::RequestVerificationAsync(&message),
        };
        match op.and_then(|op| op.join()) {
            Ok(UserConsentVerificationResult::Verified) => AuthOutcome::Success,
            Ok(UserConsentVerificationResult::Canceled) => AuthOutcome::Cancelled,
            Ok(_) => AuthOutcome::Unavailable,
            Err(e) => {
                log::warn!("Windows Hello verification failed: {e}");
                AuthOutcome::Unavailable
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Memory(Mutex<BTreeMap<String, String>>);

    impl Backend for Memory {
        fn get(&self, key: &str) -> crate::Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &str) -> crate::Result<()> {
            self.0.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
        fn delete(&self, key: &str) -> crate::Result<()> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn legacy_file_is_imported_once_then_deleted() {
        let dir = std::env::temp_dir().join(format!(
            "pbm-migrate-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let legacy = FileStore::new(dir.join("dev-secrets.json"));
        legacy.set("box.a", "peckboard://pair/a").unwrap();
        legacy.set("box.b", "peckboard://pair/b").unwrap();

        let to = Memory::default();
        assert_eq!(migrate_legacy(&legacy, &to).unwrap(), 2);
        assert_eq!(
            to.get("box.a").unwrap().as_deref(),
            Some("peckboard://pair/a")
        );
        assert_eq!(
            to.get("box.b").unwrap().as_deref(),
            Some("peckboard://pair/b")
        );
        assert!(!legacy.path.exists());

        // Nothing left to import; the keyring is untouched.
        to.delete("box.a").unwrap();
        assert_eq!(migrate_legacy(&legacy, &to).unwrap(), 0);
        assert_eq!(to.get("box.a").unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
