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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use tauri::plugin::PluginApi;
use tauri::{AppHandle, Manager, Runtime};

use crate::Lifecycle;

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
        _r: std::marker::PhantomData,
    })
}

pub struct Native<R: Runtime> {
    backend: Box<dyn Backend>,
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
