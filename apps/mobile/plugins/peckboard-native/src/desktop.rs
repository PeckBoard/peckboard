//! Desktop stand-in so the app builds and runs for UI iteration. NOT secure
//! storage: values sit in a 0600 JSON file in the app data dir. The desktop
//! build is a development target only and is never shipped.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use tauri::plugin::PluginApi;
use tauri::{AppHandle, Manager, Runtime};

use crate::Lifecycle;

pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Native<R>> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| crate::Error::Native(e.to_string()))?;
    Ok(Native {
        path: dir.join("dev-secrets.json"),
        lock: Mutex::new(()),
        _r: std::marker::PhantomData,
    })
}

pub struct Native<R: Runtime> {
    path: PathBuf,
    lock: Mutex<()>,
    _r: std::marker::PhantomData<fn() -> R>,
}

impl<R: Runtime> Native<R> {
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

    pub fn secret_get(&self, key: &str) -> crate::Result<Option<String>> {
        let _g = self.lock.lock().unwrap();
        Ok(self.load()?.remove(key))
    }

    pub fn secret_set(&self, key: &str, value: &str) -> crate::Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut map = self.load()?;
        map.insert(key.to_string(), value.to_string());
        self.save(&map)
    }

    pub fn secret_delete(&self, key: &str) -> crate::Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut map = self.load()?;
        if map.remove(key).is_some() {
            self.save(&map)?;
        }
        Ok(())
    }

    /// Desktop windows don't get suspended; nothing to report.
    pub fn watch_lifecycle(
        &self,
        _f: impl Fn(Lifecycle) + Send + Sync + 'static,
    ) -> crate::Result<()> {
        Ok(())
    }
}
