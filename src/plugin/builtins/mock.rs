//! Built-in plugin that exposes the scripted `MockProvider`.
//!
//! Used in dev (no `claude` CLI required) and as the deterministic engine
//! behind e2e tests. Lives in its own plugin to mirror the production
//! "every agent is a plugin" model and so the catalog UI shows what's
//! providing the `mock:*` model ids.

use async_trait::async_trait;
use std::sync::Arc;

use crate::plugin::builtin::{BuiltinPlugin, Permission, PluginInitContext, PluginMetadata};
use crate::provider::mock::{MockProvider, mock_model_infos};
use crate::provider::registry::{ProviderInfo, standard_effort_levels};

pub struct MockPlugin;

#[async_trait]
impl BuiltinPlugin for MockPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "mock".into(),
            display_name: "Mock Provider".into(),
            description: "Scripted in-process provider for dev/test scenarios.".into(),
            version: env!("PECKBOARD_VERSION").into(),
            author: "Peckboard".into(),
            built_in: true,
        }
    }

    fn requested_permissions(&self) -> Vec<Permission> {
        // The mock provider never touches process / network / filesystem;
        // it only synthesizes events. The single permission it needs is
        // RegisterProvider so its scripted models are dispatchable.
        vec![Permission::RegisterProvider]
    }

    async fn init(&self, ctx: &PluginInitContext) -> anyhow::Result<()> {
        ctx.require(Permission::RegisterProvider)?;

        ctx.provider_registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "mock".into(),
                    display_name: "Mock".into(),
                    models: mock_model_infos(),
                    // The mock provider ignores effort at run time, but it's
                    // the deterministic vehicle e2e uses to exercise the
                    // effort picker, so it exposes the standard ladder.
                    effort_levels: standard_effort_levels(),
                },
            )
            .await;

        Ok(())
    }
}
