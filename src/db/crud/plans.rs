use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Fetch one plan by id.
    pub async fn get_plan(&self, id: &str) -> anyhow::Result<Option<Plan>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            plans::table
                .find(&id)
                .select(Plan::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Every plan, most recently updated first. Feeds the pickers that let
    /// a user choose an existing plan (e.g. the Document Review wizard's
    /// `plan` source kind) — the other accessors all need a card/session
    /// context the picker doesn't have.
    pub async fn list_plans(&self) -> anyhow::Result<Vec<Plan>> {
        self.with_conn(move |conn| {
            plans::table
                .order(plans::updated_at.desc())
                .select(Plan::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Latest plan authored by a session (newest first).
    pub async fn get_plan_for_session(&self, session_id: &str) -> anyhow::Result<Option<Plan>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            plans::table
                .filter(plans::session_id.eq(&session_id))
                .order(plans::updated_at.desc())
                .select(Plan::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Latest plan attached to a card (newest first).
    pub async fn get_plan_for_card(&self, card_id: &str) -> anyhow::Result<Option<Plan>> {
        let card_id = card_id.to_string();
        self.with_conn(move |conn| {
            plans::table
                .filter(plans::card_id.eq(&card_id))
                .order(plans::updated_at.desc())
                .select(Plan::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Create a plan, or revise the session's existing plan in place.
    ///
    /// `propose_plan` calls this: the first proposal inserts a row; every
    /// later proposal on the same session bumps `version`, updates the
    /// markdown/title, and resets `status` to `proposed` (so a re-proposal
    /// re-opens review). Returns the resulting plan.
    pub async fn upsert_plan(
        &self,
        session_id: &str,
        card_id: Option<&str>,
        project_id: Option<&str>,
        title: &str,
        markdown: &str,
    ) -> anyhow::Result<Plan> {
        let session_id = session_id.to_string();
        let card_id = card_id.map(str::to_string);
        let project_id = project_id.map(str::to_string);
        let title = title.to_string();
        let markdown = markdown.to_string();
        self.with_conn(move |conn| {
            let existing: Option<Plan> = plans::table
                .filter(plans::session_id.eq(&session_id))
                .order(plans::updated_at.desc())
                .select(Plan::as_select())
                .first(conn)
                .optional()?;
            let now = chrono::Utc::now().to_rfc3339();
            if let Some(prev) = existing {
                diesel::update(plans::table.find(&prev.id))
                    .set((
                        plans::title.eq(&title),
                        plans::markdown.eq(&markdown),
                        plans::status.eq("proposed"),
                        plans::version.eq(prev.version + 1),
                        plans::updated_at.eq(&now),
                    ))
                    .execute(conn)?;
                plans::table
                    .find(&prev.id)
                    .select(Plan::as_select())
                    .first(conn)
                    .map_err(Into::into)
            } else {
                let new = NewPlan {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: session_id.clone(),
                    card_id,
                    project_id,
                    title,
                    markdown,
                    status: "proposed".into(),
                    version: 1,
                    created_at: now.clone(),
                    updated_at: now,
                };
                diesel::insert_into(plans::table)
                    .values(&new)
                    .returning(Plan::as_returning())
                    .get_result(conn)
                    .map_err(Into::into)
            }
        })
        .await
    }

    /// Set a plan's lifecycle status.
    pub async fn set_plan_status(&self, id: &str, status: &str) -> anyhow::Result<()> {
        let id = id.to_string();
        let status = status.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            diesel::update(plans::table.find(&id))
                .set((plans::status.eq(&status), plans::updated_at.eq(&now)))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Delete a plan.
    pub async fn delete_plan(&self, id: &str) -> anyhow::Result<usize> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(plans::table.find(&id))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::provider::stream::ModelInfo;

    fn info(caps: &[&str], id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: id.into(),
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            tier: 0,
        }
    }

    #[test]
    fn is_thinking_reads_capability_tags() {
        assert!(info(&["code", "reasoning"], "opus").is_thinking());
        assert!(info(&["thinking"], "llama").is_thinking());
        // The id sniff is retired — providers tag their catalogs instead
        // (Cursor tags `reasoning` from its id convention at build time).
        assert!(!info(&[], "claude-opus-4-8-thinking-high").is_thinking());
        assert!(!info(&["code"], "haiku").is_thinking());
        assert!(!info(&[], "sonnet").is_thinking());
    }

    #[tokio::test]
    async fn upsert_revises_in_place_and_bumps_version() {
        let db = Db::in_memory().unwrap();
        let p1 = db
            .upsert_plan("sess-1", Some("card-1"), Some("proj-1"), "T", "# v1")
            .await
            .unwrap();
        assert_eq!(p1.version, 1);
        assert_eq!(p1.status, "proposed");

        let p2 = db
            .upsert_plan("sess-1", Some("card-1"), Some("proj-1"), "T2", "# v2")
            .await
            .unwrap();
        assert_eq!(p2.id, p1.id, "same session revises in place");
        assert_eq!(p2.version, 2);
        assert_eq!(p2.markdown, "# v2");

        // Reachable by card and by session.
        assert_eq!(
            db.get_plan_for_card("card-1").await.unwrap().unwrap().id,
            p1.id
        );
        assert_eq!(
            db.get_plan_for_session("sess-1").await.unwrap().unwrap().id,
            p1.id
        );
        assert!(db.get_plan_for_card("nope").await.unwrap().is_none());
    }
}
