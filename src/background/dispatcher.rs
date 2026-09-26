//! The production notifier for background-task reports: an
//! [`ExpertDispatcher`] that holds the `AppState` weakly. `AppState` owns the
//! registry, and the registry owns its dispatcher, so a strong
//! `AppExpertDispatcher` there would be an ownership cycle.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use crate::service::mcp_server::{AppExpertDispatcher, ExpertDispatcher};
use crate::state::AppState;

pub struct WeakAppDispatcher(Weak<AppState>);

impl WeakAppDispatcher {
    pub fn new(state: &Arc<AppState>) -> Arc<dyn ExpertDispatcher> {
        Arc::new(Self(Arc::downgrade(state)))
    }

    fn upgrade(&self) -> anyhow::Result<AppExpertDispatcher> {
        self.0
            .upgrade()
            .map(AppExpertDispatcher::new)
            .ok_or_else(|| anyhow::anyhow!("server is shutting down"))
    }
}

type BoxFut<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

impl ExpertDispatcher for WeakAppDispatcher {
    fn dispatch_capture<'a>(&'a self, expert_session_id: &'a str, prompt: &'a str) -> BoxFut<'a> {
        Box::pin(async move {
            self.upgrade()?
                .dispatch_capture(expert_session_id, prompt)
                .await
        })
    }

    fn resume_session<'a>(&'a self, session_id: &'a str, text: &'a str) -> BoxFut<'a> {
        Box::pin(async move { self.upgrade()?.resume_session(session_id, text).await })
    }

    fn resume_session_appended<'a>(&'a self, session_id: &'a str, text: &'a str) -> BoxFut<'a> {
        Box::pin(async move {
            self.upgrade()?
                .resume_session_appended(session_id, text)
                .await
        })
    }
}
