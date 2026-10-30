//! The voice assistant's topic ring (`voice_topics`), curated by the
//! assistant model through `assistant_topics` (`service::voice_topics`).
//!
//! Every accessor is scoped by the voice `session_id` — an id from another
//! session is simply "not found". At most one topic per session is current:
//! [`Db::focus_voice_topic`] clears the old one in the same transaction, and
//! a partial unique index backs that up.

use diesel::connection::Connection;
use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::VoiceTopic;
use crate::db::schema::voice_topics;

/// Longest topic title, in chars — a label on the ring, not a sentence.
pub const MAX_VOICE_TOPIC_TITLE_CHARS: usize = 60;
/// Longest one-line summary, in chars.
pub const MAX_VOICE_TOPIC_SUMMARY_CHARS: usize = 200;
/// Most open topics one voice session may have on its ring.
pub const MAX_OPEN_VOICE_TOPICS: usize = 12;
/// The topic kinds the table's CHECK accepts.
pub const VOICE_TOPIC_KINDS: &[&str] = &["conversation", "session", "project"];

fn validate_title(raw: &str) -> anyhow::Result<String> {
    let title = raw.trim();
    if title.is_empty() {
        anyhow::bail!("title must not be empty");
    }
    let len = title.chars().count();
    if len > MAX_VOICE_TOPIC_TITLE_CHARS {
        anyhow::bail!(
            "title is {len} chars; the limit is {MAX_VOICE_TOPIC_TITLE_CHARS}. Use a few words."
        );
    }
    Ok(title.to_string())
}

/// `None` for blank; an error when over the cap.
fn validate_summary(raw: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(summary) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let len = summary.chars().count();
    if len > MAX_VOICE_TOPIC_SUMMARY_CHARS {
        anyhow::bail!(
            "summary is {len} chars; the limit is {MAX_VOICE_TOPIC_SUMMARY_CHARS}. Keep it to one line."
        );
    }
    Ok(Some(summary.to_string()))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn load_open(conn: &mut SqliteConnection, session_id: &str) -> QueryResult<Vec<VoiceTopic>> {
    voice_topics::table
        .filter(voice_topics::session_id.eq(session_id))
        .filter(voice_topics::status.eq("open"))
        // Millisecond timestamps tie on quick successive focuses: the
        // current topic is always the latest focus, so it breaks the tie.
        .order((
            voice_topics::updated_at.desc(),
            voice_topics::is_current.desc(),
            voice_topics::id.asc(),
        ))
        .select(VoiceTopic::as_select())
        .load(conn)
}

fn load_one(
    conn: &mut SqliteConnection,
    session_id: &str,
    id: &str,
) -> QueryResult<Option<VoiceTopic>> {
    voice_topics::table
        .filter(voice_topics::id.eq(id))
        .filter(voice_topics::session_id.eq(session_id))
        .select(VoiceTopic::as_select())
        .first(conn)
        .optional()
}

impl Db {
    /// `session_id`'s open topics, most recently changed first.
    pub async fn list_open_voice_topics(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<VoiceTopic>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| load_open(conn, &session_id).map_err(Into::into))
            .await
    }

    /// Add an open topic. Validates kind / title / summary, requires
    /// `ref_id` for `session` / `project` (existence is the caller's check),
    /// refuses a duplicate open topic for the same target, and enforces
    /// [`MAX_OPEN_VOICE_TOPICS`].
    pub async fn add_voice_topic(
        &self,
        session_id: &str,
        kind: &str,
        title: &str,
        summary: Option<&str>,
        ref_id: Option<&str>,
    ) -> anyhow::Result<VoiceTopic> {
        if !VOICE_TOPIC_KINDS.contains(&kind) {
            anyhow::bail!("unknown kind '{kind}'; use conversation, session, or project");
        }
        let ref_id = ref_id.map(str::trim).filter(|r| !r.is_empty());
        let ref_id = match (kind, ref_id) {
            ("conversation", _) => None,
            (_, Some(r)) => Some(r.to_string()),
            (_, None) => anyhow::bail!("a {kind} topic needs ref_id: the {kind}'s id"),
        };
        let row = VoiceTopic {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            title: validate_title(title)?,
            summary: validate_summary(summary)?,
            ref_id,
            status: "open".into(),
            is_current: false,
            created_at: now(),
            updated_at: now(),
        };
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let open = load_open(conn, &row.session_id)?;
                if let Some(dup) = row
                    .ref_id
                    .as_ref()
                    .and_then(|r| open.iter().find(|t| t.ref_id.as_ref() == Some(r)))
                {
                    anyhow::bail!(
                        "that {} is already on the ring as topic {} (\"{}\"); update or focus it instead",
                        row.kind,
                        dup.id,
                        dup.title
                    );
                }
                if open.len() >= MAX_OPEN_VOICE_TOPICS {
                    anyhow::bail!(
                        "the ring already has {MAX_OPEN_VOICE_TOPICS} open topics, the limit. \
                         Close finished ones or merge related ones (update one, close the rest) first."
                    );
                }
                diesel::insert_into(voice_topics::table)
                    .values(&row)
                    .execute(conn)?;
                Ok(row)
            })
        })
        .await
    }

    /// Rename / re-summarise an open topic. `summary: Some("")` clears it.
    /// `Ok(None)` when `id` is not an open topic of `session_id`.
    pub async fn update_voice_topic(
        &self,
        session_id: &str,
        id: &str,
        title: Option<&str>,
        summary: Option<&str>,
    ) -> anyhow::Result<Option<VoiceTopic>> {
        let title = title.map(validate_title).transpose()?;
        let summary = summary.map(|s| validate_summary(Some(s))).transpose()?;
        let (session_id, id) = (session_id.to_string(), id.to_string());
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let Some(mut topic) = load_one(conn, &session_id, &id)? else {
                    return Ok(None);
                };
                if topic.status != "open" {
                    return Ok(None);
                }
                if let Some(t) = title {
                    topic.title = t;
                }
                if let Some(s) = summary {
                    topic.summary = s;
                }
                topic.updated_at = now();
                diesel::update(voice_topics::table.find(&id))
                    .set((
                        voice_topics::title.eq(&topic.title),
                        voice_topics::summary.eq(&topic.summary),
                        voice_topics::updated_at.eq(&topic.updated_at),
                    ))
                    .execute(conn)?;
                Ok(Some(topic))
            })
        })
        .await
    }

    /// Close an open topic (and drop it as current). `Ok(None)` when `id` is
    /// not an open topic of `session_id`.
    pub async fn close_voice_topic(
        &self,
        session_id: &str,
        id: &str,
    ) -> anyhow::Result<Option<VoiceTopic>> {
        let (session_id, id) = (session_id.to_string(), id.to_string());
        self.with_conn(move |conn| {
            let at = now();
            let n = diesel::update(
                voice_topics::table
                    .filter(voice_topics::id.eq(&id))
                    .filter(voice_topics::session_id.eq(&session_id))
                    .filter(voice_topics::status.eq("open")),
            )
            .set((
                voice_topics::status.eq("closed"),
                voice_topics::is_current.eq(false),
                voice_topics::updated_at.eq(&at),
            ))
            .execute(conn)?;
            if n == 0 {
                return Ok(None);
            }
            load_one(conn, &session_id, &id).map_err(Into::into)
        })
        .await
    }

    /// Make an open topic the current one, clearing any other in the same
    /// transaction. `Ok(None)` when `id` is not an open topic of
    /// `session_id`.
    pub async fn focus_voice_topic(
        &self,
        session_id: &str,
        id: &str,
    ) -> anyhow::Result<Option<VoiceTopic>> {
        let (session_id, id) = (session_id.to_string(), id.to_string());
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                match load_one(conn, &session_id, &id)? {
                    Some(t) if t.status == "open" => {}
                    _ => return Ok(None),
                }
                diesel::update(
                    voice_topics::table
                        .filter(voice_topics::session_id.eq(&session_id))
                        .filter(voice_topics::is_current.eq(true)),
                )
                .set(voice_topics::is_current.eq(false))
                .execute(conn)?;
                diesel::update(voice_topics::table.find(&id))
                    .set((
                        voice_topics::is_current.eq(true),
                        voice_topics::updated_at.eq(now()),
                    ))
                    .execute(conn)?;
                load_one(conn, &session_id, &id).map_err(Into::into)
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn voice_topics_crud_scopes_validates_and_keeps_one_current() {
        let db = Db::in_memory().unwrap();
        let a = db
            .add_voice_topic(
                "v1",
                "conversation",
                " Dinner plans ",
                None,
                Some("ignored"),
            )
            .await
            .unwrap();
        assert_eq!(a.title, "Dinner plans");
        assert_eq!(a.ref_id, None);
        let b = db
            .add_voice_topic("v1", "session", "Stashify", Some("the build"), Some("s1"))
            .await
            .unwrap();

        // Validation.
        assert!(
            db.add_voice_topic("v1", "card", "x", None, None)
                .await
                .is_err()
        );
        assert!(
            db.add_voice_topic("v1", "project", "x", None, None)
                .await
                .is_err()
        );
        let long = "x".repeat(MAX_VOICE_TOPIC_TITLE_CHARS + 1);
        assert!(
            db.add_voice_topic("v1", "conversation", &long, None, None)
                .await
                .is_err()
        );
        let dup = db
            .add_voice_topic("v1", "session", "Again", None, Some("s1"))
            .await
            .unwrap_err();
        assert!(dup.to_string().contains(&b.id), "{dup}");

        // One current at a time; other sessions' ids are not found.
        db.focus_voice_topic("v1", &a.id).await.unwrap().unwrap();
        let fb = db.focus_voice_topic("v1", &b.id).await.unwrap().unwrap();
        assert!(fb.is_current);
        let open = db.list_open_voice_topics("v1").await.unwrap();
        assert_eq!(open.iter().filter(|t| t.is_current).count(), 1);
        assert_eq!(open[0].id, b.id, "newest change first");
        assert!(db.focus_voice_topic("v2", &a.id).await.unwrap().is_none());
        assert!(db.close_voice_topic("v2", &a.id).await.unwrap().is_none());

        let up = db
            .update_voice_topic("v1", &a.id, Some("Dinner"), Some(""))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((up.title.as_str(), up.summary), ("Dinner", None));

        // Closing drops it from the ring and as current; a closed topic
        // can't be focused.
        let closed = db.close_voice_topic("v1", &b.id).await.unwrap().unwrap();
        assert!(!closed.is_current && closed.status == "closed");
        assert!(db.focus_voice_topic("v1", &b.id).await.unwrap().is_none());
        assert_eq!(db.list_open_voice_topics("v1").await.unwrap().len(), 1);

        // The open cap.
        for i in 1..MAX_OPEN_VOICE_TOPICS {
            db.add_voice_topic("v1", "conversation", &format!("t{i}"), None, None)
                .await
                .unwrap();
        }
        let full = db
            .add_voice_topic("v1", "conversation", "one more", None, None)
            .await
            .unwrap_err();
        assert!(full.to_string().contains("Close"), "{full}");
    }
}
