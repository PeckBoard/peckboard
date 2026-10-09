//! Where a terminal's connection details come from.
//!
//! Hosts (and their credentials) live in plugins — today the `ssh-fleet`
//! plugin's data store. Core never persists a credential: a terminal row
//! holds only `(plugin_id, host_id)`, and every connect — the first one and
//! every automatic reconnect — asks the owning plugin to resolve that
//! reference again through the `terminal.host.resolve` hook. The plugin
//! answers with the same connection shape the `peckboard_ssh_*` host
//! functions take (`host`, `port`, `username`, `auth`, optional
//! `known_host`), which this process consumes in memory and never logs.
//!
//! Both hooks are gated by the plugin's `ssh` permission (the same grant the
//! host functions require); a `key_id` credential additionally needs
//! `ssh_keys`, exactly as it does for `peckboard_ssh_exec`.
//!
//! The trait exists so the terminal driver is testable without a WASM
//! plugin: tests hand it a resolver that returns a throwaway local sshd.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use crate::plugin::hooks::{TERMINAL_HOST_RESOLVE_HOOK, TERMINAL_HOSTS_LIST_HOOK};
use crate::plugin::manager::PluginManager;

/// A host a plugin offers for terminals — identity only, never a
/// credential. Shown in the host picker.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct HostEntry {
    pub plugin_id: String,
    pub id: String,
    pub label: String,
    pub hostname: String,
    pub username: String,
    pub port: u16,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One resolved host reference: the connection fields (with the secret)
/// and the display identity. Lives only for the duration of a connect.
pub struct ResolvedHost {
    /// The `peckboard_ssh_*`-shaped connection object, parsed by
    /// `plugin::ssh::parse_conn`.
    pub conn: serde_json::Value,
    /// Friendly label (the host record's label).
    pub label: String,
    /// Whether the resolving plugin may use a vault key by id.
    pub key_ref_allowed: bool,
}

#[async_trait]
pub trait HostResolver: Send + Sync {
    /// Resolve `host_id` through `plugin_id`. `Err` carries a user-facing
    /// reason (plugin missing, host gone, permission missing).
    async fn resolve(&self, plugin_id: &str, host_id: &str) -> Result<ResolvedHost, String>;
    /// Every host every capable plugin offers.
    async fn list_hosts(&self) -> Vec<HostEntry>;
}

/// The production resolver: dispatches the two terminal hooks to WASM
/// plugins through the [`PluginManager`].
pub struct PluginHostResolver {
    plugins: Arc<PluginManager>,
}

impl PluginHostResolver {
    pub fn new(plugins: Arc<PluginManager>) -> Self {
        Self { plugins }
    }
}

fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[async_trait]
impl HostResolver for PluginHostResolver {
    async fn resolve(&self, plugin_id: &str, host_id: &str) -> Result<ResolvedHost, String> {
        let payload = self
            .plugins
            .call_plugin_hook(
                plugin_id,
                TERMINAL_HOST_RESOLVE_HOOK,
                "ssh",
                serde_json::json!({ "host_id": host_id }),
            )
            .await?;
        let conn = payload
            .get("conn")
            .cloned()
            .filter(serde_json::Value::is_object)
            .ok_or_else(|| format!("plugin '{plugin_id}' returned no connection for the host"))?;
        let label = str_field(&payload, "label");
        let key_ref_allowed = self
            .plugins
            .plugin_has_permission(plugin_id, "ssh_keys")
            .await;
        Ok(ResolvedHost {
            conn,
            label,
            key_ref_allowed,
        })
    }

    async fn list_hosts(&self) -> Vec<HostEntry> {
        let mut out = Vec::new();
        for (plugin_id, payload) in self
            .plugins
            .collect_hook(TERMINAL_HOSTS_LIST_HOOK, "ssh", serde_json::json!({}))
            .await
        {
            let Some(hosts) = payload.get("hosts").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for h in hosts {
                let id = str_field(h, "id");
                let hostname = str_field(h, "hostname");
                if id.is_empty() || hostname.is_empty() {
                    continue;
                }
                let label = {
                    let l = str_field(h, "label");
                    if l.is_empty() { hostname.clone() } else { l }
                };
                out.push(HostEntry {
                    plugin_id: plugin_id.clone(),
                    id,
                    label,
                    hostname,
                    username: str_field(h, "username"),
                    port: h
                        .get("port")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|p| u16::try_from(p).ok())
                        .unwrap_or(22),
                    tags: h
                        .get("tags")
                        .and_then(serde_json::Value::as_array)
                        .map(|t| {
                            t.iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
        }
        out.sort_by_key(|h| h.label.to_lowercase());
        out
    }
}
