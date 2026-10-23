//! OAuth `client_credentials` clients for `/mcp` and their access tokens.
//!
//! A client and its backing session are created in one transaction, and the
//! session is created sealed: no agent may ever run in it (see
//! `SessionManager::send_message_locked`), it only anchors the client's
//! tool-call scope and collects what its tools write (ask_user prompts,
//! shared files).

use diesel::prelude::*;
use serde::Serialize;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// A non-revoked client joined with its folder / project names, as served by
/// `GET /api/oauth-clients`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OAuthClientView {
    pub id: String,
    pub name: String,
    pub client_id: String,
    pub folder_id: String,
    pub folder_name: String,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub session_id: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

/// `sessions.sealed_reason` stamped on an OAuth client's backing session.
pub const EXTERNAL_MCP_SEAL_REASON: &str = "external_mcp";

/// `now` formatted the way `oauth_access_tokens.expires_at` is stored:
/// RFC3339 UTC at second precision, fixed width, so `<` / `>` on the text
/// column order by time.
pub fn oauth_ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn view_of(
    conn: &mut SqliteConnection,
    filter_id: Option<&str>,
) -> anyhow::Result<Vec<OAuthClientView>> {
    let mut query = oauth_clients::table
        .inner_join(folders::table.on(folders::id.eq(oauth_clients::folder_id)))
        .left_join(projects::table.on(projects::id.nullable().eq(oauth_clients::project_id)))
        .filter(oauth_clients::revoked_at.is_null())
        .into_boxed();
    if let Some(id) = filter_id {
        query = query.filter(oauth_clients::id.eq(id.to_string()));
    }
    let rows: Vec<(OAuthClient, String, Option<String>)> = query
        .order((oauth_clients::created_at.desc(), oauth_clients::id.desc()))
        .select((
            OAuthClient::as_select(),
            folders::name,
            projects::name.nullable(),
        ))
        .load(conn)?;
    Ok(rows
        .into_iter()
        .map(|(c, folder_name, project_name)| OAuthClientView {
            id: c.id,
            name: c.name,
            client_id: c.client_id,
            folder_id: c.folder_id,
            folder_name,
            project_id: c.project_id,
            project_name,
            session_id: c.session_id,
            created_at: c.created_at,
            last_used_at: c.last_used_at,
        })
        .collect())
}

impl Db {
    /// Insert `client` and its backing `session` atomically, sealing the
    /// session in the same transaction so there is no window in which a
    /// dispatcher could start an agent in it.
    pub async fn create_oauth_client(
        &self,
        client: OAuthClient,
        session: NewSession,
    ) -> anyhow::Result<OAuthClientView> {
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                diesel::insert_into(sessions::table)
                    .values(&session)
                    .execute(conn)?;
                diesel::update(sessions::table.find(&session.id))
                    .set((
                        sessions::sealed_at.eq(Some(&session.created_at)),
                        sessions::sealed_reason.eq(Some(EXTERNAL_MCP_SEAL_REASON)),
                    ))
                    .execute(conn)?;
                diesel::insert_into(oauth_clients::table)
                    .values(&client)
                    .execute(conn)?;
                view_of(conn, Some(&client.id))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("oauth client vanished after insert"))
            })
        })
        .await
    }

    /// Non-revoked clients, newest first.
    pub async fn list_oauth_clients(&self) -> anyhow::Result<Vec<OAuthClientView>> {
        self.with_conn(move |conn| view_of(conn, None)).await
    }

    /// A non-revoked client by its row id.
    pub async fn get_oauth_client(&self, id: &str) -> anyhow::Result<Option<OAuthClient>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            oauth_clients::table
                .find(&id)
                .filter(oauth_clients::revoked_at.is_null())
                .select(OAuthClient::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// A non-revoked client by its public `client_id` (`pbc_…`).
    pub async fn get_oauth_client_by_client_id(
        &self,
        client_id: &str,
    ) -> anyhow::Result<Option<OAuthClient>> {
        let client_id = client_id.to_string();
        self.with_conn(move |conn| {
            oauth_clients::table
                .filter(oauth_clients::client_id.eq(&client_id))
                .filter(oauth_clients::revoked_at.is_null())
                .select(OAuthClient::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Replace a non-revoked client's secret hash and drop every access
    /// token minted under the old secret. `false` when no such client.
    pub async fn rotate_oauth_client_secret(
        &self,
        id: &str,
        secret_hash: &str,
    ) -> anyhow::Result<bool> {
        let id = id.to_string();
        let secret_hash = secret_hash.to_string();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let n = diesel::update(
                    oauth_clients::table
                        .find(&id)
                        .filter(oauth_clients::revoked_at.is_null()),
                )
                .set(oauth_clients::secret_hash.eq(&secret_hash))
                .execute(conn)?;
                if n == 0 {
                    return Ok(false);
                }
                diesel::delete(
                    oauth_access_tokens::table.filter(oauth_access_tokens::oauth_client_id.eq(&id)),
                )
                .execute(conn)?;
                Ok(true)
            })
        })
        .await
    }

    /// Revoke a client: stamp `revoked_at` and delete its access tokens. The
    /// backing session row stays. `false` when no non-revoked client matched.
    pub async fn revoke_oauth_client(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let n = diesel::update(
                    oauth_clients::table
                        .find(&id)
                        .filter(oauth_clients::revoked_at.is_null()),
                )
                .set(oauth_clients::revoked_at.eq(Some(&now)))
                .execute(conn)?;
                diesel::delete(
                    oauth_access_tokens::table.filter(oauth_access_tokens::oauth_client_id.eq(&id)),
                )
                .execute(conn)?;
                Ok(n > 0)
            })
        })
        .await
    }

    /// Store a freshly minted access token, pruning every expired one on the
    /// way (opportunistic cleanup — there is no sweeper).
    pub async fn insert_oauth_access_token(&self, token: OAuthAccessToken) -> anyhow::Result<()> {
        let now = oauth_ts(chrono::Utc::now());
        self.with_conn(move |conn| {
            diesel::delete(
                oauth_access_tokens::table.filter(oauth_access_tokens::expires_at.le(&now)),
            )
            .execute(conn)?;
            diesel::insert_into(oauth_access_tokens::table)
                .values(&token)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Resolve an access token hash to its client: unexpired token, client
    /// not revoked. `None` otherwise.
    pub async fn lookup_oauth_access_token(
        &self,
        token_hash: &str,
    ) -> anyhow::Result<Option<OAuthClient>> {
        let token_hash = token_hash.to_string();
        let now = oauth_ts(chrono::Utc::now());
        self.with_conn(move |conn| {
            oauth_access_tokens::table
                .inner_join(oauth_clients::table)
                .filter(oauth_access_tokens::token_hash.eq(&token_hash))
                .filter(oauth_access_tokens::expires_at.gt(&now))
                .filter(oauth_clients::revoked_at.is_null())
                .select(OAuthClient::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Stamp `last_used_at`, at most once a minute per client (the write is
    /// skipped while the stored stamp is newer than that).
    pub async fn touch_oauth_client(&self, id: &str) -> anyhow::Result<()> {
        let id = id.to_string();
        let now = chrono::Utc::now();
        let stamp = now.to_rfc3339();
        let cutoff = (now - chrono::Duration::seconds(60)).to_rfc3339();
        self.with_conn(move |conn| {
            diesel::update(
                oauth_clients::table.find(&id).filter(
                    oauth_clients::last_used_at
                        .is_null()
                        .or(oauth_clients::last_used_at.lt(&cutoff)),
                ),
            )
            .set(oauth_clients::last_used_at.eq(Some(&stamp)))
            .execute(conn)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(id: &str, folder: &str, session: &str) -> OAuthClient {
        OAuthClient {
            id: id.into(),
            name: format!("client {id}"),
            client_id: format!("pbc_{id}"),
            secret_hash: "h".into(),
            folder_id: folder.into(),
            project_id: None,
            session_id: session.into(),
            created_by: "u1".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[tokio::test]
    async fn client_lifecycle_seals_session_and_expires_tokens() {
        let db = Db::in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder One".into(),
            path: "/tmp/f1".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        let view = db
            .create_oauth_client(
                client("c1", "f1", "s1"),
                NewSession {
                    id: "s1".into(),
                    name: "External: c1".into(),
                    folder_id: "f1".into(),
                    created_at: now.clone(),
                    last_activity: now.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(view.folder_name, "Folder One");
        let s = db.get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.sealed_reason.as_deref(), Some(EXTERNAL_MCP_SEAL_REASON));

        let live = chrono::Utc::now() + chrono::Duration::seconds(3600);
        let dead = chrono::Utc::now() - chrono::Duration::seconds(1);
        for (hash, exp) in [("live", live), ("dead", dead)] {
            db.insert_oauth_access_token(OAuthAccessToken {
                token_hash: hash.into(),
                oauth_client_id: "c1".into(),
                expires_at: oauth_ts(exp),
                created_at: now.clone(),
            })
            .await
            .unwrap();
        }
        assert!(
            db.lookup_oauth_access_token("live")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.lookup_oauth_access_token("dead")
                .await
                .unwrap()
                .is_none()
        );

        assert!(db.revoke_oauth_client("c1").await.unwrap());
        assert!(
            db.lookup_oauth_access_token("live")
                .await
                .unwrap()
                .is_none()
        );
        assert!(db.list_oauth_clients().await.unwrap().is_empty());
        // The session row survives revocation.
        assert!(db.get_session("s1").await.unwrap().is_some());
    }
}
