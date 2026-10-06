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

use commands::{AppState, MicSync, ShellNonce, ShellOrigins, now_ms};
use link::PairSlot;
use store::{SecretStore, Store};
use tunnel::{Milestone, TunnelManager, TunnelState, TunnelStatus};

/// Event the shell UI listens to; payload is a `TunnelStatus`.
pub const STATUS_EVENT: &str = "tunnel-status";
/// A pairing deep link is waiting; the shell UI calls `take_pair_link`.
pub const PAIR_EVENT: &str = "pair-link";
/// A pairing link arrived while another was on the confirm screen and was
/// dropped (the shell shows a toast).
pub const PAIR_IGNORED_EVENT: &str = "pair-link-ignored";

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

/// Origins the shell UI is served from: this platform's Tauri origin (and
/// the dev server in debug builds) — see `nav::shell_origins`.
fn shell_origins(app: &AppHandle) -> Vec<Url> {
    let dev = if cfg!(debug_assertions) {
        app.config().build.dev_url.clone()
    } else {
        None
    };
    nav::shell_origins(cfg!(any(windows, target_os = "android")), dev)
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

/// A pairing link (`https://peckboard.com/pair#…` or `peckboard://pair/…`)
/// opened the app: park it for the shell UI, which asks the user to
/// confirm (showing the relay and the box fingerprint) before pairing, and
/// bring the WebView back to the shell if a box UI is showing. While one
/// link is on the confirm screen a second one is dropped, so a page can't
/// swap the link under the user.
fn open_pair_link(app: &AppHandle, raw: &str) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    log::info!("deep link opened ({} chars)", raw.len());
    // Debug builds only: `PBM_DEBUG_AUTOCONFIRM=1` (e.g. `devicectl device
    // process launch --environment-variables`) pairs the link through the
    // same path as the confirm screen's Pair button, so pairing can be
    // verified on a device without a tap.
    #[cfg(debug_assertions)]
    if std::env::var_os("PBM_DEBUG_AUTOCONFIRM").is_some() {
        let app = app.clone();
        let raw = raw.to_string();
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            match commands::pair_link(&app, &state, &raw, "").await {
                Ok(v) => log::info!("debug autoconfirm: paired {:?}", v.record.auth),
                Err(e) => log::warn!("debug autoconfirm: {e}"),
            }
        });
        return;
    }
    let parked = state.pair_slot.lock().unwrap().offer(raw);
    if !parked {
        log::warn!("deep link ignored: another pairing link is being confirmed");
        let _ = app.emit(PAIR_IGNORED_EVENT, ());
        return;
    }
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
    let fallback = if cfg!(any(windows, target_os = "android")) {
        "http://tauri.localhost/"
    } else {
        "tauri://localhost/"
    };
    home.or_else(|| Url::parse(fallback).ok())
}

/// A second launch (desktop) handed its link to this instance: bring the
/// window up.
#[cfg(desktop)]
fn focus_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
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
    let mut builder = tauri::Builder::default();
    #[cfg(desktop)]
    {
        // Single-instance first, so a second launch exits before setup.
        builder = builder
            .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
                focus_main(app)
            }))
            .plugin(tauri_plugin_window_state::Builder::default().build());
    }
    builder = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_peckboard_native::init())
        .manage(ShellHome::default())
        .manage(MicSync::default());
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

            // Navigation allow-list and IPC proof: shell + the active box's
            // loopback origin; other web links go to the system browser.
            let shell = shell_origins(&handle);
            let nonce = ShellNonce::generate();
            let init_script = nonce.init_script(&shell);
            app.manage(nonce);
            app.manage(ShellOrigins(shell.clone()));

            let events = handle.clone();
            let badge_shell = shell.clone();
            let tunnel = Arc::new(TunnelManager::new(move |st: &TunnelStatus| {
                if let Some(s) = events.try_state::<AppState>() {
                    if st.state == TunnelState::Connected {
                        let _ = s
                            .store
                            .lock()
                            .unwrap()
                            .touch_connected(&st.box_id, now_ms());
                        if st.rekeyed {
                            reboot_box_page(&events, &s.tunnel, st.port);
                        }
                    }
                    // Pairing v2 bookkeeping (the credential itself was
                    // stored by `on_enrolled` before the box was told).
                    match &st.milestone {
                        Some(Milestone::Enrolled {
                            box_fingerprint,
                            legacy_upgrade,
                        }) => {
                            log::info!(
                                "box {}: enrolled with box {box_fingerprint} (legacy upgrade: {legacy_upgrade})",
                                st.box_id
                            );
                            if let Err(e) = s
                                .store
                                .lock()
                                .unwrap()
                                .set_enrolled(&st.box_id, box_fingerprint)
                            {
                                log::warn!("enrollment bookkeeping failed: {e:#}");
                            }
                        }
                        Some(Milestone::Activated) => {
                            log::info!("box {}: activated, link retired", st.box_id);
                            if let Err(e) = s
                                .store
                                .lock()
                                .unwrap()
                                .finish_activation(s.secrets.as_ref(), &st.box_id)
                            {
                                log::warn!("activation bookkeeping failed: {e:#}");
                            }
                        }
                        None => {}
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
                // Microphone follows the active box (and none once stopped).
                commands::sync_mic_policy(&events);
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

            // Wi-Fi ↔ cellular: reconnect now instead of after the ping
            // timeout. Only a running (foregrounded) tunnel is kicked, and
            // bursts collapse into one (`KICK_DEBOUNCE`). Port and gate key
            // stay, so the box page isn't rebooted.
            let network_tunnel = tunnel.clone();
            if let Err(e) = app.native().watch_network(move |c| {
                if network_tunnel.network_changed() {
                    log::info!("network changed ({}): reconnecting the tunnel", c.detail);
                } else {
                    log::info!(
                        "network changed ({}): no running tunnel or debounced",
                        c.detail
                    );
                }
            }) {
                log::warn!("network monitor unavailable: {e}");
            }

            // The user answered a box's microphone prompt (native). Off the
            // caller's thread: saving it pushes the new policy to native.
            let mic = handle.clone();
            if let Err(e) = app.native().watch_mic_decisions(move |d| {
                let app = mic.clone();
                async_runtime::spawn_blocking(move || commands::apply_mic_decision(&app, d));
            }) {
                log::warn!("microphone decisions unavailable: {e}");
            }

            app.manage(AppState {
                store: Mutex::new(store),
                secrets: Arc::new(NativeSecrets(handle.clone())),
                tunnel: tunnel.clone(),
                pair_slot: Mutex::new(PairSlot::default()),
            });

            let opener = handle.clone();
            let home = handle.state::<ShellHome>().0.clone();
            let (page_shell, page_tunnel) = (shell.clone(), tunnel.clone());
            let builder =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    // The shell's IPC nonce; defines itself on shell origins
                    // only (see `ShellNonce::init_script`).
                    .initialization_script(init_script)
                    .on_page_load(move |win, page| {
                        let app = win.app_handle();
                        let port = page_tunnel.active_port();
                        match page.event() {
                            // Android's history navigations (Back) skip
                            // `on_navigation`: re-check what's loading and
                            // send anything off the allow-list (another
                            // box's retired origin) back to the shell.
                            // `about:blank` (the WebView's empty page) is
                            // inert and never ours to redirect.
                            PageLoadEvent::Started => {
                                if page.url().scheme() != "about"
                                    && nav::decide(page.url(), &page_shell, port)
                                        != nav::Decision::Allow
                                {
                                    log::warn!(
                                        "page load off the allow-list: {}",
                                        nav::origin_string(page.url())
                                    );
                                    if let Some(h) = shell_home(app) {
                                        let _ = win.navigate(h);
                                    }
                                }
                                return;
                            }
                            PageLoadEvent::Finished => {}
                        }
                        let Some(home) = shell_home(app) else {
                            return;
                        };
                        // Back on the shell: a box's pages must not stay
                        // reachable through history once it's left. Off this
                        // thread: on Android this callback runs on the main
                        // thread, and plugin calls are dispatched to — and
                        // awaited from — that same thread.
                        if nav::is_shell(page.url(), &page_shell) {
                            let app = app.clone();
                            async_runtime::spawn_blocking(move || {
                                if let Err(e) = app.native().clear_history() {
                                    log::warn!("clearing history failed: {e}");
                                }
                            });
                        }
                        // Every box page load (incl. the resume re-boot)
                        // gets the app's "Boxes" button; SPA route changes
                        // keep it.
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
                                if nav::is_shell(url, &shell) {
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
            let builder = builder
                .title("PeckBoard")
                .inner_size(1200.0, 800.0)
                .min_inner_size(400.0, 600.0);
            builder.build()?;

            // Pairing links (https://peckboard.com/pair#… via Universal /
            // App Links on iOS / Android, peckboard://pair/… everywhere):
            // launch URL, then any later ones (on Windows / Linux a later
            // one arrives via single-instance).
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            if let Err(e) = app.deep_link().register_all() {
                log::warn!("registering peckboard:// failed: {e}");
            }
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
            commands::confirm_pair,
            commands::dismiss_pair,
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
