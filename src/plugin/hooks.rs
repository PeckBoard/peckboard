use serde::{Deserialize, Serialize};

/// Verdict returned by a plugin for a hook call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Allow the operation to proceed, optionally with a modified payload.
    Allow {
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    /// Cancel the operation with a reason.
    Cancel { reason: String },
    /// This plugin has no opinion — pass through unchanged.
    Skip,
}

impl Verdict {
    pub fn allow() -> Self {
        Verdict::Allow { payload: None }
    }

    pub fn allow_modified(payload: serde_json::Value) -> Self {
        Verdict::Allow {
            payload: Some(payload),
        }
    }

    pub fn cancel(reason: impl Into<String>) -> Self {
        Verdict::Cancel {
            reason: reason.into(),
        }
    }

    pub fn skip() -> Self {
        Verdict::Skip
    }
}

/// Result of dispatching a hook to all registered plugins.
#[derive(Debug)]
pub enum HookResult {
    /// All plugins allowed (or skipped). Contains the final payload (possibly modified).
    Allowed(serde_json::Value),
    /// A plugin cancelled the operation.
    Cancelled { plugin: String, reason: String },
}

impl HookResult {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, HookResult::Cancelled { .. })
    }

    pub fn into_payload(self) -> Option<serde_json::Value> {
        match self {
            HookResult::Allowed(v) => Some(v),
            HookResult::Cancelled { .. } => None,
        }
    }
}

/// Plugin manifest declaring which hooks a plugin handles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub hooks: Vec<String>,
    #[serde(default)]
    pub http_routes: Vec<String>,
    /// UI panels this plugin contributes to Peckboard's web app. Each is
    /// surfaced in the `/api/plugins` catalog and rendered by the host as
    /// a sandboxed `<iframe>` pointed at the panel's [`UiPanel::path`].
    /// Generic: core never interprets a panel's contents — the plugin
    /// owns the page (served over its own `/plugin-api/*` surface).
    #[serde(default)]
    pub ui_panels: Vec<UiPanel>,
}

/// A UI panel a plugin contributes to the web app, declared in the
/// plugin manifest's `ui_panels`.
///
/// `id` is the plugin-local panel id (stable, used in test ids and as a
/// React key); `title` is the human label shown in Settings. `path` is
/// the page the host embeds in a sandboxed iframe — it MUST be a
/// same-origin absolute path under the plugin-owned `/plugin-api/`
/// prefix. The catalog ([`crate::plugin::manager::PluginManager::ui_panels`])
/// drops any panel whose path escapes that prefix, so a plugin can't aim
/// the iframe at an external site or an authenticated `/api/*` route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPanel {
    pub id: String,
    pub title: String,
    pub path: String,
}

/// A validated UI panel as surfaced in the `/api/plugins` catalog: the
/// declaring plugin's name plus the panel's metadata. The frontend reads
/// the top-level `ui_panels` array of the catalog response.
#[derive(Debug, Clone, Serialize)]
pub struct UiPanelEntry {
    /// The loaded plugin that declared this panel (its `.wasm` file stem).
    pub plugin: String,
    pub id: String,
    pub title: String,
    pub path: String,
}

/// The hook fired when a plugin is asked to fully serve a public HTTP
/// route mounted under `/plugin-api/*`. See [`crate::plugin::manager::PluginManager::serve_http`]
/// and the "HTTP Route Hooks" section of `docs/architecture/plugins.md`
/// for the full request/response contract.
///
/// This hook is *terminal*: unlike the cancel/modify hooks (which
/// observe an operation core is performing), the plugin here OWNS the
/// route end to end — it receives the request and returns the complete
/// HTTP response. Core does no auth and has no knowledge of the route.
pub const HTTP_REQUEST_HOOK: &str = "http.request.before";

/// The request a plugin receives for a plugin-served HTTP route.
///
/// Serialized as the `payload` of the [`HTTP_REQUEST_HOOK`] hook call.
/// `headers` keys are lowercased (HTTP header names are case-insensitive);
/// duplicate header values are joined with `", "`. `body` is the raw
/// request body decoded as UTF-8 (lossily). `params` holds the path
/// parameters captured from the plugin's matched `http_routes` pattern —
/// e.g. a declared route `GET /plugin-api/cards/:id` matched against
/// `/plugin-api/cards/42` yields `{"id": "42"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginHttpRequest {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
}

/// The response a plugin returns to serve a plugin-owned HTTP route.
///
/// The plugin returns this object as the `payload` of a
/// [`Verdict::Allow`]. `status` defaults to 200. `body` may be a JSON
/// string (sent verbatim) or any other JSON value (serialized to JSON
/// text, with `content-type: application/json` defaulted unless the
/// plugin set one). A [`Verdict::Cancel`] instead maps to a 500 error
/// response carrying the reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginHttpResponse {
    #[serde(default = "default_http_status")]
    pub status: u16,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub body: serde_json::Value,
}

fn default_http_status() -> u16 {
    200
}

/// Outcome of [`crate::plugin::manager::PluginManager::serve_http`].
///
/// The route layer maps `Served` straight to an HTTP response and
/// `NoRoute` to a 404 — no loaded plugin declared a matching route.
#[derive(Debug)]
pub enum PluginHttpOutcome {
    /// A plugin produced (or errored into) a complete HTTP response.
    Served {
        status: u16,
        /// Response headers as `(name, value)` pairs.
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// No loaded plugin claims a route matching this request.
    NoRoute,
}
