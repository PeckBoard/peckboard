//! Role/session hard-gates for MCP tool dispatch, shared by every in-process
//! tool runner. Originally these three predicates (pre-hatcher allowlist,
//! autoswitch gate, worker role gate) lived only in the HTTP `/mcp` route
//! (`routes/mcp.rs`), which every provider except Ollama reaches — Ollama
//! runs tools in-process via `dispatch_tool_call` and consulted none of
//! them, so a worker session on an `ollama:` model could call
//! `delete_project` with no gate at all. `ToolGate` is the single place
//! `tools/list` advertisement and `tools/call` enforcement now derive from,
//! for both the HTTP route and the Ollama provider.

use crate::db::models::Session;

use super::handlers::autoswitch_enabled;
use super::schemas::{
    PRE_HATCHER_EXPERT_KIND, chat_hidden_tool_names, pre_hatcher_allowed_tool_names,
    worker_hidden_tool_names,
};

/// Resolved gate state for one session. Cheap to build (a handful of bools),
/// so callers construct it fresh per turn rather than caching it.
pub struct ToolGate {
    is_worker: bool,
    pre_hatcher: bool,
    doc_review: bool,
    autoswitch_on: bool,
    /// Names of plugin-owned tools this session's role must never dispatch
    /// — populated via [`Self::with_plugin_tools`] from each active plugin's
    /// `mcp_tools()` entries whose `worker_allowed` is `false`. Empty (and so
    /// a no-op) until a caller supplies the plugin tool list; a `ToolGate`
    /// built via `from_session`/`none` alone never blocks a plugin tool by
    /// name, only by the core `worker_hidden_tool_names` list below.
    worker_denied_plugin_tools: std::collections::HashSet<String>,
}

impl ToolGate {
    /// Derive the gate from a session row — the normal case.
    pub fn from_session(session: &Session) -> Self {
        let pre_hatcher = session.expert_kind.as_deref() == Some(PRE_HATCHER_EXPERT_KIND);
        let doc_review =
            session.expert_kind.as_deref() == Some(crate::service::doc_reviews::EXPERT_KIND);
        let autoswitch_on = autoswitch_enabled(session.model_autoswitch, session.is_worker);
        Self {
            is_worker: session.is_worker,
            pre_hatcher,
            doc_review,
            autoswitch_on,
            worker_denied_plugin_tools: std::collections::HashSet::new(),
        }
    }

    /// No session row to derive from (e.g. the row failed to load). Matches
    /// the route's prior `unwrap_or(false)` fallbacks: treat as a plain,
    /// non-worker, non-expert, autoswitch-off session — the most
    /// restrictive of the "harmless" defaults, never the most permissive.
    pub fn none() -> Self {
        Self {
            is_worker: false,
            pre_hatcher: false,
            doc_review: false,
            autoswitch_on: false,
            worker_denied_plugin_tools: std::collections::HashSet::new(),
        }
    }

    /// Fold in the active plugins' declared MCP tools: any entry with
    /// `worker_allowed == false` becomes name-blocked for a worker session
    /// via both [`Self::advertised`] and [`Self::blocked`]. Call once per
    /// gate, after `from_session`/`none`, with
    /// `PluginManager::mcp_tools()`'s result — a no-op for a non-worker gate.
    pub fn with_plugin_tools(
        mut self,
        plugin_tools: &[crate::plugin::hooks::PluginMcpToolEntry],
    ) -> Self {
        self.worker_denied_plugin_tools = plugin_tools
            .iter()
            .filter(|t| !t.worker_allowed)
            .map(|t| t.name.clone())
            .collect();
        self
    }

    /// Should this tool be advertised in `tools/list`? Advertisement is a
    /// context-budget trim, not enforcement — [`Self::blocked`] is the hard
    /// gate consulted at dispatch.
    pub fn advertised(&self, name: &str) -> bool {
        if self.pre_hatcher {
            return pre_hatcher_allowed_tool_names().contains(&name);
        }
        if matches!(name, "get_model_guidance" | "switch_session_model") {
            return self.autoswitch_on;
        }
        if matches!(name, "get_review_doc" | "submit_review_revision") {
            return self.doc_review;
        }
        if self.is_worker && self.worker_denied_plugin_tools.contains(name) {
            return false;
        }
        let hidden: &[&str] = if self.is_worker {
            worker_hidden_tool_names()
        } else {
            chat_hidden_tool_names()
        };
        !hidden.contains(&name)
    }

    /// Hard gate: `Some(reason)` refuses the call outright, whatever the
    /// model asked for by name. Trimming the advertisement alone doesn't
    /// stop a session calling a hidden tool by name, so every in-process
    /// tool runner must consult this before dispatch.
    pub fn blocked(&self, name: &str) -> Option<String> {
        if self.pre_hatcher && !pre_hatcher_allowed_tool_names().contains(&name) {
            return Some(format!(
                "tool '{name}' is blocked: pre-hatcher sessions are \
                 read-only context gatherers. Use the read tools \
                 (read_file, search_files, file_outline, read_symbol, \
                 list_files) and hand off with pre_hatch_result; code \
                 changes are the main model's job."
            ));
        }

        if matches!(name, "get_model_guidance" | "switch_session_model") && !self.autoswitch_on {
            return Some(format!(
                "tool '{name}' is unavailable: model auto-switch is off for this session."
            ));
        }

        if self.is_worker
            && worker_hidden_tool_names().contains(&name)
            && !(self.doc_review && matches!(name, "get_review_doc" | "submit_review_revision"))
        {
            return Some(format!(
                "tool '{name}' is blocked: worker sessions execute their \
                 own card and cannot administer projects, folders, workflows, \
                 schedules, plugins, or other sessions. Use the card tools \
                 (complete_step, finish_card, create_card, …); ask a human \
                 via ask_user for anything else."
            ));
        }

        if self.is_worker && self.worker_denied_plugin_tools.contains(name) {
            return Some(format!(
                "tool '{name}' is blocked: worker sessions cannot use this \
                 plugin-provided administrative tool. Use the card tools \
                 (complete_step, finish_card, create_card, …); ask a human \
                 via ask_user for anything else."
            ));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(is_worker: bool, expert_kind: Option<&str>, autoswitch: Option<bool>) -> Session {
        Session {
            id: "s1".into(),
            name: "s".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: "now".into(),
            last_activity: "now".into(),
            is_expert: expert_kind.is_some(),
            expert_kind: expert_kind.map(|k| k.to_string()),
            knowledge_summary: None,
            knowledge_area: None,
            scope_path: None,
            is_permanent: false,
            repeating_task_id: None,
            system_prompt: None,
            handover_to_model: None,
            handover_run_id: None,
            pending_handover_doc: None,
            worker_step: None,
            user_id: None,
            context_reset_ts: None,
            model_autoswitch: autoswitch,
            system_prompt_name: None,
            pending_plan_review: false,
            pending_doc_review: None,
            is_temp: false,
            parent_session_id: None,
            subagent_completed_at: None,
        }
    }

    fn plugin_tool(name: &str, worker_allowed: bool) -> crate::plugin::hooks::PluginMcpToolEntry {
        crate::plugin::hooks::PluginMcpToolEntry {
            plugin: "test-plugin".into(),
            name: name.into(),
            description: "d".into(),
            input_schema: serde_json::json!({}),
            worker_allowed,
        }
    }

    #[test]
    fn worker_session_is_blocked_from_delete_project() {
        let gate = ToolGate::from_session(&session(true, None, None));
        assert!(gate.blocked("delete_project").is_some());
        assert!(!gate.advertised("delete_project"));
    }

    #[test]
    fn worker_session_can_still_use_card_tools() {
        let gate = ToolGate::from_session(&session(true, None, None));
        assert!(gate.blocked("complete_step").is_none());
        assert!(gate.advertised("complete_step"));
    }

    #[test]
    fn pre_hatcher_session_is_blocked_from_write_file() {
        let gate = ToolGate::from_session(&session(false, Some(PRE_HATCHER_EXPERT_KIND), None));
        assert!(gate.blocked("write_file").is_some());
        assert!(gate.blocked("read_file").is_none());
    }

    #[test]
    fn autoswitch_tools_blocked_when_toggle_is_off() {
        let gate = ToolGate::from_session(&session(false, None, Some(false)));
        assert!(gate.blocked("switch_session_model").is_some());
    }

    #[test]
    fn autoswitch_tools_allowed_when_toggle_is_on() {
        let gate = ToolGate::from_session(&session(false, None, Some(true)));
        assert!(gate.blocked("switch_session_model").is_none());
    }

    #[test]
    fn missing_session_row_falls_back_to_the_restrictive_default() {
        let gate = ToolGate::none();
        assert!(gate.blocked("switch_session_model").is_some());
        assert!(gate.blocked("complete_step").is_none());
    }

    #[test]
    fn worker_session_is_blocked_from_a_worker_denied_plugin_tool() {
        let tools = vec![plugin_tool("clear_session", false)];
        let gate = ToolGate::from_session(&session(true, None, None)).with_plugin_tools(&tools);
        assert!(gate.blocked("clear_session").is_some());
        assert!(!gate.advertised("clear_session"));
    }

    #[test]
    fn worker_session_may_use_a_worker_allowed_plugin_tool() {
        let tools = vec![plugin_tool("read_file", true)];
        let gate = ToolGate::from_session(&session(true, None, None)).with_plugin_tools(&tools);
        assert!(gate.blocked("read_file").is_none());
        assert!(gate.advertised("read_file"));
    }

    #[test]
    fn chat_session_is_unaffected_by_worker_denied_plugin_tools() {
        let tools = vec![plugin_tool("clear_session", false)];
        let gate = ToolGate::from_session(&session(false, None, None)).with_plugin_tools(&tools);
        assert!(gate.blocked("clear_session").is_none());
        assert!(gate.advertised("clear_session"));
    }
}
