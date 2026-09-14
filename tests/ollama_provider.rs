//! Retired. The Ollama provider is a WASM plugin (`peck-plugins/ollama`),
//! not a compiled-in `AgentProvider`. HTTP-protocol coverage belongs in
//! that plugin's repo.

#[test]
fn ollama_provider_is_a_plugin() {
    assert!(std::path::Path::new("peck-plugins/ollama/src/lib.rs").exists());
}
