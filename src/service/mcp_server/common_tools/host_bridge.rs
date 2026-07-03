//! Synchronous shim replacing the WASM plugin's Extism FFI (`host.rs` in the
//! `common-tools` plugin). The plugin called Peckboard core over the FFI
//! boundary; now that these tools live natively in core, the same JSON-in /
//! JSON-out host functions are the `*_impl` free functions in
//! [`crate::plugin::host`] — this bridge calls them directly.
//!
//! Every `*_impl` returns a JSON *string* and, on failure, an `{"error": ...}`
//! envelope instead of trapping; [`HostCtx::call_host`] turns that envelope
//! into an `Err(String)` so tool code can keep using `?`.

use crate::plugin::host::{
    InvocationContext, exec_impl, http_fetch_impl, list_project_files_impl, read_file_impl,
    store_delete_impl, store_get_impl, store_list_impl, store_put_impl, write_file_impl,
};

/// Data-store namespace (the "plugin id" the store impls key on) for this
/// tool family — web page references and run_command approvals live here.
pub const NS: &str = "core.common-tools";

/// Which host function a [`HostCtx::call_host`] targets. The full set of
/// bridged host functions is kept even where a given variant currently has no
/// caller, so the shim stays a faithful, self-contained replacement for the
/// plugin's FFI table.
#[allow(dead_code)]
pub enum HostFn {
    HttpFetch,
    Exec,
    ExecAny,
    ListProjectFiles,
    ReadFile,
    WriteFile,
    StorePut,
    StoreGet,
    StoreDelete,
}

/// The caller context threaded into every host-touching tool: the DB handle
/// plus the trusted [`InvocationContext`] (session/project/folder scope) the
/// file/exec impls resolve the caller's folder from.
pub struct HostCtx<'a> {
    pub db: &'a crate::db::Db,
    pub inv: InvocationContext,
}

impl HostCtx<'_> {
    /// Invoke a host function with a JSON value, parse its JSON reply, and
    /// surface an `{"error": ...}` envelope as `Err(String)`.
    pub fn call_host(
        &self,
        which: HostFn,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let s = input.to_string();
        let out = match which {
            HostFn::HttpFetch => http_fetch_impl(&s),
            HostFn::Exec => exec_impl(self.db, &s, &self.inv, true),
            HostFn::ExecAny => exec_impl(self.db, &s, &self.inv, false),
            HostFn::ListProjectFiles => list_project_files_impl(self.db, &self.inv),
            HostFn::ReadFile => read_file_impl(self.db, &s, &self.inv),
            HostFn::WriteFile => write_file_impl(self.db, &s, &self.inv),
            HostFn::StorePut => store_put_impl(self.db, NS, &s),
            HostFn::StoreGet => store_get_impl(self.db, NS, &s),
            HostFn::StoreDelete => store_delete_impl(self.db, NS, &s),
        };
        parse_envelope(&out)
    }

    /// A random opaque id (page references, approval tokens).
    pub fn gen_id() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    /// List the keys in a data-store collection under [`NS`]. Core's
    /// `store_list_impl` returns `{"items":[{key,value}]}` (the plugin's
    /// helper read the old `{"keys":[...]}` shape); this handles the new one.
    #[allow(dead_code)] // completes the store API surface of the bridge
    pub fn store_list_keys(&self, collection: &str) -> Result<Vec<String>, String> {
        let out = store_list_impl(
            self.db,
            NS,
            &serde_json::json!({ "collection": collection }).to_string(),
        );
        let v = parse_envelope(&out)?;
        Ok(v["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|it| it.get("key").and_then(|k| k.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}

fn parse_envelope(out: &str) -> Result<serde_json::Value, String> {
    let v: serde_json::Value =
        serde_json::from_str(out).map_err(|e| format!("host returned invalid json: {e}"))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    Ok(v)
}
