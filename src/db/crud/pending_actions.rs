use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Park one gated tool call (`service::voice_actions`).
    pub async fn insert_pending_action(&self, row: PendingAction) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(pending_actions::table)
                .values(&row)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    pub async fn get_pending_action(&self, id: &str) -> anyhow::Result<Option<PendingAction>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            pending_actions::table
                .find(&id)
                .select(PendingAction::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// `user_id`'s still-pending, unexpired actions, oldest first. `now` is
    /// in the fixed format `service::voice_actions` writes `expires_at` in.
    pub async fn list_open_pending_actions(
        &self,
        user_id: &str,
        now: &str,
    ) -> anyhow::Result<Vec<PendingAction>> {
        let (user_id, now) = (user_id.to_string(), now.to_string());
        self.with_conn(move |conn| {
            pending_actions::table
                .filter(pending_actions::user_id.eq(&user_id))
                .filter(pending_actions::status.eq("pending"))
                .filter(pending_actions::expires_at.gt(&now))
                .select(PendingAction::as_select())
                .order((pending_actions::created_at.asc(), pending_actions::id.asc()))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Move a `pending`, unexpired action owned by `user_id` to `status` in
    /// ONE conditional UPDATE — the single-use guarantee: of two racing
    /// presses exactly one sees `true`.
    pub async fn resolve_pending_action(
        &self,
        id: &str,
        user_id: &str,
        status: &str,
        now: &str,
    ) -> anyhow::Result<bool> {
        let (id, user_id, status, now) = (
            id.to_string(),
            user_id.to_string(),
            status.to_string(),
            now.to_string(),
        );
        self.with_conn(move |conn| {
            let n = diesel::update(
                pending_actions::table
                    .filter(pending_actions::id.eq(&id))
                    .filter(pending_actions::user_id.eq(&user_id))
                    .filter(pending_actions::status.eq("pending"))
                    .filter(pending_actions::expires_at.gt(&now)),
            )
            .set((
                pending_actions::status.eq(&status),
                pending_actions::resolved_at.eq(Some(&now)),
                pending_actions::resolved_by.eq(Some(&user_id)),
            ))
            .execute(conn)?;
            Ok(n == 1)
        })
        .await
    }

    /// Mark every pending action whose `expires_at` has passed as `expired`.
    pub async fn expire_pending_actions(&self, now: &str) -> anyhow::Result<usize> {
        let now = now.to_string();
        self.with_conn(move |conn| {
            diesel::update(
                pending_actions::table
                    .filter(pending_actions::status.eq("pending"))
                    .filter(pending_actions::expires_at.le(&now)),
            )
            .set((
                pending_actions::status.eq("expired"),
                pending_actions::resolved_at.eq(Some(&now)),
            ))
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Record what a confirmed action returned (audit).
    pub async fn set_pending_action_result(&self, id: &str, result: String) -> anyhow::Result<()> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(pending_actions::table.find(&id))
                .set(pending_actions::result_json.eq(Some(result)))
                .execute(conn)?;
            Ok(())
        })
        .await
    }
}
