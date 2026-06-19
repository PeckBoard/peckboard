use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_push_subscription(
        &self,
        new: NewPushSubscription,
    ) -> anyhow::Result<PushSubscription> {
        self.with_conn(move |conn| {
            diesel::insert_into(push_subscriptions::table)
                .values(&new)
                .returning(PushSubscription::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_push_subscriptions(&self) -> anyhow::Result<Vec<PushSubscription>> {
        self.with_conn(move |conn| {
            push_subscriptions::table
                .select(PushSubscription::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_push_subscription(&self, endpoint: &str) -> anyhow::Result<bool> {
        let endpoint = endpoint.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(push_subscriptions::table.find(&endpoint)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
