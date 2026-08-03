//! Cross-entity scan for `@<account_id>`-pinned model references.
//!
//! Deleting a provider account (claude/grok/kimi) removes its row and its
//! isolated CLI config dir, but any session/card/project/repeating-task/
//! queued-turn still holding a `model@account_id` string would otherwise
//! hard-fail from then on ("account not found") instead of falling back to
//! the default login. This module finds those references so the delete
//! route can warn (or, with `?force=true`, rewrite them to the bare model).

use diesel::prelude::*;

use crate::db::Db;
use crate::db::schema::*;

/// What still points at an account, by entity. Session ids are kept (small
/// set, useful for the live-child check); the rest are counts.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AccountModelRefs {
    pub sessions: Vec<String>,
    pub cards: usize,
    pub projects: usize,
    pub repeating_tasks: usize,
    pub queued_messages: usize,
}

impl AccountModelRefs {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.cards == 0
            && self.projects == 0
            && self.repeating_tasks == 0
            && self.queued_messages == 0
    }
}

fn pinned_to(model: &Option<String>, suffix: &str) -> bool {
    model.as_deref().is_some_and(|m| m.ends_with(suffix))
}

/// Strip a trailing `@<account_id>` suffix, leaving the bare model id.
/// Returns `None` unchanged if `model` isn't pinned to `suffix`.
fn strip_suffix(model: &Option<String>, suffix: &str) -> Option<String> {
    model.as_deref().map(|m| match m.strip_suffix(suffix) {
        Some(bare) => bare.to_string(),
        None => m.to_string(),
    })
}

impl Db {
    /// Every session/card/project/repeating-task/queued-message still
    /// pinned to `account_id` via a `model@account_id` (or, for sessions,
    /// `handover_to_model@account_id`) reference.
    pub async fn account_model_refs(&self, account_id: &str) -> anyhow::Result<AccountModelRefs> {
        let account_id = account_id.to_string();
        self.with_conn(move |conn| {
            let suffix = format!("@{account_id}");

            let session_rows: Vec<(String, Option<String>, Option<String>)> = sessions::table
                .select((sessions::id, sessions::model, sessions::handover_to_model))
                .load(conn)?;
            let sessions: Vec<String> = session_rows
                .into_iter()
                .filter(|(_, model, handover)| {
                    pinned_to(model, &suffix) || pinned_to(handover, &suffix)
                })
                .map(|(id, _, _)| id)
                .collect();

            let card_models: Vec<Option<String>> = cards::table.select(cards::model).load(conn)?;
            let cards = card_models.iter().filter(|m| pinned_to(m, &suffix)).count();

            let project_models: Vec<Option<String>> =
                projects::table.select(projects::model).load(conn)?;
            let projects = project_models
                .iter()
                .filter(|m| pinned_to(m, &suffix))
                .count();

            let task_models: Vec<Option<String>> = repeating_tasks::table
                .select(repeating_tasks::model)
                .load(conn)?;
            let repeating_tasks = task_models.iter().filter(|m| pinned_to(m, &suffix)).count();

            let queued_models: Vec<Option<String>> = queued_messages::table
                .select(queued_messages::model)
                .load(conn)?;
            let queued_messages = queued_models
                .iter()
                .filter(|m| pinned_to(m, &suffix))
                .count();

            Ok(AccountModelRefs {
                sessions,
                cards,
                projects,
                repeating_tasks,
                queued_messages,
            })
        })
        .await
    }

    /// Rewrite every `model@account_id` (and session `handover_to_model`)
    /// reference back to the bare model id. Used by a force-delete so rows
    /// left behind resolve through the provider/app default instead of
    /// hard-failing once the account row is gone. Returns the number of
    /// rows touched.
    pub async fn strip_account_model_refs(&self, account_id: &str) -> anyhow::Result<usize> {
        let account_id = account_id.to_string();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let suffix = format!("@{account_id}");
                let mut touched = 0usize;

                let session_rows: Vec<(String, Option<String>, Option<String>)> = sessions::table
                    .select((sessions::id, sessions::model, sessions::handover_to_model))
                    .load(conn)?;
                for (id, model, handover) in session_rows {
                    let new_model = strip_suffix(&model, &suffix);
                    let new_handover = strip_suffix(&handover, &suffix);
                    if new_model != model || new_handover != handover {
                        diesel::update(sessions::table.find(&id))
                            .set((
                                sessions::model.eq(&new_model),
                                sessions::handover_to_model.eq(&new_handover),
                            ))
                            .execute(conn)?;
                        touched += 1;
                    }
                }

                let card_rows: Vec<(String, Option<String>)> =
                    cards::table.select((cards::id, cards::model)).load(conn)?;
                for (id, model) in card_rows {
                    let new_model = strip_suffix(&model, &suffix);
                    if new_model != model {
                        diesel::update(cards::table.find(&id))
                            .set(cards::model.eq(&new_model))
                            .execute(conn)?;
                        touched += 1;
                    }
                }

                let project_rows: Vec<(String, Option<String>)> = projects::table
                    .select((projects::id, projects::model))
                    .load(conn)?;
                for (id, model) in project_rows {
                    let new_model = strip_suffix(&model, &suffix);
                    if new_model != model {
                        diesel::update(projects::table.find(&id))
                            .set(projects::model.eq(&new_model))
                            .execute(conn)?;
                        touched += 1;
                    }
                }

                let task_rows: Vec<(String, Option<String>)> = repeating_tasks::table
                    .select((repeating_tasks::id, repeating_tasks::model))
                    .load(conn)?;
                for (id, model) in task_rows {
                    let new_model = strip_suffix(&model, &suffix);
                    if new_model != model {
                        diesel::update(repeating_tasks::table.find(&id))
                            .set(repeating_tasks::model.eq(&new_model))
                            .execute(conn)?;
                        touched += 1;
                    }
                }

                let queued_rows: Vec<(i64, Option<String>)> = queued_messages::table
                    .select((queued_messages::id, queued_messages::model))
                    .load(conn)?;
                for (id, model) in queued_rows {
                    let new_model = strip_suffix(&model, &suffix);
                    if new_model != model {
                        diesel::update(queued_messages::table.find(id))
                            .set(queued_messages::model.eq(&new_model))
                            .execute(conn)?;
                        touched += 1;
                    }
                }

                Ok(touched)
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewCard, NewFolder, NewProject, NewSession};

    async fn seed_folder(db: &Db, id: &str) {
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: id.into(),
            name: id.into(),
            path: format!("/tmp/account-refs-test/{id}"),
            created_at: ts,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn finds_and_strips_pinned_session_and_card_refs() {
        let db = Db::in_memory().unwrap();
        seed_folder(&db, "f1").await;
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "s1".into(),
            folder_id: "f1".into(),
            model: Some("claude-opus-4-8@acc_1".into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s2".into(),
            name: "s2".into(),
            folder_id: "f1".into(),
            model: Some("claude-opus-4-8@acc_2".into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

        let project = db
            .create_project(NewProject {
                id: "p1".into(),
                name: "p1".into(),
                context: String::new(),
                folder_id: "f1".into(),
                worker_count: 1,
                status: "active".into(),
                workflow: "fast-develop-software".into(),
                model: Some("claude-opus-4-8@acc_1".into()),
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: false,
                worker_communication: false,
                worktree_isolation: false,
                created_at: ts.clone(),
                last_accessed_at: ts.clone(),
                budget_usd_cents: None,
                budget_period: None,
            })
            .await
            .unwrap();
        db.create_card(NewCard {
            id: "c1".into(),
            project_id: project.id.clone(),
            title: "c1".into(),
            description: String::new(),
            step: "backlog".into(),
            priority: 0,
            workflow: "fast-develop-software".into(),
            model: Some("claude-opus-4-8@acc_1".into()),
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts,
            system_prompt_name: None,
        })
        .await
        .unwrap();

        let refs = db.account_model_refs("acc_1").await.unwrap();
        assert_eq!(refs.sessions, vec!["s1".to_string()]);
        assert_eq!(refs.cards, 1);
        assert_eq!(refs.projects, 1);
        assert!(!refs.is_empty());

        let touched = db.strip_account_model_refs("acc_1").await.unwrap();
        assert_eq!(touched, 3);

        let s1 = db.get_session("s1").await.unwrap().unwrap();
        assert_eq!(s1.model.as_deref(), Some("claude-opus-4-8"));
        let s2 = db.get_session("s2").await.unwrap().unwrap();
        assert_eq!(s2.model.as_deref(), Some("claude-opus-4-8@acc_2"));

        let refs_after = db.account_model_refs("acc_1").await.unwrap();
        assert!(refs_after.is_empty());
    }
}
