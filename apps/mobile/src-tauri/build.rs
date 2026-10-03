// App commands are permission-gated (not implicitly open to every origin):
// only capabilities/shell.json grants them, and only to the local shell UI.
// The tunnelled box UI is a remote URL with no capability, so it gets none.
const COMMANDS: &[&str] = &[
    "list_boxes",
    "add_box",
    "rename_box",
    "remove_box",
    "connect_box",
    "disconnect_box",
    "tunnel_status",
    "take_pair_link",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("tauri build");
}
