// No commands are exposed to JavaScript: the app's Rust core is the only
// caller (secure storage must never be reachable from a WebView).
const COMMANDS: &[&str] = &[];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .ios_path("ios")
        .build();
}
