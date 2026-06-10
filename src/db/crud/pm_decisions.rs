//! PM-decision log: pending questions and answered decisions, per
//! project. Answered rows are immutable — `supersede_decision` is the
//! ONLY mutation path for changing an answered decision (it inserts a
//! replacement row and marks the old one 'superseded'). Callers gate
//! supersession on user origin.

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Insert a new question awaiting an answer. `asked_by_session_id`
    /// records provenance; `None` means user/PM-initiated.
    pub async fn create_pending_question(
        &self,
        project_id: &str,
        question: &str,
        asked_by_session_id: Option<&str>,
    ) -> anyhow::Result<PmDecision> {
        let new = NewPmDecision {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: project_id.to_string(),
            question: question.to_string(),
            answer: None,
            status: "pending".into(),
            asked_by_session_id: asked_by_session_id.map(str::to_string),
            superseded_by: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            answered_at: None,
        };
        self.with_conn(move |conn| {
            diesel::insert_into(pm_decisions::table)
                .values(&new)
                .returning(PmDecision::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Answer a pending question. Errors if the row doesn't exist or is
    /// no longer pending — answered/superseded rows are immutable here.
    pub async fn answer_question(&self, id: &str, answer: &str) -> anyhow::Result<PmDecision> {
        let id = id.to_string();
        let answer = answer.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            let updated: Option<PmDecision> = diesel::update(
                pm_decisions::table
                    .find(&id)
                    .filter(pm_decisions::status.eq("pending")),
            )
            .set((
                pm_decisions::answer.eq(&answer),
                pm_decisions::status.eq("answered"),
                pm_decisions::answered_at.eq(&now),
            ))
            .returning(PmDecision::as_returning())
            .get_result(conn)
            .optional()?;
            match updated {
                Some(decision) => Ok(decision),
                None => {
                    let status: Option<String> = pm_decisions::table
                        .find(&id)
                        .select(pm_decisions::status)
                        .first(conn)
                        .optional()?;
                    match status {
                        Some(status) => anyhow::bail!(
                            "pm decision {id} is '{status}', not pending; \
                             use supersede_decision to change an answered decision"
                        ),
                        None => anyhow::bail!("pm decision not found: {id}"),
                    }
                }
            }
        })
        .await
    }

    /// Record a question that arrives already answered.
    pub async fn record_decision(
        &self,
        project_id: &str,
        question: &str,
        answer: &str,
        asked_by_session_id: Option<&str>,
    ) -> anyhow::Result<PmDecision> {
        let now = chrono::Utc::now().to_rfc3339();
        let new = NewPmDecision {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: project_id.to_string(),
            question: question.to_string(),
            answer: Some(answer.to_string()),
            status: "answered".into(),
            asked_by_session_id: asked_by_session_id.map(str::to_string),
            superseded_by: None,
            created_at: now.clone(),
            answered_at: Some(now),
        };
        self.with_conn(move |conn| {
            diesel::insert_into(pm_decisions::table)
                .values(&new)
                .returning(PmDecision::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Replace an answered decision: insert the replacement (already
    /// answered) and mark the old row 'superseded', pointing its
    /// `superseded_by` at the new row. Atomic. Errors if the old row is
    /// missing or not 'answered'.
    pub async fn supersede_decision(
        &self,
        old_id: &str,
        new_question: &str,
        new_answer: &str,
    ) -> anyhow::Result<PmDecision> {
        let old_id = old_id.to_string();
        let new_question = new_question.to_string();
        let new_answer = new_answer.to_string();
        let new_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let old: Option<PmDecision> = pm_decisions::table
                    .find(&old_id)
                    .select(PmDecision::as_select())
                    .first(conn)
                    .optional()?;
                let Some(old) = old else {
                    anyhow::bail!("pm decision not found: {old_id}");
                };
                if old.status != "answered" {
                    anyhow::bail!(
                        "pm decision {old_id} is '{status}', not answered; \
                         only answered decisions can be superseded",
                        status = old.status
                    );
                }
                let new = NewPmDecision {
                    id: new_id.clone(),
                    project_id: old.project_id.clone(),
                    question: new_question.clone(),
                    answer: Some(new_answer.clone()),
                    status: "answered".into(),
                    // Supersession is user-originated, not session provenance.
                    asked_by_session_id: None,
                    superseded_by: None,
                    created_at: now.clone(),
                    answered_at: Some(now.clone()),
                };
                let inserted: PmDecision = diesel::insert_into(pm_decisions::table)
                    .values(&new)
                    .returning(PmDecision::as_returning())
                    .get_result(conn)?;
                diesel::update(pm_decisions::table.find(&old_id))
                    .set((
                        pm_decisions::status.eq("superseded"),
                        pm_decisions::superseded_by.eq(&new_id),
                    ))
                    .execute(conn)?;
                Ok(inserted)
            })
        })
        .await
    }

    pub async fn get_pm_decision(&self, id: &str) -> anyhow::Result<Option<PmDecision>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            pm_decisions::table
                .find(&id)
                .select(PmDecision::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Every decision row for a project, regardless of status, oldest
    /// first.
    pub async fn list_pm_decisions_for_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<PmDecision>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            pm_decisions::table
                .filter(pm_decisions::project_id.eq(&project_id))
                .select(PmDecision::as_select())
                .order(pm_decisions::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Questions still awaiting an answer, oldest first.
    pub async fn list_pending_pm_decisions(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<PmDecision>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            pm_decisions::table
                .filter(pm_decisions::project_id.eq(&project_id))
                .filter(pm_decisions::status.eq("pending"))
                .select(PmDecision::as_select())
                .order(pm_decisions::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Current answered decisions, oldest first. Superseded rows are
    /// excluded — their replacement carries the live answer.
    pub async fn list_answered_pm_decisions(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<PmDecision>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            pm_decisions::table
                .filter(pm_decisions::project_id.eq(&project_id))
                .filter(pm_decisions::status.eq("answered"))
                .select(PmDecision::as_select())
                .order(pm_decisions::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn pending_pm_decision_count(&self, project_id: &str) -> anyhow::Result<i64> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            pm_decisions::table
                .filter(pm_decisions::project_id.eq(&project_id))
                .filter(pm_decisions::status.eq("pending"))
                .count()
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }
}
