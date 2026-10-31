use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Insert, or refresh the keys of, the subscription for `new.endpoint`
    /// (a browser re-subscribing hands out the same endpoint again).
    pub async fn create_push_subscription(
        &self,
        new: NewPushSubscription,
    ) -> anyhow::Result<PushSubscription> {
        self.with_conn(move |conn| {
            diesel::insert_into(push_subscriptions::table)
                .values(&new)
                .on_conflict(push_subscriptions::endpoint)
                .do_update()
                .set((
                    push_subscriptions::p256dh.eq(&new.p256dh),
                    push_subscriptions::auth_key.eq(&new.auth_key),
                ))
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
