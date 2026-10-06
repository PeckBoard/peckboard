//! PeckBoard mobile: pairs with PeckBoard boxes and shows each box's web UI
//! through a direct, end-to-end encrypted tunnel (`peckboard-relay`) served
//! on a gated loopback port inside the app. See ../README.md.

mod commands;
#[cfg(target_os = "ios")]
mod launch_url;
mod link;
mod lock;
mod nav;
mod store;
mod tunnel;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, async_runtime};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_peckboard_native::{Lifecycle, PeckboardNativeExt};
use url::Url;

use commands::{AppState, MicSync, ShellNonce, ShellOrigins, now_ms};
use link::PairSlot;
use lock::LockManager;
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

/// The app just locked on its way back to the foreground: the native
/// privacy cover stays up until the shell page (the lock screen) has loaded.
#[derive(Default)]
struct CoverPending(AtomicBool);

/// What the lifecycle consumer reacts to, in order.
#[derive(Debug)]
enum Signal {
    /// With whether one of our own biometric prompts was showing when it
    /// fired (the Face ID sheet resigns the app active: not "away").
    Life(Lifecycle, bool),
    /// Desktop: the main window lost focus (alt-tab, a dialog, our own
    /// Touch ID / Windows Hello prompt): not "away" by itself.
    #[cfg(desktop)]
    Blur,
    /// Desktop: the main window was minimized / hidden at this wall-clock
    /// ms (starts the auto-lock timer only; the tunnel keeps running).
    #[cfg(desktop)]
    Hidden(u64),
    /// Desktop: the main window got focus back.
    #[cfg(desktop)]
    Focus,
    /// Desktop: the machine slept, last awake at this wall-clock ms.
    #[cfg(desktop)]
    Slept(u64),
}

/// Desktop: how long after losing focus the window is checked for being
/// minimized / hidden (the state lags the focus event on some platforms).
#[cfg(desktop)]
const HIDE_SETTLE: std::time::Duration = std::time::Duration::from_millis(600);

/// Desktop: the main window is minimized or hidden (not merely unfocused).
#[cfg(desktop)]
fn window_away(win: &tauri::WebviewWindow) -> bool {
    win.is_minimized().unwrap_or(false) || !win.is_visible().unwrap_or(true)
}

fn lower_privacy_cover(app: &AppHandle) {
    if let Err(e) = app.native().lower_privacy_cover() {
        log::warn!("lowering the privacy cover failed: {e}");
    }
}

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
            if state.lock.is_locked() {
                log::warn!("debug autoconfirm: app is locked");
                return;
            }
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

/// Engage the app lock (if one is configured): stop the tunnel and any
/// pairing round and, with `navigate`, bring the WebView back to the shell,
/// which boots into the lock screen; the privacy cover comes down once that
/// page has loaded.
pub(crate) async fn engage_lock(app: &AppHandle, navigate: bool) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let (lock, tunnel) = (state.lock.clone(), state.tunnel.clone());
    if !lock.engage().await {
        return;
    }
    log::info!("app locked");
    tunnel.stop_all().await;
    if navigate {
        let pending = &app.state::<CoverPending>().0;
        pending.store(true, Ordering::SeqCst);
        let shown = match (app.get_webview_window("main"), shell_home(app)) {
            (Some(win), Some(home)) => win.navigate(home).is_ok(),
            _ => false,
        };
        if !shown && pending.swap(false, Ordering::SeqCst) {
            let app = app.clone();
            let _ = async_runtime::spawn_blocking(move || lower_privacy_cover(&app)).await;
        }
    }
    let app = app.clone();
    let _ = async_runtime::spawn_blocking(move || commands::emit_lock_status(&app, &lock)).await;
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
        .manage(CoverPending::default())
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

            // Foreground-only tunnel, and the app lock's timer. One consumer
            // task keeps transitions in order (background → foreground in
            // quick succession).
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Signal>();
            let lifecycle_app = handle.clone();
            async_runtime::spawn(async move {
                let app = lifecycle_app;
                // Desktop: whether the main window has focus (it may come
                // back from a wake still unfocused).
                #[cfg_attr(mobile, allow(unused_mut))]
                let mut focused = true;
                while let Some(sig) = rx.recv().await {
                    log::info!("lifecycle: {sig:?}");
                    let Some(state) = app.try_state::<AppState>() else {
                        continue;
                    };
                    let (lock, tunnel) = (state.lock.clone(), state.tunnel.clone());
                    match sig {
                        Signal::Life(Lifecycle::Background, own_prompt) => {
                            // Our own Face ID sheet resigning the app
                            // active isn't leaving it.
                            if !own_prompt {
                                lock.note_away(now_ms());
                            }
                            let (a, l) = (app.clone(), lock.clone());
                            let _ = async_runtime::spawn_blocking(move || {
                                commands::sync_privacy_cover(&a, &l)
                            })
                            .await;
                            tunnel.pause().await;
                        }
                        Signal::Life(Lifecycle::Foreground, _) => {
                            if lock.due(now_ms()) {
                                engage_lock(&app, true).await;
                                continue;
                            }
                            if focused {
                                lock.clear_away();
                            }
                            // Locked (already): stays paused.
                            if let Ok(unlocked) = lock.unlocked().await
                                && let Err(e) = tunnel.resume(&unlocked).await
                            {
                                log::warn!("resume failed: {e:#}");
                            }
                            let a = app.clone();
                            let _ =
                                async_runtime::spawn_blocking(move || lower_privacy_cover(&a))
                                    .await;
                        }
                        #[cfg(desktop)]
                        Signal::Blur => focused = false,
                        #[cfg(desktop)]
                        Signal::Hidden(at) => {
                            focused = false;
                            lock.note_away(at);
                        }
                        #[cfg(desktop)]
                        Signal::Focus => {
                            focused = true;
                            if lock.due(now_ms()) {
                                engage_lock(&app, true).await;
                            } else {
                                lock.clear_away();
                            }
                        }
                        #[cfg(desktop)]
                        Signal::Slept(at) => {
                            lock.note_away(at);
                            if lock.due(now_ms()) {
                                engage_lock(&app, true).await;
                            } else if focused {
                                lock.clear_away();
                            }
                        }
                    }
                }
            });
            let life_tx = tx.clone();
            let life_app = handle.clone();
            app.native().watch_lifecycle(move |l| {
                let own_prompt = life_app
                    .try_state::<AppState>()
                    .is_some_and(|s| s.lock.prompt_in_flight());
                let _ = life_tx.send(Signal::Life(l, own_prompt));
            })?;
            // Desktop: the plugin's wake report (Background + Foreground)
            // comes after the sleep, so time asleep is measured here: a tick
            // that took much longer than it should means the machine slept
            // since the previous one.
            #[cfg(desktop)]
            {
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name("peckboard-lock-clock".into())
                    .spawn(move || {
                        let mut last = now_ms();
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            let now = now_ms();
                            if now.saturating_sub(last) >= 10_000
                                && tx.send(Signal::Slept(last)).is_err()
                            {
                                break;
                            }
                            last = now;
                        }
                    })?;
            }

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

            let secrets: Arc<dyn SecretStore> = Arc::new(NativeSecrets(handle.clone()));
            let lock = Arc::new(LockManager::load(
                data_dir.join("lock.json"),
                secrets.clone(),
                !store.boxes().is_empty(),
            ));
            app.manage(AppState {
                store: Mutex::new(store),
                secrets,
                tunnel: tunnel.clone(),
                pair_slot: Mutex::new(PairSlot::default()),
                lock: lock.clone(),
            });
            // Check the lock against secure storage (announcing any change)
            // and arm the privacy cover before the app can first leave the
            // foreground. Off the main thread (plugin calls need it free).
            let cover = handle.clone();
            async_runtime::spawn_blocking(move || {
                commands::lock_status_of(&cover, &lock);
                commands::sync_privacy_cover(&cover, &lock);
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
                            let lower = app
                                .state::<CoverPending>()
                                .0
                                .swap(false, Ordering::SeqCst);
                            async_runtime::spawn_blocking(move || {
                                if let Err(e) = app.native().clear_history() {
                                    log::warn!("clearing history failed: {e}");
                                }
                                // The lock screen is up: drop the cover.
                                if lower {
                                    lower_privacy_cover(&app);
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
            let _main = builder.build()?;
            // Desktop: being minimized / hidden starts the auto-lock timer
            // (the tunnel stays up); plain focus loss — alt-tab, a file
            // picker, our own Touch ID / Windows Hello prompt — doesn't.
            // Getting focus back checks the timer.
            #[cfg(desktop)]
            {
                let tx = tx.clone();
                let win = _main.clone();
                _main.on_window_event(move |ev| match ev {
                    tauri::WindowEvent::Focused(true) => {
                        let _ = tx.send(Signal::Focus);
                    }
                    tauri::WindowEvent::Focused(false) => {
                        let _ = tx.send(Signal::Blur);
                        let (tx, win, at) = (tx.clone(), win.clone(), now_ms());
                        async_runtime::spawn(async move {
                            tokio::time::sleep(HIDE_SETTLE).await;
                            if !win.is_focused().unwrap_or(false) && window_away(&win) {
                                let _ = tx.send(Signal::Hidden(at));
                            }
                        });
                    }
                    // Windows reports a minimize as a resize to 0×0.
                    tauri::WindowEvent::Resized(size) if size.width == 0 || size.height == 0 => {
                        let _ = tx.send(Signal::Hidden(now_ms()));
                    }
                    _ => {}
                });
            }
            drop(tx);

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
            commands::lock_status,
            commands::lock_setup,
            commands::lock_unlock,
            commands::lock_unlock_biometric,
            commands::lock_change,
            commands::lock_set_options,
            commands::lock_disable,
            commands::lock_now,
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
