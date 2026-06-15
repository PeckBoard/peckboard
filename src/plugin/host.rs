//! Data-access host functions exposed to WASM plugins.
//!
//! WASM plugins run fully sandboxed — no filesystem, no network. The only
//! way they can read or write Peckboard data is by calling back through the
//! host functions registered here, which are wired into every loaded plugin
//! in [`crate::plugin::manager::PluginManager`].
//!
//! Each function is JSON-string-in / JSON-string-out and **must never panic
//! across the FFI boundary** — every error path returns an `{"error": ...}`
//! JSON object instead of unwinding. The extism `host_fn!` macro handles the
//! plugin-memory marshalling; the real logic lives in the `*_impl` free
//! functions, which are synchronous (so they can run inside the synchronous
//! extism call without entering the async runtime) and unit-testable on their
//! own with [`crate::db::Db::in_memory`].
//!
//! These functions are intentionally generic and **not** API-key/scope aware:
//! scope enforcement belongs to the plugin that fronts them (e.g. the public
//! API plugin). Note also that, unlike hook registration (gated by
//! `ALLOWED_HOOKS`), there is currently no per-plugin gate on host functions —
//! every loaded `.wasm` plugin can call all of them, including the
//! `peckboard_create_card` write. Anything dropped into `<dataDir>/plugins/`
//! is already trusted to run in-process.
//!
//! The plugin-settings functions are the exception that proves the rule: they
//! are *namespaced* to the calling plugin. Each loaded plugin gets its own
//! host-function set carrying its own id ([`HostState::plugin_id`]), so a
//! plugin can only read and write rows under its own `plugin_id` — it cannot
//! reach another plugin's stored state. The stored values are returned to the
//! owning plugin verbatim (it is the data's owner and needs the real value,
//! e.g. to verify an API key); redaction of secrets only happens at the
//! separate `/api/plugins/:id/settings` HTTP surface, which surfaces values to
//! the browser. These host functions never log stored values.

use extism::*;
use serde::Deserialize;

use crate::db::Db;
use crate::db::models::NewCard;

/// Per-plugin user data shared by all of a single plugin's host functions.
///
/// Carries the live [`Db`] handle plus the **calling plugin's id**, so the
/// plugin-settings functions can scope every read/write to that plugin's own
/// namespace. Each loaded plugin is wired with its own `HostState` (see
/// [`host_functions`]); they are not shared across plugins.
struct HostState {
    db: Db,
    plugin_id: String,
}

/// JSON request for `peckboard_list_cards`. All fields optional; a missing
/// `project_id` lists cards across every project, and `step` filters the
/// result to a single workflow step.
#[derive(Deserialize)]
struct ListCardsRequest {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    step: Option<String>,
}

/// JSON request for `peckboard_create_card`.
#[derive(Deserialize)]
struct CreateCardRequest {
    project_id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    step: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    workflow: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    blocked: Option<bool>,
    #[serde(default)]
    block_reason: Option<String>,
}

/// JSON request for `peckboard_get_plugin_setting` and (the key half of)
/// `peckboard_set_plugin_setting`.
#[derive(Deserialize)]
struct GetPluginSettingRequest {
    key: String,
}

/// JSON request for `peckboard_set_plugin_setting`. A missing or `null`
/// `value` deletes the key (matching the `set_plugin_settings_batch`
/// convention), so the schema default — if any — takes over.
#[derive(Deserialize)]
struct SetPluginSettingRequest {
    key: String,
    #[serde(default)]
    value: serde_json::Value,
}

/// Largest setting key Peckboard will accept from a plugin. Keeps a
/// misbehaving plugin from filling the `plugin_settings` table with
/// pathological keys; comfortably larger than any real key name.
const MAX_SETTING_KEY_LEN: usize = 256;

/// Largest serialized setting value (in bytes) a plugin may store. API
/// keys and small JSON blobs are tiny; this caps a runaway plugin without
/// constraining legitimate use.
const MAX_SETTING_VALUE_LEN: usize = 64 * 1024;

/// The JSON error envelope every host function returns on failure.
fn error_json(msg: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": msg.to_string() }).to_string()
}

/// Reject empty / oversized setting keys before they reach the DB.
fn validate_setting_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("key is required".to_string());
    }
    if key.len() > MAX_SETTING_KEY_LEN {
        return Err(format!(
            "key too long: {} bytes (max {MAX_SETTING_KEY_LEN})",
            key.len()
        ));
    }
    Ok(())
}

/// `peckboard_list_projects` — list every project (read).
pub(crate) fn list_projects_impl(db: &Db) -> String {
    match db.list_projects_blocking() {
        Ok(projects) => serde_json::json!({ "projects": projects }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_list_cards` — list cards, optionally filtered by project and
/// step (read).
pub(crate) fn list_cards_impl(db: &Db, input: &str) -> String {
    let req: ListCardsRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };

    match db.list_cards_blocking(req.project_id.as_deref()) {
        Ok(mut cards) => {
            if let Some(step) = req.step.as_deref() {
                cards.retain(|c| c.step == step);
            }
            serde_json::json!({ "cards": cards }).to_string()
        }
        Err(e) => error_json(e),
    }
}

/// `peckboard_create_card` — create a card on a project (write).
///
/// Mirrors the validation the HTTP route does (priority in the allowed set,
/// project must exist, explicit workflow ids validated, workflow inherited
/// from the project otherwise) but does NOT fire the `card.create.before`
/// hook or broadcast — it is the generic data primitive; policy lives in the
/// calling plugin.
pub(crate) fn create_card_impl(db: &Db, input: &str) -> String {
    let req: CreateCardRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };

    let title = req.title.trim();
    if title.is_empty() {
        return error_json("title is required");
    }

    let priority = req.priority.unwrap_or(2);
    if !crate::routes::misc::is_valid_priority(priority) {
        return error_json(format!(
            "invalid priority: {priority} (allowed: 0=Critical, 1=High, 2=Medium, 3=Low)"
        ));
    }

    // Project must exist; we also need its workflow as the inherited default.
    let project = match db.get_project_blocking(&req.project_id) {
        Ok(Some(p)) => p,
        Ok(None) => return error_json("project not found"),
        Err(e) => return error_json(e),
    };

    // Resolve the card's workflow: validate an explicit non-empty id against
    // the registry, otherwise copy the project's.
    let workflow = match req.workflow.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            if crate::workflow::workflow_by_id(id).is_none() {
                return error_json(format!("unknown workflow id '{id}'"));
            }
            id.to_string()
        }
        _ => project.workflow.clone(),
    };

    // A non-empty block_reason implicitly blocks the card, matching the route.
    let block_reason = req
        .block_reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let blocked = req.blocked.unwrap_or(block_reason.is_some());

    let step = req
        .step
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("backlog")
        .to_string();

    let now = chrono::Utc::now().to_rfc3339();
    let new = NewCard {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: req.project_id.clone(),
        title: title.to_string(),
        description: req.description.unwrap_or_default(),
        step,
        priority,
        workflow,
        model: req.model,
        effort: req.effort,
        blocked,
        block_reason,
        created_at: now.clone(),
        updated_at: now,
    };

    match db.create_card_blocking(&new) {
        Ok(card) => serde_json::json!({ "card": card }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_get_plugin_setting` — read one of the calling plugin's own
/// stored settings (read, namespaced to `plugin_id`).
///
/// Returns the value verbatim — the calling plugin owns this data and needs
/// the real value (e.g. to verify an API key it stored). `{"value": null}`
/// when the key is unset. Never logs the value.
pub(crate) fn get_plugin_setting_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: GetPluginSettingRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Err(e) = validate_setting_key(&req.key) {
        return error_json(e);
    }
    match db.get_plugin_setting_blocking(plugin_id, req.key.trim()) {
        Ok(value) => serde_json::json!({ "value": value }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_set_plugin_setting` — write one of the calling plugin's own
/// stored settings (write, namespaced to `plugin_id`).
///
/// A `null` (or omitted) value deletes the key. Rejects oversized
/// keys/values so a misbehaving plugin can't bloat the table. Returns
/// `{"ok": true}` on success. Never logs the value.
pub(crate) fn set_plugin_setting_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: SetPluginSettingRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Err(e) = validate_setting_key(&req.key) {
        return error_json(e);
    }
    // Bound the stored value. `serde_json::to_string` only fails on
    // non-serializable values, which a parsed `Value` never is.
    if let Ok(encoded) = serde_json::to_string(&req.value)
        && encoded.len() > MAX_SETTING_VALUE_LEN
    {
        return error_json(format!(
            "value too large: {} bytes (max {MAX_SETTING_VALUE_LEN})",
            encoded.len()
        ));
    }
    match db.set_plugin_setting_blocking(plugin_id, req.key.trim(), &req.value) {
        Ok(()) => serde_json::json!({ "ok": true }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_list_plugin_settings` — list all of the calling plugin's own
/// stored settings as a `key → value` object (read, namespaced to
/// `plugin_id`). Values are returned verbatim; never logs them.
pub(crate) fn list_plugin_settings_impl(db: &Db, plugin_id: &str) -> String {
    match db.list_plugin_settings_blocking(plugin_id) {
        Ok(settings) => serde_json::json!({ "settings": settings }).to_string(),
        Err(e) => error_json(e),
    }
}

/// Clone the `Db` and calling plugin id out of the host-function user data
/// without holding the mutex across the (potentially DB-locking) call. A
/// poisoned mutex is surfaced as an `Err` rather than a panic, keeping the
/// FFI boundary safe.
fn state_from(user_data: &UserData<HostState>) -> Result<(Db, String), Error> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    Ok((state.db.clone(), state.plugin_id.clone()))
}

host_fn!(peckboard_list_projects(user_data: HostState; _input: String) -> String {
    let (db, _plugin_id) = state_from(&user_data)?;
    Ok(list_projects_impl(&db))
});

host_fn!(peckboard_list_cards(user_data: HostState; input: String) -> String {
    let (db, _plugin_id) = state_from(&user_data)?;
    Ok(list_cards_impl(&db, &input))
});

host_fn!(peckboard_create_card(user_data: HostState; input: String) -> String {
    let (db, _plugin_id) = state_from(&user_data)?;
    Ok(create_card_impl(&db, &input))
});

host_fn!(peckboard_get_plugin_setting(user_data: HostState; input: String) -> String {
    let (db, plugin_id) = state_from(&user_data)?;
    Ok(get_plugin_setting_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_set_plugin_setting(user_data: HostState; input: String) -> String {
    let (db, plugin_id) = state_from(&user_data)?;
    Ok(set_plugin_setting_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_list_plugin_settings(user_data: HostState; _input: String) -> String {
    let (db, plugin_id) = state_from(&user_data)?;
    Ok(list_plugin_settings_impl(&db, &plugin_id))
});

/// Build the host-function set a single loaded plugin is wired with. Every
/// function shares one `UserData<HostState>` (a cheap `Arc` clone of the live
/// `Db` plus this plugin's id). `plugin_id` namespaces the plugin-settings
/// functions to the caller's own rows — pass the loading plugin's id (its
/// `.wasm` file stem, the same id its `plugin_settings` rows are keyed by).
pub(crate) fn host_functions(db: &Db, plugin_id: &str) -> Vec<Function> {
    let ud = UserData::new(HostState {
        db: db.clone(),
        plugin_id: plugin_id.to_string(),
    });
    vec![
        Function::new(
            "peckboard_list_projects",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_projects,
        ),
        Function::new(
            "peckboard_list_cards",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_cards,
        ),
        Function::new(
            "peckboard_create_card",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_create_card,
        ),
        Function::new(
            "peckboard_get_plugin_setting",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_get_plugin_setting,
        ),
        Function::new(
            "peckboard_set_plugin_setting",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_set_plugin_setting,
        ),
        Function::new(
            "peckboard_list_plugin_settings",
            [PTR],
            [PTR],
            ud,
            peckboard_list_plugin_settings,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewProject};

    async fn setup() -> Db {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: String::new(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts,
        })
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn create_then_list_card_roundtrip() {
        let db = setup().await;

        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "Hello", "priority": 1 }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_none(), "unexpected error: {out}");
        assert_eq!(v["card"]["title"], "Hello");
        assert_eq!(v["card"]["project_id"], "p1");
        // Workflow inherited from the project; step defaults to backlog.
        assert_eq!(v["card"]["workflow"], "task");
        assert_eq!(v["card"]["step"], "backlog");

        // Project-scoped list finds it.
        let out = list_cards_impl(&db, &serde_json::json!({ "project_id": "p1" }).to_string());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cards"].as_array().unwrap().len(), 1);

        // Global list (no project filter) finds it too.
        let out = list_cards_impl(&db, "{}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cards"].as_array().unwrap().len(), 1);

        // Step filter that matches nothing returns an empty list.
        let out = list_cards_impl(&db, &serde_json::json!({ "step": "done" }).to_string());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cards"].as_array().unwrap().len(), 0);

        // Projects listing.
        let out = list_projects_impl(&db);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["projects"].as_array().unwrap().len(), 1);
        assert_eq!(v["projects"][0]["id"], "p1");
    }

    #[tokio::test]
    async fn create_card_unknown_project_is_error_not_panic() {
        let db = setup().await;
        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "nope", "title": "x" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"], "project not found");
    }

    #[tokio::test]
    async fn invalid_priority_and_workflow_are_errors() {
        let db = setup().await;

        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "x", "priority": 99 }).to_string(),
        );
        assert!(out.contains("invalid priority"), "got: {out}");

        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "x", "workflow": "nope" })
                .to_string(),
        );
        assert!(out.contains("unknown workflow id"), "got: {out}");
    }

    #[tokio::test]
    async fn malformed_json_is_error_not_panic() {
        let db = setup().await;
        assert!(create_card_impl(&db, "not json").contains("invalid request"));
        assert!(list_cards_impl(&db, "not json").contains("invalid request"));
    }

    #[tokio::test]
    async fn plugin_setting_set_get_roundtrip_and_is_plugin_scoped() {
        let db = Db::in_memory().unwrap();

        // Set a value for plugin "api".
        let out = set_plugin_setting_impl(
            &db,
            "api",
            &serde_json::json!({ "key": "keys", "value": [{ "key": "k1", "scope": "read" }] })
                .to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true, "unexpected: {out}");

        // Get it back verbatim (no redaction — the owner needs the real value).
        let out = get_plugin_setting_impl(
            &db,
            "api",
            &serde_json::json!({ "key": "keys" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["value"][0]["key"], "k1");
        assert_eq!(v["value"][0]["scope"], "read");

        // A DIFFERENT plugin sees nothing under the same key — namespaced.
        let out = get_plugin_setting_impl(
            &db,
            "other",
            &serde_json::json!({ "key": "keys" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["value"].is_null(), "cross-plugin read leaked: {out}");

        // list is scoped too: "api" has one key, "other" has none.
        let out = list_plugin_settings_impl(&db, "api");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["settings"].as_object().unwrap().len(), 1);
        let out = list_plugin_settings_impl(&db, "other");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["settings"].as_object().unwrap().len(), 0);

        // A null value deletes the key, so a later get is null again.
        let out = set_plugin_setting_impl(
            &db,
            "api",
            &serde_json::json!({ "key": "keys", "value": null }).to_string(),
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out).unwrap()["ok"],
            true
        );
        let out = get_plugin_setting_impl(
            &db,
            "api",
            &serde_json::json!({ "key": "keys" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["value"].is_null());
    }

    #[tokio::test]
    async fn plugin_setting_validates_inputs_without_panic() {
        let db = Db::in_memory().unwrap();

        // Malformed JSON → error, not panic.
        assert!(get_plugin_setting_impl(&db, "api", "not json").contains("invalid request"));
        assert!(set_plugin_setting_impl(&db, "api", "not json").contains("invalid request"));

        // Empty key is rejected on both read and write.
        assert!(
            get_plugin_setting_impl(&db, "api", &serde_json::json!({ "key": "  " }).to_string())
                .contains("key is required")
        );
        assert!(
            set_plugin_setting_impl(
                &db,
                "api",
                &serde_json::json!({ "key": "", "value": 1 }).to_string()
            )
            .contains("key is required")
        );

        // Oversized key and value are rejected.
        let big_key = "k".repeat(MAX_SETTING_KEY_LEN + 1);
        assert!(
            set_plugin_setting_impl(
                &db,
                "api",
                &serde_json::json!({ "key": big_key, "value": 1 }).to_string()
            )
            .contains("key too long")
        );
        let big_value = "v".repeat(MAX_SETTING_VALUE_LEN + 1);
        assert!(
            set_plugin_setting_impl(
                &db,
                "api",
                &serde_json::json!({ "key": "k", "value": big_value }).to_string()
            )
            .contains("value too large")
        );
    }
}
