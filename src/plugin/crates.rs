//! Trusted crate plugins compiled into the Peckboard binary.
//!
//! These are not WASM: they appear in the builtin catalog (`/api/plugins`
//! `plugins` array) as always-on, cannot be installed or removed, and win
//! over a leftover `.wasm` of the same id in the Settings list. Provider
//! crates register a native [`crate::provider::agent::AgentProvider`] at
//! boot; session-control stays catalog-only until its NativePlugin lands.

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
/// installed-plugins list and cannot be installed/uninstalled via the registry.
pub const CRATE_PLUGIN_IDS: &[&str] = &[
    "claude",
    "grok",
    "cursor",
    "kimi",
    "codex",
    "ollama",
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
        "claude" | "grok" | "cursor" | "kimi" | "codex" | "ollama" | "mock"
    )
}
/// session-control's own release version, parsed from its nested crate
/// manifest at compile time (the crate versions independently of core).
static SESSION_CONTROL_VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    include_str!("../../peck-plugins/session-control/Cargo.toml")
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("version = \"")
                .and_then(|r| r.strip_suffix('"'))
        })
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_string()
});

/// Register every crate plugin into the builtin catalog and, for provider
/// crates, a native [`AgentProvider`].
pub async fn register_all(
    registry: &BuiltinPluginRegistry,
    provider_registry: Arc<ProviderRegistry>,
    db: &Db,
    plugins: Arc<PluginManager>,
) {
    for spec in SPECS {
        registry
            .register_and_init(
                Arc::new(CratePlugin(spec)),
                provider_registry.clone(),
                db.clone(),
            )
            .await;
        if let Some(hooks) = spec.hooks {
            crate_provider::register_crate_provider(&provider_registry, plugins.clone(), db, hooks)
                .await;
        }
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
};
const GROK: CrateHooks = CrateHooks {
    send: peckboard_grok_plugin::send_turn,
    refresh_models: peckboard_grok_plugin::refresh_models,
    registration: peckboard_grok_plugin::registration,
    manifest_json: peckboard_grok_plugin::manifest_json,
};
const CURSOR: CrateHooks = CrateHooks {
    send: peckboard_cursor_plugin::send_turn,
    refresh_models: peckboard_cursor_plugin::refresh_models,
    registration: peckboard_cursor_plugin::registration,
    manifest_json: peckboard_cursor_plugin::manifest_json,
};
const KIMI: CrateHooks = CrateHooks {
    send: peckboard_kimi_plugin::send_turn,
    refresh_models: peckboard_kimi_plugin::refresh_models,
    registration: peckboard_kimi_plugin::registration,
    manifest_json: peckboard_kimi_plugin::manifest_json,
};
const CODEX: CrateHooks = CrateHooks {
    send: peckboard_codex_plugin::send_turn,
    refresh_models: peckboard_codex_plugin::refresh_models,
    registration: peckboard_codex_plugin::registration,
    manifest_json: peckboard_codex_plugin::manifest_json,
};
const OLLAMA: CrateHooks = CrateHooks {
    send: peckboard_ollama_plugin::send_turn,
    refresh_models: peckboard_ollama_plugin::refresh_models,
    registration: peckboard_ollama_plugin::registration,
    manifest_json: peckboard_ollama_plugin::manifest_json,
};
const MOCK: CrateHooks = CrateHooks {
    send: peckboard_mock_plugin::send_turn,
    refresh_models: peckboard_mock_plugin::refresh_models,
    registration: peckboard_mock_plugin::registration,
    manifest_json: peckboard_mock_plugin::manifest_json,
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
