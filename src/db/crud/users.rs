use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_user(&self, new: NewUser) -> anyhow::Result<User> {
        self.with_conn(move |conn| {
            diesel::insert_into(users::table)
                .values(&new)
                .returning(User::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_user(&self, id: &str) -> anyhow::Result<Option<User>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            users::table
                .find(&id)
                .select(User::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_user_by_username(&self, username: &str) -> anyhow::Result<Option<User>> {
        let username = username.to_string();
        self.with_conn(move |conn| {
            users::table
                .filter(users::username.eq(&username))
                .select(User::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_users(&self) -> anyhow::Result<Vec<User>> {
        self.with_conn(move |conn| {
            users::table
                .select(User::as_select())
                .order(users::username.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_user(&self, id: &str, update: UpdateUser) -> anyhow::Result<Option<User>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(users::table.find(&id))
                .set(&update)
                .returning(User::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_user(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(users::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    pub async fn count_users(&self) -> anyhow::Result<i64> {
        self.with_conn(move |conn| users::table.count().get_result(conn).map_err(Into::into))
            .await
    }
}
