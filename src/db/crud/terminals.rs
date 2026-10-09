use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Open (not closed) terminals, most recently active first. `user_id =
    /// None` lists every user's (admin view).
    pub async fn list_terminals(&self, user_id: Option<&str>) -> anyhow::Result<Vec<Terminal>> {
        let user_id = user_id.map(str::to_string);
        self.with_conn(move |conn| {
            let mut q = terminals::table
                .filter(terminals::closed_at.is_null())
                .into_boxed();
            if let Some(uid) = user_id {
                q = q.filter(terminals::user_id.eq(uid));
            }
            q.select(Terminal::as_select())
                .order(terminals::last_active_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_terminal(&self, id: &str) -> anyhow::Result<Option<Terminal>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            terminals::table
                .find(&id)
                .select(Terminal::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn insert_terminal(&self, row: Terminal) -> anyhow::Result<Terminal> {
        self.with_conn(move |conn| {
            diesel::insert_into(terminals::table)
                .values(&row)
                .execute(conn)?;
            Ok(row)
        })
        .await
    }

    /// Rename. `false` when the id is unknown.
    pub async fn rename_terminal(&self, id: &str, name: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let name = name.to_string();
        self.with_conn(move |conn| {
            let n = diesel::update(terminals::table.find(&id))
                .set(terminals::name.eq(&name))
                .execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    /// Record that the remote shell runs inside tmux (or not), learned on
    /// the first successful connect.
    pub async fn set_terminal_persistent(&self, id: &str, persistent: bool) -> anyhow::Result<()> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(terminals::table.find(&id))
                .set(terminals::persistent.eq(persistent))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Bump `last_active_at` to now.
    pub async fn touch_terminal(&self, id: &str) -> anyhow::Result<()> {
        let id = id.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            diesel::update(terminals::table.find(&id))
                .set(terminals::last_active_at.eq(&now))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Mark closed and drop every tab pointing at it (all users). `false`
    /// when the id is unknown or already closed.
    pub async fn close_terminal(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let n = diesel::update(
                    terminals::table
                        .find(&id)
                        .filter(terminals::closed_at.is_null()),
                )
                .set(terminals::closed_at.eq(&now))
                .execute(conn)?;
                diesel::delete(
                    user_tabs::table
                        .filter(user_tabs::item_type.eq("terminal"))
                        .filter(user_tabs::item_id.eq(&id)),
                )
                .execute(conn)?;
                Ok(n > 0)
            })
        })
        .await
    }
}
