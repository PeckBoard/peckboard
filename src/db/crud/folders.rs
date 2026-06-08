use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_folder(&self, new: NewFolder) -> anyhow::Result<Folder> {
        self.with_conn(move |conn| {
            diesel::insert_into(folders::table)
                .values(&new)
                .returning(Folder::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_folder(&self, id: &str) -> anyhow::Result<Option<Folder>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            folders::table
                .find(&id)
                .select(Folder::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_folders(&self) -> anyhow::Result<Vec<Folder>> {
        self.with_conn(move |conn| {
            folders::table
                .select(Folder::as_select())
                .order(folders::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_folder(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(folders::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
