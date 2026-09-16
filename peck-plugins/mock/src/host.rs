//! FFI layer: Peckboard core host functions this plugin calls. Same envelope
//! contract as the other Rust plugins: JSON-string-in / JSON-string-out, an
//! `{"error": ...}` reply surfaced as `Err(String)`.

#[allow(dead_code)]
pub enum HostFn {
    RegisterProvider,
    EmitProviderEvent,
    ProviderShouldStop,
    ProviderReadStdin,
    ProviderTakeMessage,
    ProviderGetSession,
    ProviderGetMcpConfig,
    ProviderInvokeMcp,
    GetPluginSetting,
    HttpRequest,
    StorePut,
    StoreGet,
    StoreList,
    StoreDelete,
}
impl HostFn {
    pub fn name(self) -> &'static str {
        match self {
            HostFn::RegisterProvider => "peckboard_register_provider",
            HostFn::EmitProviderEvent => "peckboard_emit_provider_event",
            HostFn::ProviderShouldStop => "peckboard_provider_should_stop",
            HostFn::ProviderReadStdin => "peckboard_provider_read_stdin",
            HostFn::ProviderTakeMessage => "peckboard_provider_take_message",
            HostFn::ProviderGetSession => "peckboard_provider_get_session",
            HostFn::ProviderGetMcpConfig => "peckboard_provider_get_mcp_config",
            HostFn::ProviderInvokeMcp => "peckboard_provider_invoke_mcp",
            HostFn::GetPluginSetting => "peckboard_get_plugin_setting",
            HostFn::HttpRequest => "peckboard_http_request",
            HostFn::StorePut => "peckboard_store_put",
            HostFn::StoreGet => "peckboard_store_get",
            HostFn::StoreList => "peckboard_store_list",
            HostFn::StoreDelete => "peckboard_store_delete",
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::HostFn;
    use extism_pdk::*;

    #[host_fn]
    extern "ExtismHost" {
        fn peckboard_register_provider(input: String) -> String;
        fn peckboard_emit_provider_event(input: String) -> String;
        fn peckboard_provider_should_stop(input: String) -> String;
        fn peckboard_provider_read_stdin(input: String) -> String;
        fn peckboard_provider_take_message(input: String) -> String;
        fn peckboard_provider_get_session(input: String) -> String;
        fn peckboard_provider_get_mcp_config(input: String) -> String;
        fn peckboard_provider_invoke_mcp(input: String) -> String;
        fn peckboard_get_plugin_setting(input: String) -> String;
        fn peckboard_http_request(input: String) -> String;
        fn peckboard_store_put(input: String) -> String;
        fn peckboard_store_get(input: String) -> String;
        fn peckboard_store_list(input: String) -> String;
        fn peckboard_store_delete(input: String) -> String;
    }

    pub fn call_host(
        which: HostFn,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let s = input.to_string();
        let out = unsafe {
            match which {
                HostFn::RegisterProvider => peckboard_register_provider(s),
                HostFn::EmitProviderEvent => peckboard_emit_provider_event(s),
                HostFn::ProviderShouldStop => peckboard_provider_should_stop(s),
                HostFn::ProviderReadStdin => peckboard_provider_read_stdin(s),
                HostFn::ProviderTakeMessage => peckboard_provider_take_message(s),
                HostFn::ProviderGetSession => peckboard_provider_get_session(s),
                HostFn::ProviderGetMcpConfig => peckboard_provider_get_mcp_config(s),
                HostFn::ProviderInvokeMcp => peckboard_provider_invoke_mcp(s),
                HostFn::GetPluginSetting => peckboard_get_plugin_setting(s),
                HostFn::HttpRequest => peckboard_http_request(s),
                HostFn::StorePut => peckboard_store_put(s),
                HostFn::StoreGet => peckboard_store_get(s),
                HostFn::StoreList => peckboard_store_list(s),
                HostFn::StoreDelete => peckboard_store_delete(s),
            }
        }
        .map_err(|e| e.to_string())?;
        let v: serde_json::Value =
            serde_json::from_str(&out).map_err(|e| format!("host returned invalid json: {e}"))?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            return Err(err.to_string());
        }
        Ok(v)
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::HostFn;

    pub fn call_host(
        which: HostFn,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        peck_plugin_native_host::call_json(which.name(), input)
    }
}

pub use imp::call_host;
