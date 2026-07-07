use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Every saved system prompt, newest name first is not meaningful here —
    /// order alphabetically by name so the Settings list and the MCP
    /// `list_system_prompts` tool are stable.
    pub async fn list_system_prompts(&self) -> anyhow::Result<Vec<SystemPrompt>> {
        self.with_conn(move |conn| {
            system_prompts::table
                .select(SystemPrompt::as_select())
                .order(system_prompts::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Look up one prompt by id.
    pub async fn get_system_prompt(&self, id: &str) -> anyhow::Result<Option<SystemPrompt>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            system_prompts::table
                .find(&id)
                .select(SystemPrompt::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Look up one prompt by its unique name. This is the resolver the
    /// model-switch / `set_session_system_prompt` paths use to turn a
    /// library name into a prompt body.
    pub async fn get_system_prompt_by_name(
        &self,
        name: &str,
    ) -> anyhow::Result<Option<SystemPrompt>> {
        let name = name.to_string();
        self.with_conn(move |conn| {
            system_prompts::table
                .filter(system_prompts::name.eq(&name))
                .select(SystemPrompt::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Create a prompt. Fails if the name is already taken (the column is
    /// UNIQUE) — callers that want update-on-conflict use
    /// [`upsert_system_prompt_by_name`](Self::upsert_system_prompt_by_name).
    pub async fn create_system_prompt(
        &self,
        name: &str,
        body: &str,
        source_url: Option<&str>,
    ) -> anyhow::Result<SystemPrompt> {
        let row = SystemPrompt {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.trim().to_string(),
            body: body.to_string(),
            source_url: source_url.map(|s| s.to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        let new = NewSystemPrompt {
            id: row.id.clone(),
            name: row.name.clone(),
            body: row.body.clone(),
            source_url: row.source_url.clone(),
            created_at: row.created_at.clone(),
            updated_at: row.updated_at.clone(),
        };
        self.with_conn(move |conn| {
            diesel::insert_into(system_prompts::table)
                .values(&new)
                .execute(conn)?;
            Ok(row)
        })
        .await
    }

    /// Update a prompt's name and/or body/source by id. `None` fields are
    /// left untouched. Returns the updated row, or `None` if it's gone.
    pub async fn update_system_prompt(
        &self,
        id: &str,
        name: Option<&str>,
        body: Option<&str>,
        source_url: Option<Option<&str>>,
    ) -> anyhow::Result<Option<SystemPrompt>> {
        let id = id.to_string();
        let name = name.map(|s| s.trim().to_string());
        let body = body.map(|s| s.to_string());
        let source_url = source_url.map(|o| o.map(|s| s.to_string()));
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            let existing: Option<SystemPrompt> = system_prompts::table
                .find(&id)
                .select(SystemPrompt::as_select())
                .first(conn)
                .optional()?;
            let Some(existing) = existing else {
                return Ok(None);
            };
            let updated = SystemPrompt {
                name: name.unwrap_or(existing.name),
                body: body.unwrap_or(existing.body),
                source_url: source_url.unwrap_or(existing.source_url),
                updated_at: now,
                ..existing
            };
            diesel::update(system_prompts::table.find(&id))
                .set((
                    system_prompts::name.eq(&updated.name),
                    system_prompts::body.eq(&updated.body),
                    system_prompts::source_url.eq(&updated.source_url),
                    system_prompts::updated_at.eq(&updated.updated_at),
                ))
                .execute(conn)?;
            Ok(Some(updated))
        })
        .await
    }

    /// Import-friendly upsert keyed by name: create a new prompt, or update
    /// the body/source of an existing one with the same name in place. Used
    /// by the URL-import path so re-importing a prompt refreshes it.
    pub async fn upsert_system_prompt_by_name(
        &self,
        name: &str,
        body: &str,
        source_url: Option<&str>,
    ) -> anyhow::Result<SystemPrompt> {
        let name = name.trim().to_string();
        let body = body.to_string();
        let source_url = source_url.map(|s| s.to_string());
        let now = chrono::Utc::now().to_rfc3339();
        let new_id = uuid::Uuid::new_v4().to_string();
        self.with_conn(move |conn| {
            let existing: Option<SystemPrompt> = system_prompts::table
                .filter(system_prompts::name.eq(&name))
                .select(SystemPrompt::as_select())
                .first(conn)
                .optional()?;
            if let Some(existing) = existing {
                let updated = SystemPrompt {
                    body,
                    source_url,
                    updated_at: now,
                    ..existing
                };
                diesel::update(system_prompts::table.find(&updated.id))
                    .set((
                        system_prompts::body.eq(&updated.body),
                        system_prompts::source_url.eq(&updated.source_url),
                        system_prompts::updated_at.eq(&updated.updated_at),
                    ))
                    .execute(conn)?;
                Ok(updated)
            } else {
                let row = SystemPrompt {
                    id: new_id,
                    name,
                    body,
                    source_url,
                    created_at: now.clone(),
                    updated_at: now,
                };
                let insert = NewSystemPrompt {
                    id: row.id.clone(),
                    name: row.name.clone(),
                    body: row.body.clone(),
                    source_url: row.source_url.clone(),
                    created_at: row.created_at.clone(),
                    updated_at: row.updated_at.clone(),
                };
                diesel::insert_into(system_prompts::table)
                    .values(&insert)
                    .execute(conn)?;
                Ok(row)
            }
        })
        .await
    }

    /// Delete a prompt by id. Idempotent — `false` when nothing was removed.
    pub async fn delete_system_prompt(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(system_prompts::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Seed the built-in default prompts on first run. No-op once the
    /// library holds any entry, so a user who deletes a default won't have
    /// it resurrected on the next boot. Returns how many were inserted.
    pub async fn seed_default_system_prompts(&self) -> anyhow::Result<usize> {
        if !self.list_system_prompts().await?.is_empty() {
            return Ok(0);
        }
        let defaults: &[(&str, &str)] = &[
            (
                "implement",
                "You are implementing a well-scoped code change. Follow the \
                 existing plan and the surrounding code's conventions. Make \
                 the smallest change that fully satisfies the task, keep the \
                 diff focused, and add tests proportional to the change. Do \
                 not refactor unrelated code.",
            ),
            (
                "research",
                "You are researching a question in this codebase. Read \
                 broadly, trace the relevant code paths, and produce a \
                 concise, well-cited summary (file:line references) of how \
                 things work and what the options are. Do not change code.",
            ),
            (
                "debug",
                "You are debugging a defect. Reproduce the failure first, \
                 form a hypothesis, and confirm the root cause before \
                 changing anything. Prefer the minimal fix that addresses \
                 the cause, then verify it and guard it with a regression \
                 test.",
            ),
            (
                "review",
                "You are reviewing a change for correctness, security, and \
                 clarity. Look for real bugs, missing edge cases, and \
                 reuse/simplification opportunities. Be specific and \
                 actionable; distinguish blocking issues from nits.",
            ),
            (
                "docs",
                "You are writing or updating documentation. Be accurate, \
                 concise, and match the existing tone and structure. Prefer \
                 examples over prose, and never document behavior you have \
                 not verified against the code.",
            ),
        ];
        let mut inserted = 0;
        for (name, body) in defaults {
            self.create_system_prompt(name, body, None).await?;
            inserted += 1;
        }
        Ok(inserted)
    }
}
