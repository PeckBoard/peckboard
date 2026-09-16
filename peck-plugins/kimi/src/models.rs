//! Seed catalog copied from `src/provider/kimi/mod.rs::default_models`.

pub const PROVIDER_ID: &str = "kimi";
pub const DISPLAY_NAME: &str = "Kimi Code";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "default",
            "display_name": "Default (Kimi config)",
            "capabilities": ["code"],
            "tier": 0
        }
    ])
}

/// One catalog entry for a discovered alias / additional model id. The
/// display name carries the "(Kimi)" suffix so config aliases read as
/// Kimi's in the shared picker.
pub fn cli_model_json(
    id: &str,
    display_name: Option<&str>,
    capabilities: &[String],
) -> serde_json::Value {
    let display = match display_name {
        Some(d) => format!("{d} (Kimi)"),
        None => format!("{id} (Kimi)"),
    };
    serde_json::json!({
        "id": id,
        "display_name": display,
        "capabilities": capabilities,
        "tier": 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_model_json_keeps_the_kimi_suffix_and_capabilities() {
        let caps = vec!["code".to_string(), "thinking".to_string()];
        let m = cli_model_json("kimi-k2-thinking", Some("Kimi K2 Thinking"), &caps);
        assert_eq!(m["display_name"], "Kimi K2 Thinking (Kimi)");
        assert_eq!(m["capabilities"], serde_json::json!(["code", "thinking"]));

        let bare = cli_model_json("my-alias", None, &["code".to_string()]);
        assert_eq!(bare["display_name"], "my-alias (Kimi)");
    }
}
