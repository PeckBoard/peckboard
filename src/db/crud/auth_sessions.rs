use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_auth_session(&self, new: NewAuthSession) -> anyhow::Result<AuthSession> {
        self.with_conn(move |conn| {
            diesel::insert_into(auth_sessions::table)
                .values(&new)
                .returning(AuthSession::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_auth_session(&self, id: &str) -> anyhow::Result<Option<AuthSession>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            auth_sessions::table
                .find(&id)
                .select(AuthSession::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_auth_session_last_used(
        &self,
        id: &str,
        last_used_at: i64,
    ) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::update(auth_sessions::table.find(&id))
                .set(auth_sessions::last_used_at.eq(Some(last_used_at)))
                .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    pub async fn delete_auth_session(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(auth_sessions::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    pub async fn delete_expired_auth_sessions(&self, now: i64) -> anyhow::Result<usize> {
        self.with_conn(move |conn| {
            diesel::delete(auth_sessions::table.filter(auth_sessions::expires_at.lt(now)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_auth_sessions_by_user(&self, user_id: &str) -> anyhow::Result<usize> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(auth_sessions::table.filter(auth_sessions::user_id.eq(&user_id)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_auth_sessions_by_user_except(
        &self,
        user_id: &str,
        except_session_id: &str,
    ) -> anyhow::Result<usize> {
        let user_id = user_id.to_string();
        let except_session_id = except_session_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(
                auth_sessions::table
                    .filter(auth_sessions::user_id.eq(&user_id))
                    .filter(auth_sessions::id.ne(&except_session_id)),
            )
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }

    pub async fn list_auth_sessions_by_user(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<AuthSession>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            auth_sessions::table
                .filter(auth_sessions::user_id.eq(&user_id))
                .select(AuthSession::as_select())
                .order(auth_sessions::created_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }
}
