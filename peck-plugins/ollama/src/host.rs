//! FFI layer: Peckboard core host functions this plugin calls.

pub enum HostFn {
    RegisterProvider,
    EmitProviderEvent,
    ProviderShouldStop,
    ProviderTakeMessage,
    ProviderGetSession,
    ProviderGetMcpConfig,
    ProviderAccountEnv,
    ProviderWriteFile,
    ProviderSpawn,
    ProviderReadLine,
    ProviderWriteStdin,
    ProviderReadStdin,
    ProviderKill,
    GetPluginSetting,
    HttpRequest,
    StorePut,
    StoreGet,
    StoreList,
    StoreDelete,
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
        fn peckboard_provider_take_message(input: String) -> String;
        fn peckboard_provider_get_session(input: String) -> String;
        fn peckboard_provider_get_mcp_config(input: String) -> String;
        fn peckboard_provider_account_env(input: String) -> String;
        fn peckboard_provider_write_file(input: String) -> String;
        fn peckboard_provider_spawn(input: String) -> String;
        fn peckboard_provider_read_line(input: String) -> String;
        fn peckboard_provider_write_stdin(input: String) -> String;
        fn peckboard_provider_read_stdin(input: String) -> String;
        fn peckboard_provider_kill(input: String) -> String;
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
                HostFn::ProviderTakeMessage => peckboard_provider_take_message(s),
                HostFn::ProviderGetSession => peckboard_provider_get_session(s),
                HostFn::ProviderGetMcpConfig => peckboard_provider_get_mcp_config(s),
                HostFn::ProviderAccountEnv => peckboard_provider_account_env(s),
                HostFn::ProviderWriteFile => peckboard_provider_write_file(s),
                HostFn::ProviderSpawn => peckboard_provider_spawn(s),
                HostFn::ProviderReadLine => peckboard_provider_read_line(s),
                HostFn::ProviderWriteStdin => peckboard_provider_write_stdin(s),
                HostFn::ProviderReadStdin => peckboard_provider_read_stdin(s),
                HostFn::ProviderKill => peckboard_provider_kill(s),
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
        _which: HostFn,
        _input: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        unimplemented!("host calls are only available on wasm32")
    }
}

pub use imp::call_host;
