//! Shell-UI commands. Granted only to the local shell (capabilities/shell.json);
//! the tunnelled box UI can't call any of them.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::State;

use crate::link::{PairPrompt, parse_link};
use crate::store::{BoxRecord, SecretStore, Store};
use crate::tunnel::{TunnelManager, TunnelStatus};

pub struct AppState {
    pub store: Mutex<Store>,
    pub secrets: Arc<dyn SecretStore>,
    pub tunnel: Arc<TunnelManager>,
    /// Raw deep-linked pairing link awaiting the user's confirmation.
    pub pending_link: Mutex<Option<String>>,
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
pub fn list_boxes(state: State<'_, AppState>) -> Vec<BoxView> {
    let boxes = state.store.lock().unwrap().boxes().to_vec();
    boxes.into_iter().map(|b| view(&state, b)).collect()
}

#[tauri::command]
pub fn add_box(state: State<'_, AppState>, link: String, name: String) -> CmdResult<BoxView> {
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
pub fn rename_box(state: State<'_, AppState>, id: String, name: String) -> CmdResult<BoxView> {
    let rec = state
        .store
        .lock()
        .unwrap()
        .rename(&id, &name)
        .map_err(msg)?;
    Ok(view(&state, rec))
}

#[tauri::command]
pub async fn remove_box(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.tunnel.stop_box(&id).await;
    state
        .store
        .lock()
        .unwrap()
        .remove(state.secrets.as_ref(), &id)
        .map_err(msg)
}

#[tauri::command]
pub async fn connect_box(state: State<'_, AppState>, id: String) -> CmdResult<TunnelStatus> {
    let (link, port) = {
        let store = state.store.lock().unwrap();
        let rec = store.get(&id).ok_or("No such box.")?;
        let link = store.link(state.secrets.as_ref(), &id).map_err(msg)?;
        (link, rec.port)
    };
    state.tunnel.start(&id, link, port).await.map_err(msg)
}

#[tauri::command]
pub async fn disconnect_box(state: State<'_, AppState>) -> CmdResult<()> {
    state.tunnel.stop().await;
    Ok(())
}

#[tauri::command]
pub fn tunnel_status(state: State<'_, AppState>) -> Option<TunnelStatus> {
    state.tunnel.status()
}

/// The pairing deep link waiting for confirmation, if any (taken once).
#[tauri::command]
pub fn take_pair_link(state: State<'_, AppState>) -> Option<PairPrompt> {
    let raw = state.pending_link.lock().unwrap().take()?;
    Some(PairPrompt::from_link(&raw))
}
