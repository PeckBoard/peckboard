use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Replace the full set of cards `card_id` depends on. Existing edges
    /// for `card_id` are removed and the given set inserted, in one
    /// transaction. Self-edges are silently dropped.
    pub async fn set_card_dependencies(
        &self,
        card_id: &str,
        depends_on: Vec<String>,
    ) -> anyhow::Result<()> {
        let card_id = card_id.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::delete(
                    card_dependencies::table.filter(card_dependencies::card_id.eq(&card_id)),
                )
                .execute(conn)?;

                let rows: Vec<NewCardDependency> = depends_on
                    .into_iter()
                    .filter(|d| d != &card_id)
                    .map(|d| NewCardDependency {
                        card_id: card_id.clone(),
                        depends_on_card_id: d,
                        created_at: now.clone(),
                    })
                    .collect();

                if !rows.is_empty() {
                    diesel::insert_into(card_dependencies::table)
                        .values(&rows)
                        .execute(conn)?;
                }
                Ok(())
            })
        })
        .await
    }

    /// The ids of cards that `card_id` depends on.
    pub async fn list_card_dependencies(&self, card_id: &str) -> anyhow::Result<Vec<String>> {
        let card_id = card_id.to_string();
        self.with_conn(move |conn| {
            card_dependencies::table
                .filter(card_dependencies::card_id.eq(&card_id))
                .select(card_dependencies::depends_on_card_id)
                .order(card_dependencies::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// All dependency edges `(card_id, depends_on_card_id)` whose
    /// dependent card belongs to `project_id`.
    pub async fn list_dependencies_by_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            card_dependencies::table
                .inner_join(cards::table.on(cards::id.eq(card_dependencies::card_id)))
                .filter(cards::project_id.eq(&project_id))
                .select((
                    card_dependencies::card_id,
                    card_dependencies::depends_on_card_id,
                ))
                .load::<(String, String)>(conn)
                .map_err(Into::into)
        })
        .await
    }
}
