use diesel::prelude::*;

use super::Db;
use super::models::*;
use super::schema::*;

impl Db {
    // ── Folders ──────────────────────────────────────────────────────

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

    // ── Sessions ─────────────────────────────────────────────────────

    pub async fn create_session(&self, new: NewSession) -> anyhow::Result<Session> {
        self.with_conn(move |conn| {
            diesel::insert_into(sessions::table)
                .values(&new)
                .returning(Session::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_session(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .find(&id)
                .select(Session::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            sessions::table
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_sessions_by_folder(&self, folder_id: &str) -> anyhow::Result<Vec<Session>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::folder_id.eq(&folder_id))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Move all sessions from one folder to another.
    pub async fn move_sessions_to_folder(&self, from_folder_id: &str, to_folder_id: &str) -> anyhow::Result<usize> {
        let from = from_folder_id.to_string();
        let to = to_folder_id.to_string();
        self.with_conn(move |conn| {
            diesel::update(sessions::table.filter(sessions::folder_id.eq(&from)))
                .set(sessions::folder_id.eq(&to))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_worker_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_worker.eq(true))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_plain_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_worker.eq(false))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_plain_sessions_by_folder(&self, folder_id: &str) -> anyhow::Result<Vec<Session>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::folder_id.eq(&folder_id))
                .filter(sessions::is_worker.eq(false))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_session(&self, id: &str, update: UpdateSession) -> anyhow::Result<Option<Session>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(sessions::table.find(&id))
                .set(&update)
                .returning(Session::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_session(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(sessions::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    // ── Projects ─────────────────────────────────────────────────────

    pub async fn create_project(&self, new: NewProject) -> anyhow::Result<Project> {
        self.with_conn(move |conn| {
            diesel::insert_into(projects::table)
                .values(&new)
                .returning(Project::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_project(&self, id: &str) -> anyhow::Result<Option<Project>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            projects::table
                .find(&id)
                .select(Project::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
        self.with_conn(move |conn| {
            projects::table
                .select(Project::as_select())
                .order(projects::last_accessed_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_projects_by_folder(&self, folder_id: &str) -> anyhow::Result<Vec<Project>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            projects::table
                .filter(projects::folder_id.eq(&folder_id))
                .select(Project::as_select())
                .order(projects::last_accessed_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_project(&self, id: &str, update: UpdateProject) -> anyhow::Result<Option<Project>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(projects::table.find(&id))
                .set(&update)
                .returning(Project::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_project(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(projects::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    // ── Cards ────────────────────────────────────────────────────────

    pub async fn create_card(&self, new: NewCard) -> anyhow::Result<Card> {
        self.with_conn(move |conn| {
            diesel::insert_into(cards::table)
                .values(&new)
                .returning(Card::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_card(&self, id: &str) -> anyhow::Result<Option<Card>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            cards::table
                .find(&id)
                .select(Card::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_cards_by_project(&self, project_id: &str) -> anyhow::Result<Vec<Card>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            cards::table
                .filter(cards::project_id.eq(&project_id))
                .select(Card::as_select())
                .order(cards::priority.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_card(&self, id: &str, update: UpdateCard) -> anyhow::Result<Option<Card>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(cards::table.find(&id))
                .set(&update)
                .returning(Card::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_cards_by_project(&self, project_id: &str) -> anyhow::Result<usize> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(cards::table.filter(cards::project_id.eq(&project_id)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_card(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(cards::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    // ── Events ───────────────────────────────────────────────────────

    pub async fn create_event(&self, new: NewEvent) -> anyhow::Result<Event> {
        self.with_conn(move |conn| {
            diesel::insert_into(events::table)
                .values(&new)
                .returning(Event::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_events_by_session(
        &self,
        session_id: &str,
        after_seq: Option<i32>,
    ) -> anyhow::Result<Vec<Event>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let mut query = events::table
                .filter(events::session_id.eq(&session_id))
                .into_boxed();

            if let Some(seq) = after_seq {
                query = query.filter(events::seq.gt(seq));
            }

            query
                .select(Event::as_select())
                .order(events::seq.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_events_by_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(events::table.filter(events::session_id.eq(&session_id)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    // ── Users ────────────────────────────────────────────────────────

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
        self.with_conn(move |conn| {
            users::table
                .count()
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    // ── Auth Sessions ────────────────────────────────────────────────

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

    pub async fn update_auth_session_last_used(&self, id: &str, last_used_at: i64) -> anyhow::Result<bool> {
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

    pub async fn list_auth_sessions_by_user(&self, user_id: &str) -> anyhow::Result<Vec<AuthSession>> {
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

    // ── Push Subscriptions ───────────────────────────────────────────

    pub async fn create_push_subscription(&self, new: NewPushSubscription) -> anyhow::Result<PushSubscription> {
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

    // ── Queued Messages ──────────────────────────────────────────────

    pub async fn upsert_queued_message(&self, new: NewQueuedMessage) -> anyhow::Result<QueuedMessage> {
        self.with_conn(move |conn| {
            diesel::replace_into(queued_messages::table)
                .values(&new)
                .returning(QueuedMessage::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_queued_message(&self, session_id: &str) -> anyhow::Result<Option<QueuedMessage>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            queued_messages::table
                .find(&session_id)
                .select(QueuedMessage::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_queued_message(&self, session_id: &str) -> anyhow::Result<bool> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(queued_messages::table.find(&session_id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    // ── Announcements ────────────────────────────────────────────────

    pub async fn create_announcement(&self, new: NewAnnouncement) -> anyhow::Result<Announcement> {
        self.with_conn(move |conn| {
            diesel::insert_into(announcements::table)
                .values(&new)
                .returning(Announcement::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_announcements(&self) -> anyhow::Result<Vec<Announcement>> {
        self.with_conn(move |conn| {
            announcements::table
                .select(Announcement::as_select())
                .order(announcements::created_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_announcement(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(announcements::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
