use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::plugin::{PluginApi, PluginHandle};
use tauri::{AppHandle, Runtime};

use crate::{Lifecycle, MicDecision, MicPolicy, NetworkChange};

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_peckboard_native);

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<Native<R>> {
    #[cfg(target_os = "android")]
    let handle =
        api.register_android_plugin("com.peckboard.nativeplugin", "PeckboardNativePlugin")?;
    #[cfg(target_os = "ios")]
    let handle = api.register_ios_plugin(init_plugin_peckboard_native)?;
    Ok(Native(handle))
}

pub struct Native<R: Runtime>(PluginHandle<R>);

#[derive(Deserialize)]
struct ValueResponse {
    #[serde(default)]
    value: Option<String>,
}

#[derive(Deserialize)]
struct LifecycleMessage {
    state: Lifecycle,
}

impl<R: Runtime> Native<R> {
    pub fn secret_get(&self, key: &str) -> crate::Result<Option<String>> {
        let r: ValueResponse = self
            .0
            .run_mobile_plugin("secretGet", json!({ "key": key }))?;
        Ok(r.value)
    }

    pub fn secret_set(&self, key: &str, value: &str) -> crate::Result<()> {
        let _: serde_json::Value = self
            .0
            .run_mobile_plugin("secretSet", json!({ "key": key, "value": value }))?;
        Ok(())
    }

    pub fn secret_delete(&self, key: &str) -> crate::Result<()> {
        let _: serde_json::Value = self
            .0
            .run_mobile_plugin("secretDelete", json!({ "key": key }))?;
        Ok(())
    }

    /// Clear the WebView's website data for a removed box's loopback
    /// `origin` (`http://127.0.0.1:<port>`). Per-origin removal is partial on
    /// both platforms — WebKit groups records by host, Android's
    /// `WebStorage.deleteOrigin` skips localStorage — so with `host_wide`
    /// (the last box is gone) everything under `127.0.0.1` goes: all WebKit
    /// data types for that host; on Android all storage, cookies and the
    /// HTTP cache (`WebStorageCompat.deleteBrowsingDataForSite` where the
    /// WebView supports it).
    pub fn clear_site_data(&self, origin: &str, host_wide: bool) -> crate::Result<()> {
        let _: serde_json::Value = self.0.run_mobile_plugin(
            "clearSiteData",
            json!({ "origin": origin, "hostWide": host_wide }),
        )?;
        Ok(())
    }

    /// Android: drop the WebView's back/forward list (all but the current
    /// page), so Back from a box can't land on another box's retired origin
    /// — history navigations bypass the Rust navigation allow-list there.
    /// iOS enforces the allow-list on history navigations itself and has
    /// no API for this, so it's a no-op.
    pub fn clear_history(&self) -> crate::Result<()> {
        #[cfg(target_os = "android")]
        {
            let _: serde_json::Value = self.0.run_mobile_plugin("clearHistory", json!({}))?;
        }
        Ok(())
    }

    /// Who may use the microphone (see [`MicPolicy`]); `None` denies all.
    pub fn set_mic_policy(&self, policy: Option<&MicPolicy>) -> crate::Result<()> {
        let _: serde_json::Value = self
            .0
            .run_mobile_plugin("setMicPolicy", json!({ "policy": policy }))?;
        Ok(())
    }

    /// Calls `f` when the user answers a box's microphone prompt. Runs on
    /// the native caller's thread: hand the work off, don't call back into
    /// the plugin from inside `f`.
    pub fn watch_mic_decisions(
        &self,
        f: impl Fn(MicDecision) + Send + Sync + 'static,
    ) -> crate::Result<()> {
        let channel: Channel<serde_json::Value> = Channel::new(move |body: InvokeResponseBody| {
            if let Ok(d) = body.deserialize::<MicDecision>() {
                f(d);
            }
            Ok(())
        });
        let _: serde_json::Value = self
            .0
            .run_mobile_plugin("watchMicDecision", json!({ "channel": channel }))?;
        Ok(())
    }

    /// Calls `f` on every foreground/background transition.
    pub fn watch_lifecycle(
        &self,
        f: impl Fn(Lifecycle) + Send + Sync + 'static,
    ) -> crate::Result<()> {
        let channel: Channel<serde_json::Value> = Channel::new(move |body: InvokeResponseBody| {
            if let Ok(m) = body.deserialize::<LifecycleMessage>() {
                f(m.state);
            }
            Ok(())
        });
        let _: serde_json::Value = self
            .0
            .run_mobile_plugin("watchLifecycle", json!({ "channel": channel }))?;
        Ok(())
    }

    /// Calls `f` when the default network changes (not for the network at
    /// subscription). May fire in bursts; debounce on the caller's side.
    pub fn watch_network(
        &self,
        f: impl Fn(NetworkChange) + Send + Sync + 'static,
    ) -> crate::Result<()> {
        let channel: Channel<serde_json::Value> = Channel::new(move |body: InvokeResponseBody| {
            if let Ok(m) = body.deserialize::<NetworkChange>() {
                f(m);
            }
            Ok(())
        });
        let _: serde_json::Value = self
            .0
            .run_mobile_plugin("watchNetwork", json!({ "channel": channel }))?;
        Ok(())
    }
}
