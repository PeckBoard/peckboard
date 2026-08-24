use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn user_has_mfa(&self, user_id: &str) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            use diesel::dsl::count_star;
            let n: i64 = mfa_methods::table
                .filter(mfa_methods::user_id.eq(&user_id))
                .select(count_star())
                .first(conn)?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn list_mfa_methods(&self, user_id: &str) -> anyhow::Result<Vec<MfaMethodRow>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            mfa_methods::table
                .filter(mfa_methods::user_id.eq(&user_id))
                .select(MfaMethodRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_mfa_method(
        &self,
        user_id: &str,
        kind: &str,
    ) -> anyhow::Result<Option<MfaMethodRow>> {
        let user_id = user_id.to_string();
        let kind = kind.to_string();
        self.with_conn(move |conn| {
            mfa_methods::table
                .filter(mfa_methods::user_id.eq(&user_id))
                .filter(mfa_methods::kind.eq(&kind))
                .select(MfaMethodRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn insert_mfa_method(&self, new: NewMfaMethod) -> anyhow::Result<MfaMethodRow> {
        self.with_conn(move |conn| {
            diesel::insert_into(mfa_methods::table)
                .values(&new)
                .returning(MfaMethodRow::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn set_mfa_last_timestep(
        &self,
        id: &str,
        last_timestep: i64,
    ) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let n = diesel::update(mfa_methods::table.find(&id))
                .set(mfa_methods::last_timestep.eq(Some(last_timestep)))
                .execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn list_unused_recovery_codes(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<MfaRecoveryCode>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            mfa_recovery_codes::table
                .filter(mfa_recovery_codes::user_id.eq(&user_id))
                .filter(mfa_recovery_codes::used_at.is_null())
                .select(MfaRecoveryCode::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn insert_recovery_codes(&self, rows: Vec<NewMfaRecoveryCode>) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(mfa_recovery_codes::table)
                .values(&rows)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    pub async fn mark_recovery_code_used(&self, id: &str, used_at: String) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let n = diesel::update(mfa_recovery_codes::table.find(&id))
                .set(mfa_recovery_codes::used_at.eq(Some(used_at)))
                .execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn delete_recovery_codes_for_user(&self, user_id: &str) -> anyhow::Result<usize> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(
                mfa_recovery_codes::table.filter(mfa_recovery_codes::user_id.eq(&user_id)),
            )
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }

    pub async fn insert_mfa_challenge(
        &self,
        new: NewMfaChallenge,
    ) -> anyhow::Result<MfaChallengeRow> {
        self.with_conn(move |conn| {
            diesel::insert_into(mfa_challenges::table)
                .values(&new)
                .returning(MfaChallengeRow::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_mfa_challenge_by_token_hash(
        &self,
        token_hash: &str,
    ) -> anyhow::Result<Option<MfaChallengeRow>> {
        let token_hash = token_hash.to_string();
        self.with_conn(move |conn| {
            mfa_challenges::table
                .filter(mfa_challenges::token_hash.eq(&token_hash))
                .select(MfaChallengeRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn consume_mfa_challenge(&self, id: &str, now: i64) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let n = diesel::update(
                mfa_challenges::table
                    .find(&id)
                    .filter(mfa_challenges::consumed_at.is_null()),
            )
            .set(mfa_challenges::consumed_at.eq(Some(now)))
            .execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn bump_mfa_challenge_failure(&self, id: &str) -> anyhow::Result<i32> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(mfa_challenges::table.find(&id))
                .set(mfa_challenges::failures.eq(mfa_challenges::failures + 1))
                .execute(conn)?;
            let row: MfaChallengeRow = mfa_challenges::table
                .find(&id)
                .select(MfaChallengeRow::as_select())
                .first(conn)?;
            Ok(row.failures)
        })
        .await
    }

    pub async fn replace_mfa_pending(&self, new: NewMfaPending) -> anyhow::Result<MfaPending> {
        self.with_conn(move |conn| {
            diesel::delete(mfa_pending::table.filter(mfa_pending::user_id.eq(&new.user_id)))
                .execute(conn)?;
            diesel::insert_into(mfa_pending::table)
                .values(&new)
                .returning(MfaPending::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_mfa_pending(&self, user_id: &str) -> anyhow::Result<Option<MfaPending>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            mfa_pending::table
                .filter(mfa_pending::user_id.eq(&user_id))
                .select(MfaPending::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_mfa_pending(&self, user_id: &str) -> anyhow::Result<usize> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(mfa_pending::table.filter(mfa_pending::user_id.eq(&user_id)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Drop every MFA row for a user (methods, codes, challenges, pending).
    /// Used by password-reset lockout recovery, admin wipe, and user delete.
    pub async fn wipe_user_mfa(&self, user_id: &str) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(mfa_pending::table.filter(mfa_pending::user_id.eq(&user_id)))
                .execute(conn)?;
            diesel::delete(mfa_challenges::table.filter(mfa_challenges::user_id.eq(&user_id)))
                .execute(conn)?;
            diesel::delete(
                mfa_recovery_codes::table.filter(mfa_recovery_codes::user_id.eq(&user_id)),
            )
            .execute(conn)?;
            diesel::delete(mfa_methods::table.filter(mfa_methods::user_id.eq(&user_id)))
                .execute(conn)?;
            Ok(())
        })
        .await
    }
}
