//! Trusted crate plugins compiled into the Peckboard binary.
//!
//! These are not WASM downloads: the code ships inside the binary, and
//! "installing" one just activates it — providers register a native
//! [`crate::provider::agent::AgentProvider`], session-control approves its
//! embedded wasm. Nothing is active on a fresh install; the user picks
//! plugins from the registry (which lists them as `kind: "crate"` with no
//! download). Upgrades of working installs seed everything installed so
//! existing sessions keep their providers.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::db::Db;
use crate::plugin::builtin::{
    BuiltinPlugin, BuiltinPluginRegistry, Permission, PluginInitContext, PluginMetadata,
};
use crate::plugin::manager::PluginManager;
use crate::plugin::settings::SettingsSchema;
use crate::provider::crate_provider::{self, CrateHooks};
use crate::provider::registry::ProviderRegistry;

/// Ids that ship as crates. WASM files of the same stem are hidden from the
/// installed-plugins list; the registry lists these ids as installable
/// activations of the compiled-in code, never as downloads.
pub const CRATE_PLUGIN_IDS: &[&str] = &[
    "claude",
    "grok",
    "cursor",
    "kimi",
    "codex",
    "ollama",
    "openai-compat",
    "mock",
    "session-control",
];

pub fn is_crate_plugin_id(id: &str) -> bool {
    CRATE_PLUGIN_IDS.contains(&id)
}

/// First-party *provider* crates that register a native AgentProvider.
/// Their WASM must not be extracted or auto-approved: the crate owns the id.
pub fn is_crate_provider_id(id: &str) -> bool {
    matches!(
        id,
        "claude" | "grok" | "cursor" | "kimi" | "codex" | "ollama" | "openai-compat" | "mock"
    )
}

/// Plugin-store key (ns `core.settings`/`app`) holding the installed crate
/// plugin ids: `{"ids": ["claude", ...]}`. Absent = never seeded.
pub const INSTALLED_KEY: &str = "crate_plugins_installed";

pub fn all_ids() -> HashSet<String> {
    CRATE_PLUGIN_IDS.iter().map(|s| s.to_string()).collect()
}

/// The persisted installed set, or `None` when it has never been seeded.
pub async fn installed_set(db: &Db) -> Option<HashSet<String>> {
    let db = db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(
            crate::routes::settings::SETTINGS_NS,
            crate::routes::settings::SETTINGS_COLLECTION,
            INSTALLED_KEY,
        )
    })
    .await
    .ok()?
    .ok()??;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(
        v.get("ids")?
            .as_array()?
            .iter()
            .filter_map(|x| x.as_str())
            .filter(|id| is_crate_plugin_id(id))
            .map(str::to_string)
            .collect(),
    )
}

/// Persist the installed set.
pub async fn write_installed(db: &Db, set: &HashSet<String>) {
    let mut ids: Vec<&str> = set.iter().map(String::as_str).collect();
    ids.sort_unstable();
    let json = serde_json::json!({ "ids": ids }).to_string();
    let db = db.clone();
    let _ = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(
            crate::routes::settings::SETTINGS_NS,
            crate::routes::settings::SETTINGS_COLLECTION,
            INSTALLED_KEY,
            &json,
        )
    })
    .await;
}

/// Resolve the installed set at boot, seeding it on first evaluation and
/// persisting the decision:
/// - `PECKBOARD_PREINSTALL_PLUGINS` (`all` | `none` | `a,b,c`) overrides the
///   seed — the e2e harness and containerized deploys use this.
/// - A fresh install (bootstrap admin just created) seeds EMPTY: nothing is
///   active until the user installs plugins from the registry.
/// - An upgrade of a working install seeds everything, so existing sessions
///   keep their providers.
pub async fn resolve_installed(db: &Db, fresh_install: bool) -> HashSet<String> {
    if let Some(set) = installed_set(db).await {
        return set;
    }
    let seeded = match std::env::var("PECKBOARD_PREINSTALL_PLUGINS")
        .ok()
        .as_deref()
    {
        Some("all") => all_ids(),
        Some("none") | Some("") => HashSet::new(),
        Some(csv) => csv
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|id| is_crate_plugin_id(id))
            .collect(),
        None if fresh_install => HashSet::new(),
        None => all_ids(),
    };
    write_installed(db, &seeded).await;
    seeded
}

/// The provider hooks for a crate plugin id (`None` for session-control).
pub fn hooks_for(id: &str) -> Option<CrateHooks> {
    SPECS.iter().find(|s| s.id == id).and_then(|s| s.hooks)
}

/// Ids of the crate plugins that are AI providers (everything with hooks).
pub fn provider_ids() -> impl Iterator<Item = &'static str> {
    SPECS.iter().filter(|s| s.hooks.is_some()).map(|s| s.id)
}

/// Live-activate one crate plugin after an install: providers register their
/// native [`AgentProvider`]; session-control approves its embedded wasm.
pub async fn activate(
    id: &str,
    provider_registry: &Arc<ProviderRegistry>,
    db: &Db,
    plugins: &Arc<PluginManager>,
) {
    match hooks_for(id) {
        Some(hooks) => {
            crate_provider::register_crate_provider(provider_registry, plugins.clone(), db, hooks)
                .await;
        }
        None => {
            let _ = plugins.decide(id, true).await;
        }
    }
}

/// Live-deactivate one crate plugin after an uninstall: providers leave the
/// registry (in-flight turns get a stop request); session-control's wasm is
/// denied, which shuts its hooks down but keeps the file for reinstall.
pub async fn deactivate(
    id: &str,
    provider_registry: &Arc<ProviderRegistry>,
    plugins: &Arc<PluginManager>,
) {
    if is_crate_provider_id(id) {
        provider_registry.unregister(id).await;
        plugins.provider_runtime().request_stop_for_plugin(id);
    } else {
        let _ = plugins.decide(id, false).await;
    }
}
/// session-control's own release version (the crate versions independently
/// of core). Read from the committed sidecar next to the embedded wasm —
/// NOT from `peck-plugins/session-control/Cargo.toml`, which is a nested
/// repo that CI checkouts don't have. Re-embedding the wasm must update the
/// sidecar too.
static SESSION_CONTROL_VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let v = include_str!("../../peck-plugins-wasm/session-control.version").trim();
    if v.is_empty() {
        env!("CARGO_PKG_VERSION").to_string()
    } else {
        v.to_string()
    }
});

/// Crate providers that replaced a registry WASM plugin with the same id.
/// Upgraded installs seeded their installed set before the crate existed, so
/// the crate would start inactive while the WASM is now skipped at load —
/// leaving `<id>:*` sessions with no provider.
const SUPERSEDED_WASM_IDS: &[&str] = &["openai-compat"];

/// One-shot adoption: for each superseded id whose `.wasm` is still in the
/// plugins dir, add the crate to the persisted installed set and rename the
/// file to `<id>.wasm.superseded` (kept, never loaded) so a later uninstall
/// of the crate sticks across reboots.
async fn adopt_superseded_wasm(
    db: &Db,
    plugins_dir: &std::path::Path,
    installed: &HashSet<String>,
) -> HashSet<String> {
    let mut set = installed.clone();
    let mut changed = false;
    for id in SUPERSEDED_WASM_IDS {
        let wasm = plugins_dir.join(format!("{id}.wasm"));
        if !wasm.is_file() {
            continue;
        }
        let dest = plugins_dir.join(format!("{id}.wasm.superseded"));
        if let Err(e) = std::fs::rename(&wasm, &dest) {
            tracing::warn!("could not retire superseded plugin {}: {e}", wasm.display());
            continue;
        }
        tracing::info!("registry plugin '{id}' superseded by the built-in provider; activating it");
        changed |= set.insert(id.to_string());
    }
    if changed {
        write_installed(db, &set).await;
    }
    set
}

/// Register every crate plugin into the builtin catalog (all of them — the
/// catalog is the metadata source for both the installed list and the
/// registry browser) and activate the ones in `installed`.
pub async fn register_all(
    registry: &BuiltinPluginRegistry,
    provider_registry: Arc<ProviderRegistry>,
    db: &Db,
    plugins: Arc<PluginManager>,
    installed: &HashSet<String>,
) {
    let adopted = adopt_superseded_wasm(db, &plugins.data_dir().join("plugins"), installed).await;
    let installed = &adopted;
    for spec in SPECS {
        registry
            .register_and_init(
                Arc::new(CratePlugin(spec)),
                provider_registry.clone(),
                db.clone(),
            )
            .await;
        if !installed.contains(spec.id) {
            continue;
        }
        if let Some(hooks) = spec.hooks {
            crate_provider::register_crate_provider(&provider_registry, plugins.clone(), db, hooks)
                .await;
        } else {
            // session-control: approve the embedded wasm so its hooks run.
            let _ = plugins.decide(spec.id, true).await;
        }
    }
    // An uninstalled session-control may still be approved from an earlier
    // life (approval persists); deny it so its hooks stay off.
    if !installed.contains("session-control") {
        let _ = plugins.decide("session-control", false).await;
    }
}

struct Spec {
    id: &'static str,
    display_name: &'static str,
    description: &'static str,
    perms: &'static [Permission],
    hooks: Option<CrateHooks>,
}

struct CratePlugin(&'static Spec);

#[async_trait]
impl BuiltinPlugin for CratePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: self.0.id.into(),
            display_name: self.0.display_name.into(),
            description: self.0.description.into(),
            // Provider crates version with core; session-control is its own
            // release line (nested repo), so surface its crate version.
            version: if self.0.id == "session-control" {
                SESSION_CONTROL_VERSION.clone()
            } else {
                env!("CARGO_PKG_VERSION").into()
            },
            author: "PeckBoard".into(),
            built_in: true,
        }
    }

    fn requested_permissions(&self) -> Vec<Permission> {
        self.0.perms.to_vec()
    }

    fn settings_schema(&self) -> SettingsSchema {
        self.0
            .hooks
            .map(crate_provider::settings_schema)
            .unwrap_or_default()
    }

    async fn init(&self, ctx: &PluginInitContext) -> anyhow::Result<()> {
        for &p in self.0.perms {
            ctx.require(p)?;
        }
        Ok(())
    }
}

const PROVIDER_PERMS: &[Permission] = &[
    Permission::RegisterProvider,
    Permission::SpawnProcess,
    Permission::NetworkAccess,
    Permission::FilesystemRead,
    Permission::FilesystemWrite,
];

/// HTTP-only providers (ollama, openai-compat): no process spawn, no files.
const OLLAMA_PERMS: &[Permission] = &[Permission::RegisterProvider, Permission::NetworkAccess];

const MOCK_PERMS: &[Permission] = &[Permission::RegisterProvider];

const SESSION_CONTROL_PERMS: &[Permission] = &[
    Permission::ProvideMcpTools,
    Permission::SessionControl,
    Permission::AskUser,
    Permission::DataStore,
    Permission::SessionOrchestrate,
    Permission::SessionWrite,
    Permission::ModelsRead,
    Permission::UserAuthority,
    Permission::ContributeSidebar,
];

const CLAUDE: CrateHooks = CrateHooks {
    send: peckboard_claude_plugin::send_turn,
    refresh_models: peckboard_claude_plugin::refresh_models,
    registration: peckboard_claude_plugin::registration,
    manifest_json: peckboard_claude_plugin::manifest_json,
    interrupt_frame: Some(peckboard_claude_plugin::interrupt_frame),
    base_prompt: Some(peckboard_claude_plugin::base_prompt),
};
const GROK: CrateHooks = CrateHooks {
    send: peckboard_grok_plugin::send_turn,
    refresh_models: peckboard_grok_plugin::refresh_models,
    registration: peckboard_grok_plugin::registration,
    manifest_json: peckboard_grok_plugin::manifest_json,
    interrupt_frame: None,
    base_prompt: None,
};
const CURSOR: CrateHooks = CrateHooks {
    send: peckboard_cursor_plugin::send_turn,
    refresh_models: peckboard_cursor_plugin::refresh_models,
    registration: peckboard_cursor_plugin::registration,
    manifest_json: peckboard_cursor_plugin::manifest_json,
    interrupt_frame: None,
    base_prompt: None,
};
const KIMI: CrateHooks = CrateHooks {
    send: peckboard_kimi_plugin::send_turn,
    refresh_models: peckboard_kimi_plugin::refresh_models,
    registration: peckboard_kimi_plugin::registration,
    manifest_json: peckboard_kimi_plugin::manifest_json,
    interrupt_frame: None,
    base_prompt: None,
};
const CODEX: CrateHooks = CrateHooks {
    send: peckboard_codex_plugin::send_turn,
    refresh_models: peckboard_codex_plugin::refresh_models,
    registration: peckboard_codex_plugin::registration,
    manifest_json: peckboard_codex_plugin::manifest_json,
    interrupt_frame: None,
    base_prompt: None,
};
const OLLAMA: CrateHooks = CrateHooks {
    send: peckboard_ollama_plugin::send_turn,
    refresh_models: peckboard_ollama_plugin::refresh_models,
    registration: peckboard_ollama_plugin::registration,
    manifest_json: peckboard_ollama_plugin::manifest_json,
    interrupt_frame: None,
    base_prompt: None,
};
const OPENAI_COMPAT: CrateHooks = CrateHooks {
    send: peckboard_openai_compat_plugin::send_turn,
    refresh_models: peckboard_openai_compat_plugin::refresh_models,
    registration: peckboard_openai_compat_plugin::registration,
    manifest_json: peckboard_openai_compat_plugin::manifest_json,
    interrupt_frame: None,
    base_prompt: None,
};
const MOCK: CrateHooks = CrateHooks {
    send: peckboard_mock_plugin::send_turn,
    refresh_models: peckboard_mock_plugin::refresh_models,
    registration: peckboard_mock_plugin::registration,
    manifest_json: peckboard_mock_plugin::manifest_json,
    interrupt_frame: None,
    base_prompt: None,
};

const SPECS: &[Spec] = &[
    Spec {
        id: "claude",
        display_name: "Claude",
        description: "Drives sessions via the Claude CLI in stream-json mode.",
        perms: PROVIDER_PERMS,
        hooks: Some(CLAUDE),
    },
    Spec {
        id: "grok",
        display_name: "Grok",
        description: "Drives sessions via the Grok CLI in streaming-json mode.",
        perms: PROVIDER_PERMS,
        hooks: Some(GROK),
    },
    Spec {
        id: "cursor",
        display_name: "Cursor",
        description: "Drives sessions through the cursor-agent CLI in print mode.",
        perms: PROVIDER_PERMS,
        hooks: Some(CURSOR),
    },
    Spec {
        id: "kimi",
        display_name: "Kimi Code",
        description: "Drives sessions through Moonshot AI's Kimi Code CLI in prompt mode.",
        perms: PROVIDER_PERMS,
        hooks: Some(KIMI),
    },
    Spec {
        id: "codex",
        display_name: "Codex",
        description: "Drives sessions through the Codex CLI (ChatGPT sign-in).",
        perms: PROVIDER_PERMS,
        hooks: Some(CODEX),
    },
    Spec {
        id: "ollama",
        display_name: "Ollama",
        description: "Drives sessions through an Ollama server's /api/chat endpoint.",
        perms: OLLAMA_PERMS,
        hooks: Some(OLLAMA),
    },
    Spec {
        id: "openai-compat",
        display_name: "OpenAI-Compatible",
        description: "Drives sessions through any OpenAI-compatible /v1/chat/completions server (vLLM, llama.cpp, LM Studio, LocalAI, TGI, SGLang).",
        perms: OLLAMA_PERMS,
        hooks: Some(OPENAI_COMPAT),
    },
    Spec {
        id: "mock",
        display_name: "Mock",
        description: "Scripted in-process provider for dev/test scenarios.",
        perms: MOCK_PERMS,
        hooks: Some(MOCK),
    },
    Spec {
        id: "session-control",
        display_name: "Session Control",
        description: "Control and orchestrate sessions: interrupt, terminate, clear, send messages, plus goal-driven orchestrators.",
        perms: SESSION_CONTROL_PERMS,
        hooks: None,
    },
];

#[cfg(test)]
mod tests {
    /// The committed session-control blob must be the build where hitting
    /// max_fires_per_hour skips instead of pausing forever. A stale
    /// re-embed (the pre-0.4.7 build) carries the old "— auto-paused" log
    /// text. The bare word alone is no proof: the current build keeps an
    /// "auto-paused" literal to heal those legacy pauses.
    #[test]
    fn embedded_session_control_wasm_has_no_auto_pause() {
        let wasm: &[u8] = include_bytes!("../../peck-plugins-wasm/session-control.wasm");
        let has = |needle: &[u8]| wasm.windows(needle.len()).any(|w| w == needle);
        assert!(
            !has("— auto-paused".as_bytes()),
            "stale session-control.wasm: auto-pauses at the cap"
        );
        assert!(has(b"skipping until the window frees up"));
    }

    /// Every provider crate's static registration must pass the same shape
    /// validation boot applies — a failure there is only a log line, and the
    /// provider silently never registers.
    #[test]
    fn every_crate_provider_registration_is_valid() {
        use crate::provider::plugin_provider::{ProviderRegistration, validate_registration};
        for id in super::provider_ids() {
            let hooks = super::hooks_for(id).unwrap();
            let reg: ProviderRegistration = serde_json::from_value((hooks.registration)())
                .unwrap_or_else(|e| panic!("{id}: registration JSON invalid: {e}"));
            assert_eq!(reg.id, id);
            validate_registration(&reg).unwrap_or_else(|e| panic!("{id}: {e}"));
        }
    }

    /// A leftover registry `openai-compat.wasm` activates the built-in crate
    /// once, and is retired so a later uninstall sticks.
    #[tokio::test]
    async fn superseded_wasm_activates_crate_once() {
        let db = crate::db::Db::in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("openai-compat.wasm"), b"x").unwrap();
        let seeded: std::collections::HashSet<String> = ["ollama".to_string()].into();

        let set = super::adopt_superseded_wasm(&db, dir.path(), &seeded).await;
        assert!(set.contains("openai-compat") && set.contains("ollama"));
        assert_eq!(super::installed_set(&db).await.unwrap(), set);
        assert!(!dir.path().join("openai-compat.wasm").exists());
        assert!(dir.path().join("openai-compat.wasm.superseded").exists());

        // Second boot after the user uninstalled it: stays uninstalled.
        let again = super::adopt_superseded_wasm(&db, dir.path(), &seeded).await;
        assert_eq!(again, seeded);
    }
}
