//! PeckBoard mobile: pairs with PeckBoard boxes and shows each box's web UI
//! through a direct, end-to-end encrypted tunnel (`peckboard-relay`) served
//! on a gated loopback port inside the app. See ../README.md.

mod commands;
#[cfg(target_os = "ios")]
mod launch_url;
mod link;
mod nav;
mod store;
mod tunnel;

use std::sync::{Arc, Mutex};

use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, async_runtime};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_peckboard_native::{Lifecycle, PeckboardNativeExt};
use url::Url;

use commands::{AppState, now_ms};
use store::{SecretStore, Store};
use tunnel::{TunnelManager, TunnelState, TunnelStatus};

/// Event the shell UI listens to; payload is a `TunnelStatus`.
pub const STATUS_EVENT: &str = "tunnel-status";
/// A pairing deep link is waiting; the shell UI calls `take_pair_link`.
pub const PAIR_EVENT: &str = "pair-link";

/// Pairing links in the Keychain / Keystore via the native plugin.
struct NativeSecrets(AppHandle);

impl SecretStore for NativeSecrets {
    fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.0.native().secret_get(key)?)
    }
    fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        Ok(self.0.native().secret_set(key, value)?)
    }
    fn delete(&self, key: &str) -> anyhow::Result<()> {
        Ok(self.0.native().secret_delete(key)?)
    }
}

/// Origins the shell UI is served from (per platform, plus the dev server).
fn shell_origins(app: &AppHandle) -> Vec<Url> {
    let mut out: Vec<Url> = [
        "tauri://localhost",
        "http://tauri.localhost",
        "https://tauri.localhost",
    ]
    .iter()
    .filter_map(|s| Url::parse(s).ok())
    .collect();
    if cfg!(debug_assertions)
        && let Some(dev) = app.config().build.dev_url.clone()
    {
        out.push(dev);
    }
    out
}

/// Debug builds: route `log` and `tracing` (relay punch, QUIC) to
/// `<app data>/debug.log` — on the simulator read it from
/// `xcrun simctl get_app_container <udid> com.peckboard.app data`.
fn init_debug_log(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("debug.log"))
    else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "info,peckboard_relay=debug,peckboard_mobile_lib=debug",
        ))
        .try_init();
}

/// The shell page's URL as first loaded (scheme/host differ per platform
/// and in dev), so a deep link or the "Boxes" button can bring the WebView
/// back from a box UI.
#[derive(Default)]
struct ShellHome(Arc<Mutex<Option<Url>>>);

/// A `peckboard://pair/…` link opened the app: park it for the shell UI,
/// which asks the user to confirm (showing the relay) before pairing, and
/// bring the WebView back to the shell if a box UI is showing.
fn open_pair_link(app: &AppHandle, raw: &str) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    log::info!("deep link opened ({} chars)", raw.len());
    *state.pending_link.lock().unwrap() = Some(raw.to_string());
    let _ = app.emit(PAIR_EVENT, ());
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let on_box = win.url().is_ok_and(|u| u.host_str() == Some(nav::LOOPBACK));
    if on_box && let Some(url) = shell_home(app) {
        let _ = win.navigate(url);
    }
}

/// The shell page to bring the WebView back to: as first loaded, else the
/// platform's default.
fn shell_home(app: &AppHandle) -> Option<Url> {
    let home = app.state::<ShellHome>().0.lock().unwrap().clone();
    let fallback = if cfg!(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "linux"
    )) {
        "tauri://localhost/"
    } else {
        "http://tauri.localhost/"
    };
    home.or_else(|| Url::parse(fallback).ok())
}

/// The gate key was rotated (resume) and the tunnel is back: a box page
/// still showing on `port` carries the old cookie, so re-boot it through
/// the new key, landing on the page it was on.
fn reboot_box_page(app: &AppHandle, tunnel: &TunnelManager, port: u16) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let Ok(cur) = win.url() else { return };
    if cur.host_str() != Some(nav::LOOPBACK) || cur.port() != Some(port) {
        return;
    }
    let next = &cur[url::Position::BeforePath..];
    let Some(boot) = tunnel.boot_url_to(next) else {
        return;
    };
    if let Ok(u) = Url::parse(&boot) {
        let _ = win.navigate(u);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_peckboard_native::init())
        .manage(ShellHome::default());
    #[cfg(mobile)]
    {
        builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    }
    let app = builder
        .setup(|app| {
            let handle = app.handle().clone();
            let data_dir = app.path().app_data_dir()?;
            if cfg!(debug_assertions) {
                init_debug_log(&data_dir);
            }
            let store = Store::load(data_dir.join("boxes.json"))?;

            let events = handle.clone();
            let badge_shell = shell_origins(&handle);
            let tunnel = Arc::new(TunnelManager::new(move |st: &TunnelStatus| {
                if st.state == TunnelState::Connected
                    && let Some(s) = events.try_state::<AppState>()
                {
                    let _ = s
                        .store
                        .lock()
                        .unwrap()
                        .touch_connected(&st.box_id, now_ms());
                    if st.rekeyed {
                        reboot_box_page(&events, &s.tunnel, st.port);
                    }
                }
                // Keep the box page's "Relayed" badge on the live path.
                if let Some(win) = events.get_webview_window("main")
                    && let Ok(url) = win.url()
                    && let Some(js) =
                        nav::relay_badge_script(&url, &badge_shell, Some(st.port), st.relay_badge())
                {
                    let _ = win.eval(js);
                }
                let _ = events.emit(STATUS_EVENT, st);
            }));

            // Foreground-only tunnel. One consumer task keeps transitions in
            // order (background → foreground in quick succession).
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Lifecycle>();
            let lifecycle_tunnel = tunnel.clone();
            async_runtime::spawn(async move {
                while let Some(l) = rx.recv().await {
                    log::info!("lifecycle: {l:?}");
                    match l {
                        Lifecycle::Background => lifecycle_tunnel.pause().await,
                        Lifecycle::Foreground => {
                            if let Err(e) = lifecycle_tunnel.resume().await {
                                log::warn!("resume failed: {e:#}");
                            }
                        }
                    }
                }
            });
            app.native().watch_lifecycle(move |l| {
                let _ = tx.send(l);
            })?;

            app.manage(AppState {
                store: Mutex::new(store),
                secrets: Arc::new(NativeSecrets(handle.clone())),
                tunnel: tunnel.clone(),
                pending_link: Mutex::new(None),
            });

            // Navigation allow-list: shell + the active box's loopback
            // origin; other web links go to the system browser.
            let shell = shell_origins(&handle);
            let opener = handle.clone();
            let home = handle.state::<ShellHome>().0.clone();
            let (page_shell, page_tunnel) = (shell.clone(), tunnel.clone());
            let builder =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    // Every box page load (incl. the resume re-boot) gets
                    // the app's "Boxes" button; SPA route changes keep it.
                    .on_page_load(move |win, page| {
                        if page.event() != PageLoadEvent::Finished {
                            return;
                        }
                        let Some(home) = shell_home(win.app_handle()) else {
                            return;
                        };
                        let port = page_tunnel.active_port();
                        if let Some(js) =
                            nav::boxes_button_script(page.url(), &page_shell, port, &home)
                        {
                            let _ = win.eval(js);
                        }
                        let relayed = page_tunnel.status().is_some_and(|s| s.relay_badge());
                        if let Some(js) =
                            nav::relay_badge_script(page.url(), &page_shell, port, relayed)
                        {
                            let _ = win.eval(js);
                        }
                    })
                    .on_navigation(move |url| {
                        match nav::decide(url, &shell, tunnel.active_port()) {
                            nav::Decision::Allow => {
                                if nav::decide(url, &shell, None) == nav::Decision::Allow {
                                    home.lock().unwrap().get_or_insert_with(|| url.clone());
                                }
                                true
                            }
                            nav::Decision::OpenExternally => {
                                let _ = opener.opener().open_url(url.as_str(), None::<&str>);
                                false
                            }
                            nav::Decision::Block => {
                                log::warn!("blocked navigation to {}", url.scheme());
                                false
                            }
                        }
                    });
            #[cfg(desktop)]
            let builder = builder.title("PeckBoard").inner_size(420.0, 820.0);
            builder.build()?;

            // peckboard://pair/… links: launch URL, then any later ones.
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                urls.iter()
                    .for_each(|u| open_pair_link(&handle, u.as_str()));
            }
            let links = handle.clone();
            app.deep_link().on_open_url(move |ev| {
                ev.urls()
                    .iter()
                    .for_each(|u| open_pair_link(&links, u.as_str()));
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_boxes,
            commands::add_box,
            commands::rename_box,
            commands::remove_box,
            commands::connect_box,
            commands::disconnect_box,
            commands::tunnel_status,
            commands::take_pair_link,
        ])
        .build(tauri::generate_context!())
        .expect("error while building PeckBoard");
    #[cfg(target_os = "ios")]
    launch_url::install();
    app.run(|_app, _ev| {
        // Cold-launch deep link (iOS): stashed while the first scene
        // connected; picked up by the first event after setup.
        #[cfg(target_os = "ios")]
        {
            if let tauri::RunEvent::Ready = _ev {
                log::info!("launch URL hook: {}", launch_url::status());
            }
            if _app.try_state::<AppState>().is_some() {
                for u in launch_url::take() {
                    open_pair_link(_app, &u);
                }
            }
        }
    });
}
