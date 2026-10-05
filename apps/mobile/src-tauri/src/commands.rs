//! Shell-UI commands. Granted only to the local shell (capabilities/shell.json)
//! — and, belt and braces, every command takes a [`ShellProof`], so a box
//! page can't call one even if Tauri's origin check is caught mid-navigation.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::ipc::{CommandArg, CommandItem, InvokeError};
use tauri::{AppHandle, Manager, Runtime, State, WebviewWindow};
use tauri_plugin_peckboard_native::{MicDecision, MicPolicy, PeckboardNativeExt};
use url::Url;

use crate::link::{PairPrompt, parse_link};
use crate::nav;
use crate::store::{BoxRecord, SecretStore, Store};
use crate::tunnel::{TunnelManager, TunnelState, TunnelStatus};

pub struct AppState {
    pub store: Mutex<Store>,
    pub secrets: Arc<dyn SecretStore>,
    pub tunnel: Arc<TunnelManager>,
    /// Raw deep-linked pairing link awaiting the user's confirmation.
    pub pending_link: Mutex<Option<String>>,
}

/// Header the shell sends with every command, carrying the [`ShellNonce`].
pub const SHELL_HEADER: &str = "x-peckboard-shell";
/// Global the shell page reads the nonce from (see [`ShellNonce::init_script`]).
const SHELL_GLOBAL: &str = "__PBM_SHELL__";
const REJECT: &str = "This command is only available to the PeckBoard shell.";

/// A per-launch secret only the shell page is given. Tauri's own IPC check
/// resolves the caller from the webview's *current* URL (on Android, the
/// one its WebViewClient saw last at `onPageStarted`), so a box page that
/// fires an invoke while navigating back to the shell could be judged as
/// the shell. The nonce closes that: the box UI never learns it.
pub struct ShellNonce(String);

impl ShellNonce {
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        Self(b.iter().map(|x| format!("{x:02x}")).collect())
    }

    /// Initialization script (runs before any page script, on every page the
    /// WebView loads): defines the nonce on `window` only when the page is a
    /// shell origin, in the top frame. A box page runs the same script but
    /// its origin never matches; it can't read the script's source either —
    /// `WKUserScript` / `addDocumentStartJavaScript` aren't in the DOM, and
    /// the HTML-injection fallback only applies to the shell's own protocol.
    /// `location.protocol` / `location.host` are unforgeable.
    pub fn init_script(&self, shell: &[Url]) -> String {
        let origins: Vec<String> = shell.iter().map(nav::origin_string).collect();
        format!(
            "(function(){{var s={origins};if(window.top!==window)return;\
if(s.indexOf(location.protocol+\"//\"+location.host)===-1)return;\
Object.defineProperty(window,{global},{{value:{nonce},writable:false,configurable:false,enumerable:false}});}})();",
            origins = serde_json::to_string(&origins).unwrap_or_else(|_| "[]".into()),
            global = serde_json::to_string(SHELL_GLOBAL).unwrap_or_default(),
            nonce = serde_json::to_string(&self.0).unwrap_or_default(),
        )
    }
}

/// The shell UI's origins on this platform (`nav::shell_origins`), as
/// managed state for [`ShellProof`].
pub struct ShellOrigins(pub Vec<Url>);

// Proof token: bearer is a command invoked by the app's own shell page — the
// "main" webview, currently showing a shell origin, sending this launch's
// shell nonce. The only constructor is the `CommandArg` impl below, so a
// command that takes a `ShellProof` can't be reached without it. See
// `remove_box` for an example.
pub struct ShellProof(());

impl<'de, R: Runtime> CommandArg<'de, R> for ShellProof {
    fn from_command(command: CommandItem<'de, R>) -> Result<Self, InvokeError> {
        let msg = command.message;
        let webview = msg.webview_ref();
        let (Some(nonce), Some(shell)) = (
            webview.try_state::<ShellNonce>(),
            webview.try_state::<ShellOrigins>(),
        ) else {
            log::warn!("{}: shell state missing", command.name);
            return Err(InvokeError::from(REJECT));
        };
        let header = msg
            .headers()
            .get(SHELL_HEADER)
            .and_then(|v| v.to_str().ok());
        // A URL the runtime can't report is not a reason to lock the shell
        // out: the nonce alone proves the caller.
        let url = webview.url().ok();
        verify_shell_caller(webview.label(), header, &nonce.0, url.as_ref(), &shell.0)
            .map(|()| ShellProof(()))
            .map_err(|why| {
                log::warn!("{} rejected: {why}", command.name);
                InvokeError::from(REJECT)
            })
    }
}

/// The checks behind [`ShellProof`]: the shell's webview label, the nonce
/// (compared in constant time), and — when known — a shell origin showing.
fn verify_shell_caller(
    label: &str,
    header: Option<&str>,
    nonce: &str,
    url: Option<&Url>,
    shell: &[Url],
) -> Result<(), &'static str> {
    if label != "main" {
        return Err("not the shell webview");
    }
    match header {
        Some(h) if ct_eq(h.as_bytes(), nonce.as_bytes()) => {}
        Some(_) => return Err("wrong shell token"),
        None => return Err("no shell token"),
    }
    match url {
        Some(u) if !nav::is_shell(u, shell) => Err("not a shell page"),
        _ => Ok(()),
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoxView {
    #[serde(flatten)]
    record: BoxRecord,
    status: Option<TunnelStatus>,
}

type CmdResult<T> = Result<T, String>;

fn msg(e: anyhow::Error) -> String {
    format!("{e:#}")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn view(state: &AppState, record: BoxRecord) -> BoxView {
    let status = state.tunnel.status().filter(|s| s.box_id == record.id);
    BoxView { record, status }
}

#[tauri::command]
pub fn list_boxes(state: State<'_, AppState>, _shell: ShellProof) -> Vec<BoxView> {
    let boxes = state.store.lock().unwrap().boxes().to_vec();
    boxes.into_iter().map(|b| view(&state, b)).collect()
}

#[tauri::command]
pub fn add_box(
    state: State<'_, AppState>,
    _shell: ShellProof,
    link: String,
    name: String,
) -> CmdResult<BoxView> {
    let link = parse_link(&link)?;
    let rec = state
        .store
        .lock()
        .unwrap()
        .add(state.secrets.as_ref(), &link, &name, now_ms())
        .map_err(msg)?;
    Ok(view(&state, rec))
}

#[tauri::command]
pub fn rename_box(
    app: AppHandle,
    state: State<'_, AppState>,
    _shell: ShellProof,
    id: String,
    name: String,
) -> CmdResult<BoxView> {
    let rec = state
        .store
        .lock()
        .unwrap()
        .rename(&id, &name)
        .map_err(msg)?;
    // The microphone prompt names the box.
    sync_mic_policy(&app);
    Ok(view(&state, rec))
}

/// Forget a box: stop its tunnel, drop its secret and metadata, and clear
/// the website data its UI left at its loopback origin.
#[tauri::command]
pub async fn remove_box(
    app: AppHandle,
    webview: WebviewWindow,
    state: State<'_, AppState>,
    _shell: ShellProof,
    id: String,
) -> CmdResult<()> {
    state.tunnel.stop_box(&id).await;
    let (removed, last) = {
        let mut store = state.store.lock().unwrap();
        let rec = store.remove(state.secrets.as_ref(), &id).map_err(msg)?;
        (rec, store.boxes().is_empty())
    };
    forget_site_data(&app, &webview, removed.port, last);
    sync_mic_policy(&app);
    Ok(())
}

/// The website data a removed box's UI left at `http://127.0.0.1:<port>`:
/// its login token in localStorage, IndexedDB, caches, cookies. No platform
/// clears exactly one origin — WebKit groups records by host, WebView2 by
/// profile, Android's `WebStorage.deleteOrigin` skips localStorage — so
/// this is best effort per origin, and once the `last` box is gone
/// everything is wiped (natively for the host, plus Tauri's whole-store
/// clear). What a partial clear leaves behind stays unreachable: the port is
/// retired, never handed to another box (`store::next_port`).
fn forget_site_data(app: &AppHandle, webview: &WebviewWindow, port: u16, last: bool) {
    let origin = nav::origin(port);
    if let Err(e) = app.native().clear_site_data(&origin, last) {
        log::warn!("clearing website data for {origin} failed: {e}");
    }
    if last && let Err(e) = webview.clear_all_browsing_data() {
        log::warn!("clearing browsing data failed: {e}");
    }
    log::info!(
        "removed box on port {port}: website data cleared ({})",
        if last {
            "all of 127.0.0.1"
        } else {
            "per-origin best effort"
        }
    );
}

#[tauri::command]
pub async fn connect_box(
    app: AppHandle,
    state: State<'_, AppState>,
    _shell: ShellProof,
    id: String,
) -> CmdResult<TunnelStatus> {
    let (link, port) = {
        let store = state.store.lock().unwrap();
        let rec = store.get(&id).ok_or("No such box.")?;
        let link = store.link(state.secrets.as_ref(), &id).map_err(msg)?;
        (link, rec.port)
    };
    let status = state.tunnel.start(&id, link, port).await.map_err(msg)?;
    sync_mic_policy(&app);
    Ok(status)
}

#[tauri::command]
pub async fn disconnect_box(
    app: AppHandle,
    state: State<'_, AppState>,
    _shell: ShellProof,
) -> CmdResult<()> {
    state.tunnel.stop().await;
    sync_mic_policy(&app);
    Ok(())
}

#[tauri::command]
pub fn tunnel_status(state: State<'_, AppState>, _shell: ShellProof) -> Option<TunnelStatus> {
    state.tunnel.status()
}

/// The pairing deep link waiting for confirmation, if any (taken once).
#[tauri::command]
pub fn take_pair_link(state: State<'_, AppState>, _shell: ShellProof) -> Option<PairPrompt> {
    let raw = state.pending_link.lock().unwrap().take()?;
    Some(PairPrompt::from_link(&raw))
}

/// The microphone policy last pushed to the native WebView delegate, so
/// `sync_mic_policy` only crosses into native code on a change.
#[derive(Default)]
pub struct MicSync(Mutex<Option<MicPolicy>>);

/// What the native side should allow right now (see [`MicPolicy`]): the
/// active box's origin with that box's remembered answer, or nothing.
fn desired_mic_policy(state: &AppState) -> Option<MicPolicy> {
    let st = state
        .tunnel
        .status()
        .filter(|s| s.state != TunnelState::Stopped)?;
    let store = state.store.lock().unwrap();
    store.get(&st.box_id).map(|b| MicPolicy {
        origin: nav::origin(st.port),
        box_id: b.id.clone(),
        box_name: b.name.clone(),
        allowed: b.mic_allowed,
    })
}

/// Push the microphone policy to the native side if it changed: after
/// connect / disconnect / switch, a rename, a remembered answer, a removal.
pub fn sync_mic_policy(app: &AppHandle) {
    let (Some(state), Some(sync)) = (app.try_state::<AppState>(), app.try_state::<MicSync>())
    else {
        return;
    };
    let desired = desired_mic_policy(&state);
    let mut last = sync.0.lock().unwrap();
    if *last == desired {
        return;
    }
    match app.native().set_mic_policy(desired.as_ref()) {
        Ok(()) => *last = desired,
        // Left unsynced: the next status change retries.
        Err(e) => log::warn!("microphone policy not applied: {e}"),
    }
}

/// The user answered a box's microphone prompt: remember it with the box
/// and let the native side grant without asking again.
pub fn apply_mic_decision(app: &AppHandle, d: MicDecision) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let saved = state
        .store
        .lock()
        .unwrap()
        .set_mic_allowed(&d.box_id, Some(d.allowed));
    match saved {
        Ok(()) => log::info!(
            "microphone {} for box {}",
            if d.allowed { "allowed" } else { "denied" },
            d.box_id
        ),
        Err(e) => log::warn!("microphone decision not saved: {e:#}"),
    }
    sync_mic_policy(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn shell_caller_needs_main_label_nonce_and_shell_origin() {
        let shell = [u("tauri://localhost")];
        let ok = |label, header: Option<&str>, url: Option<&str>| {
            verify_shell_caller(label, header, "n0nce", url.map(u).as_ref(), &shell)
        };
        assert_eq!(
            ok("main", Some("n0nce"), Some("tauri://localhost/index.html")),
            Ok(())
        );
        // URL unknown: the nonce suffices.
        assert_eq!(ok("main", Some("n0nce"), None), Ok(()));
        // A box page (even with the right nonce), another webview, no or a
        // wrong nonce — including one that only differs in length.
        assert!(ok("main", Some("n0nce"), Some("http://127.0.0.1:41000/")).is_err());
        assert!(ok("box", Some("n0nce"), Some("tauri://localhost/")).is_err());
        assert!(ok("main", None, Some("tauri://localhost/")).is_err());
        assert!(ok("main", Some("n0nc3"), Some("tauri://localhost/")).is_err());
        assert!(ok("main", Some("n0nce!"), Some("tauri://localhost/")).is_err());
        assert!(ok("main", Some(""), Some("tauri://localhost/")).is_err());
        // The other platform's shell origin is not this platform's shell.
        assert!(ok("main", Some("n0nce"), Some("http://tauri.localhost/")).is_err());
    }

    #[test]
    fn init_script_exposes_the_nonce_to_shell_origins_only() {
        let nonce = ShellNonce::generate();
        assert_eq!(nonce.0.len(), 64);
        assert_ne!(nonce.0, ShellNonce::generate().0);
        let js = nonce.init_script(&[u("tauri://localhost"), u("http://localhost:1420")]);
        assert!(
            js.contains(r#"["tauri://localhost","http://localhost:1420"]"#),
            "{js}"
        );
        assert!(js.contains(&format!("value:\"{}\"", nonce.0)));
        assert!(js.contains("location.protocol+\"//\"+location.host"));
        assert!(js.contains("window.top!==window"));
        assert!(js.contains("\"__PBM_SHELL__\""));
        // Never an IPC call of its own, and the box origin isn't listed.
        assert!(!js.contains("__TAURI") && !js.contains("invoke"));
        assert!(!js.contains("127.0.0.1"));
    }
}
