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
    /// Cancel the operation with a reason. `data` optionally carries a
    /// structured payload for the caller — e.g. the pre-hatcher attaches
    /// `{temp_session_id, model}` so core can copy it onto the `pre-hatch`
    /// placeholder event.
    Cancel {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
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
            data: None,
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
    /// A plugin cancelled the operation. `data` is the structured payload
    /// the plugin attached to its cancel verdict, if any.
    Cancelled {
        plugin: String,
        reason: String,
        data: Option<serde_json::Value>,
    },
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

/// Plugin manifest declaring a plugin's identity and which hooks it
/// handles. `description`, `version`, and `repository` are **required**
/// metadata: every plugin must describe itself, state its version, and
/// point at its source repository so the operator can see — on the plugin's
/// own card — what a plugin is, what release is running, and where it came
/// from. A manifest missing any of them (or leaving it blank) fails to load
/// with a clear error rather than surfacing a blank, anonymous plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Human-readable summary of what the plugin does. Required, non-empty.
    pub description: String,
    /// The plugin's own version string (e.g. `"0.2.0"`). Required,
    /// non-empty. This is the plugin's self-reported version — distinct
    /// from any version the registry advertises for it.
    pub version: String,
    /// URL of the plugin's source repository. Required, non-empty.
    pub repository: String,
    pub hooks: Vec<String>,
    #[serde(default)]
    pub http_routes: Vec<String>,
    /// **Authenticated** routes this plugin serves, dispatched via the
    /// [`HTTP_AUTHED_HOOK`] (`http.request.authed`) under the `/api/plugin-ui/*`
    /// surface (which core guards with `require_auth`). Unlike `http_routes`
    /// (the public, plugin-self-authenticated `/plugin-api/*` surface), these
    /// run on behalf of the **logged-in user**: the plugin's handler receives a
    /// trusted user context and may act under the user's authority (gated by the
    /// `user_authority` permission). Use these for plugin-served app UI that
    /// reads or writes the user's own data.
    #[serde(default)]
    pub ui_routes: Vec<String>,
    /// UI panels this plugin contributes to Peckboard's web app. Each is
    /// surfaced in the `/api/plugins` catalog and rendered by the host as
    /// a sandboxed `<iframe>` pointed at the panel's [`UiPanel::path`].
    /// Generic: core never interprets a panel's contents — the plugin
    /// owns the page (served over its own `/plugin-api/*` surface).
    #[serde(default)]
    pub ui_panels: Vec<UiPanel>,
    /// MCP tools this plugin contributes to the worker MCP server. Each is
    /// merged into the `tools/list` exposed to workers, and a call to one is
    /// dispatched back to this plugin via the terminal [`MCP_TOOL_INVOKE_HOOK`]
    /// (`mcp.tool.invoke`) — so a plugin that declares any `mcp_tools` MUST
    /// also list that hook. Generic: core never interprets a tool's meaning;
    /// it routes the call (with the caller's scoped context) and returns
    /// whatever the plugin produces.
    #[serde(default)]
    pub mcp_tools: Vec<PluginMcpTool>,
    /// Left-rail entries this plugin contributes to the web app. Each opens
    /// the plugin's own `/plugin-api/*` page (same iframe-sandbox model as
    /// [`UiPanel`]). Surfaced in the `/api/plugins` catalog; requires the
    /// `contribute_sidebar` permission. Generic: core renders the button and
    /// embeds the page, nothing more.
    #[serde(default)]
    pub sidebar_items: Vec<SidebarItem>,
    /// Full-page entries this plugin contributes to a **project** page. Same
    /// iframe-sandbox model and `SidebarItem` shape as `sidebar_items`, but
    /// rendered as a tab/section inside a single project's view. When the page
    /// calls its `/api/plugin-ui/*` endpoints the host attaches the project's id
    /// so the plugin's scoped host functions run in that project's folder.
    /// Surfaced in the `/api/plugins` catalog; requires `contribute_sidebar`.
    #[serde(default)]
    pub project_items: Vec<SidebarItem>,
    /// Full-page entries this plugin contributes to a **session** page. Same as
    /// [`Self::project_items`] but scoped to a single session — the host
    /// attaches the session's id so scoped host calls run in that session's
    /// folder. Surfaced in the `/api/plugins` catalog; requires
    /// `contribute_sidebar`.
    #[serde(default)]
    pub session_items: Vec<SidebarItem>,
    /// Host capabilities this plugin requests. Each must be in core's
    /// `ALLOWED_PERMISSIONS` allowlist; the granted set gates the host
    /// functions the plugin may call (see `src/plugin/host.rs`). Permissions
    /// are part of what the operator approves — changing them re-prompts —
    /// and a plugin is inert until its hook+permission grant is approved, so
    /// whenever any plugin code runs, every declared permission is granted.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Wall-clock budget for one call into this plugin, in seconds. The
    /// Extism call timeout includes time spent inside host functions, so a
    /// plugin whose tools legitimately wait on slow host-side work (e.g.
    /// `peckboard_http_request` driving a certificate issuance) declares a
    /// larger budget here. Clamped at load to core's [2 s default, 180 s max];
    /// hooks on hot paths (message dispatch) should keep the default.
    #[serde(default)]
    pub call_timeout_secs: Option<u64>,
}

/// One MCP tool a plugin contributes, declared in the manifest's `mcp_tools`.
/// Mirrors core's own `McpToolDef` so the two merge into one `tools/list`.
/// `name` must be unique across core tools and other plugins (collisions are
/// rejected/dropped, never silently shadowed). `input_schema` is the tool's
/// JSON Schema for arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMcpTool {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// A left-rail entry a plugin contributes, declared in the manifest's
/// `sidebar_items`. `id` is the plugin-local stable id (React key / test id);
/// `label` is the button text; `icon` is an optional inline SVG string the
/// host renders sandboxed (no icon → a default placeholder); `path` is the
/// `/plugin-api/*` page opened when clicked (same constraint as
/// [`UiPanel::path`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarItem {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub path: String,
}

/// A validated sidebar entry surfaced in the `/api/plugins` catalog: the
/// declaring plugin plus the entry's metadata.
#[derive(Debug, Clone, Serialize)]
pub struct SidebarItemEntry {
    pub plugin: String,
    pub id: String,
    pub label: String,
    pub icon: Option<String>,
    pub path: String,
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

/// A plugin-provided MCP tool merged into the worker `tools/list`, tagged
/// with the plugin that declared it (so a call can be routed back to it via
/// [`MCP_TOOL_INVOKE_HOOK`]). Surfaced by
/// [`crate::plugin::manager::PluginManager::mcp_tools`].
#[derive(Debug, Clone, Serialize)]
pub struct PluginMcpToolEntry {
    /// The loaded plugin that declared this tool (its `.wasm` file stem).
    pub plugin: String,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
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

/// The hook fired to serve an **authenticated** plugin route under
/// `/api/plugin-ui/*` (which core guards with `require_auth`). Same terminal
/// request→response contract as [`HTTP_REQUEST_HOOK`], but the payload carries a
/// trusted `user` block (the `require_auth`-verified user), and core sets a
/// user-authority context so the plugin's handler may call the scoped host
/// functions on the user's behalf. Gated by the `user_authority` permission;
/// see [`crate::plugin::manager::PluginManager::serve_http_authed`].
pub const HTTP_AUTHED_HOOK: &str = "http.request.authed";

/// The hook fired to dispatch an MCP tool call to the plugin that declared
/// the tool in its manifest `mcp_tools`. Like [`HTTP_REQUEST_HOOK`] this is
/// *terminal*: the plugin OWNS the call — it receives `{tool, arguments,
/// context}` and returns the tool result as the payload of a
/// [`Verdict::Allow`] (or a [`Verdict::Cancel`] mapped to a tool error). Core
/// routes the call and enforces the caller's scope; it never interprets the
/// tool's meaning. See [`crate::plugin::manager::PluginManager::invoke_mcp_tool`].
pub const MCP_TOOL_INVOKE_HOOK: &str = "mcp.tool.invoke";

/// The hook fired when a user answers a worker's `ask_user` question. It is a
/// *notification*, not a transform: the verdict is ignored, the operation has
/// already happened. It exists so a plugin (the experts plugin) can feed the
/// Q&A to its question expert without core knowing experts exist. Core fires it
/// under a **user-authority** context (the answering user), so the handler may
/// call the scoped host functions on the user's behalf — same gate as
/// [`HTTP_AUTHED_HOOK`] (`user_authority` permission). Payload:
/// `{ "asker_session_id", "project_id", "qa_text" }`. See
/// [`crate::plugin::manager::PluginManager::dispatch_authed`].
pub const USER_ANSWER_HOOK: &str = "session.user.answer";

/// Fired before an interactive chat message is dispatched to the agent
/// (`POST /api/sessions/:id/message`) — chat sessions only, never workers or
/// experts, and only for plain text turns (attachments pass straight
/// through). Payload: `{ session_id, text, model, effort, cheap_model }`,
/// where `cheap_model` is the pre-hatch model override from Settings when
/// set, otherwise the session provider's cheapest priced model
/// (`provider:model` form) — or null when neither exists.
/// Verdicts: `Allow` with a modified `text` rewrites the message inline;
/// `Cancel` means the plugin took ownership of the turn — core appends a
/// `pre-hatch` placeholder event (merged with the verdict's `data`, e.g.
/// `{temp_session_id, model}`, so the UI can follow the research session
/// live) and does NOT dispatch, and the plugin is expected to append the
/// final `user` event and resume the session when its enrichment finishes.
/// Fired under a **user-authority** context scoped to
/// the chat session (like [`HTTP_AUTHED_HOOK`]), so the handler may create
/// and dispatch helper sessions in the caller's folder — see
/// [`crate::plugin::manager::PluginManager::dispatch_scoped`].
pub const MESSAGE_BEFORE_HOOK: &str = "session.message.before";

/// Fired when the user cancels an in-flight pre-hatch (`POST
/// /api/sessions/:id/prehatch-cancel`). By the time this fires, core has
/// already terminated the temp research session's agent and dismissed the
/// chat's pending question cards — the hook exists so the plugin that owns
/// the pre-hatch can clear its pending records and deliver the parked
/// original message through its normal path. Payload: `{ session_id,
/// temp_session_id, text }` (`text` is the parked original message;
/// `temp_session_id` is null on legacy `pre-ignite` events). Verdicts:
/// `Cancel` means the plugin OWNED the cancel — it delivered the original
/// (or knows it was already delivered), so core must not deliver again;
/// `Allow`/`Skip` (or no listener at all) makes core fall back to
/// delivering the original message itself. Fired under a **user-authority**
/// context scoped to the chat session, like [`MESSAGE_BEFORE_HOOK`].
pub const PREHATCH_CANCEL_HOOK: &str = "session.prehatch.cancel";

/// Fired when the user answers a pre-hatcher question (the opt-in card or the
/// enriched-message approval card) that carries a `redirectSessionId` to the
/// temp research session. Lets the plugin resolve the outcome in CODE — deliver
/// the message, or dispatch the read-only research turn — instead of handing the
/// yes/no decision to the cheap model. Payload: `{ chat_session_id,
/// temp_session_id, token, answer, rejected }` (`answer` is the selected option
/// label, empty when `rejected`). Verdicts: `Cancel` means the plugin OWNED the
/// answer (delivered or dispatched) and core must NOT resume the temp agent with
/// the raw answer; `Allow`/`Skip` (or no listener) makes core fall back to
/// resuming the redirect target as today — e.g. a clarifying-question
/// continuation the research agent must read. Fired under a **user-authority**
/// context scoped to the chat session, like [`PREHATCH_CANCEL_HOOK`].
pub const PREHATCH_ANSWER_HOOK: &str = "session.prehatch.answer";
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_with_required_metadata() {
        let m: PluginManifest = serde_json::from_str(
            r#"{
                "description": "Does a thing",
                "version": "1.2.3",
                "repository": "https://github.com/acme/plugin",
                "hooks": ["http.request.before"]
            }"#,
        )
        .expect("manifest with all required fields should parse");
        assert_eq!(m.description, "Does a thing");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.repository, "https://github.com/acme/plugin");
        // Optional fields default when omitted.
        assert!(m.http_routes.is_empty());
        assert!(m.ui_panels.is_empty());
    }

    #[test]
    fn manifest_parses_mcp_tools_and_defaults_empty() {
        // mcp_tools is optional and defaults to empty.
        let m: PluginManifest = serde_json::from_str(
            r#"{ "description":"d","version":"1","repository":"r","hooks":[] }"#,
        )
        .unwrap();
        assert!(m.mcp_tools.is_empty());
        // When present, name/description/input_schema parse through.
        let m: PluginManifest = serde_json::from_str(
            r#"{ "description":"d","version":"1","repository":"r",
                 "hooks":["mcp.tool.invoke"],
                 "mcp_tools":[{"name":"do_thing","description":"x",
                               "input_schema":{"type":"object"}}] }"#,
        )
        .unwrap();
        assert_eq!(m.mcp_tools.len(), 1);
        assert_eq!(m.mcp_tools[0].name, "do_thing");
        assert!(m.mcp_tools[0].input_schema.is_object());
    }

    #[test]
    fn manifest_rejects_missing_required_field() {
        // No `description`/`version`/`repository` — required, so it must fail
        // to deserialize rather than yield a blank, anonymous plugin.
        let err = serde_json::from_str::<PluginManifest>(r#"{ "hooks": [] }"#)
            .expect_err("manifest without required metadata must not parse");
        assert!(
            err.to_string().contains("description"),
            "error should name the first missing field, got: {err}"
        );
    }
}
