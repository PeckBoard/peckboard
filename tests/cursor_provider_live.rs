//! Retired. The Cursor provider is a WASM plugin (`peck-plugins/cursor`),
//! not a compiled-in `AgentProvider`. Live CLI coverage belongs in that
//! plugin's repo.

#[test]
fn cursor_provider_is_a_plugin() {
    assert!(std::path::Path::new("peck-plugins/cursor/src/lib.rs").exists());
}
