//! Named voice assistants (`assistants`): who the user can talk to.
//!
//! Each row owns one voice session (`session_id`, set when the assistant is
//! first opened). Exactly one row is the default — who answers when no name
//! is said. The table starts empty: [`Db::default_assistant`] creates the
//! default row on first read and hands it the voice session an install
//! already had, so the single Assistant from before this table keeps its
//! conversation, topics and memory.

use diesel::connection::Connection;
use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::Assistant;
use crate::db::schema::{assistants, sessions};
use crate::service::voice_relay::VOICE_EXPERT_KIND;

/// Name of the default assistant created on first read.
pub const DEFAULT_ASSISTANT_NAME: &str = "Assistant";
/// Longest name, in chars. It is spoken and shown on a small chip.
pub const MAX_ASSISTANT_NAME_CHARS: usize = 24;
/// Longest job text, in chars. It is added to every prompt of the assistant.
pub const MAX_ASSISTANT_JOB_CHARS: usize = 4000;
/// Most assistants one server may have. Each is a live session.
pub const MAX_ASSISTANTS: usize = 8;

/// What a new assistant is created from.
#[derive(Debug, Default, Clone)]
pub struct NewAssistant {
    pub name: String,
    pub job: String,
    pub voice: Option<String>,
    pub color: Option<String>,
    pub icon: Option<String>,
    pub template: Option<String>,
}

/// A change to an assistant: `None` leaves a field alone; for the nullable
/// fields `Some(None)` clears it.
#[derive(Debug, Default, Clone)]
pub struct AssistantPatch {
    pub name: Option<String>,
    pub job: Option<String>,
    pub voice: Option<Option<String>>,
    pub color: Option<Option<String>>,
    pub icon: Option<Option<String>>,
    pub position: Option<i32>,
}

/// A name must be sayable and recognisable in a transcript: it starts with
/// a letter and holds only letters, digits, spaces, `-` and `'`.
fn validate_name(raw: &str) -> anyhow::Result<String> {
    let name = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        anyhow::bail!("name must not be empty");
    }
    let len = name.chars().count();
    if len > MAX_ASSISTANT_NAME_CHARS {
        anyhow::bail!("name is {len} chars; the limit is {MAX_ASSISTANT_NAME_CHARS}");
    }
    if !name.chars().next().is_some_and(char::is_alphabetic) {
        anyhow::bail!("name must start with a letter");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_alphanumeric() || matches!(c, ' ' | '-' | '\'')))
    {
        anyhow::bail!("name can't contain '{bad}': use letters, digits, spaces, - and '");
    }
    Ok(name)
}

fn validate_job(raw: &str) -> anyhow::Result<String> {
    let job = raw.trim();
    let len = job.chars().count();
    if len > MAX_ASSISTANT_JOB_CHARS {
        anyhow::bail!("job is {len} chars; the limit is {MAX_ASSISTANT_JOB_CHARS}");
    }
    Ok(job.to_string())
}

/// Trimmed, with blank meaning "not set".
fn clean(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn load_all(conn: &mut SqliteConnection) -> QueryResult<Vec<Assistant>> {
    assistants::table
        .order((
            assistants::position.asc(),
            assistants::created_at.asc(),
            assistants::id.asc(),
        ))
        .select(Assistant::as_select())
        .load(conn)
}

fn load_one(conn: &mut SqliteConnection, id: &str) -> QueryResult<Option<Assistant>> {
    assistants::table
        .find(id)
        .select(Assistant::as_select())
        .first(conn)
        .optional()
}

/// Make sure a default assistant exists. On an empty table this creates it
/// and gives it the oldest voice session, if the install has one.
///
/// Adoption happens only here, on the empty table (the upgrade from the
/// single-Assistant versions). An existing default never adopts a session
/// later: a voice session that is momentarily ownerless is one being
/// created for, or deleted with, another assistant.
fn ensure_default(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let all = load_all(conn)?;
    if all.iter().any(|a| a.is_default) {
        return Ok(());
    }
    if let Some(first) = all.first() {
        diesel::update(assistants::table.find(&first.id))
            .set(assistants::is_default.eq(true))
            .execute(conn)?;
        return Ok(());
    }
    let session_id: Option<String> = sessions::table
        .filter(sessions::expert_kind.eq(VOICE_EXPERT_KIND))
        .order((sessions::created_at.asc(), sessions::id.asc()))
        .select(sessions::id)
        .first(conn)
        .optional()?;
    let at = now();
    diesel::insert_into(assistants::table)
        .values(Assistant {
            id: uuid::Uuid::new_v4().to_string(),
            name: DEFAULT_ASSISTANT_NAME.to_string(),
            job: String::new(),
            voice: None,
            color: None,
            icon: None,
            template: None,
            session_id,
            is_default: true,
            position: 0,
            created_at: at.clone(),
            updated_at: at,
        })
        .execute(conn)?;
    Ok(())
}

fn name_taken(all: &[Assistant], name: &str, except_id: Option<&str>) -> bool {
    let wanted = name.to_lowercase();
    all.iter()
        .any(|a| Some(a.id.as_str()) != except_id && a.name.to_lowercase() == wanted)
}

impl Db {
    /// Every assistant in display order, the default one included (created
    /// on first read).
    pub async fn list_assistants(&self) -> anyhow::Result<Vec<Assistant>> {
        self.with_conn(|conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                ensure_default(conn)?;
                Ok(load_all(conn)?)
            })
        })
        .await
    }

    /// The assistant that answers when no name is said. Always exists.
    pub async fn default_assistant(&self) -> anyhow::Result<Assistant> {
        self.list_assistants()
            .await?
            .into_iter()
            .find(|a| a.is_default)
            .ok_or_else(|| anyhow::anyhow!("no default assistant"))
    }

    pub async fn get_assistant(&self, id: &str) -> anyhow::Result<Option<Assistant>> {
        let id = id.to_string();
        self.with_conn(move |conn| load_one(conn, &id).map_err(Into::into))
            .await
    }

    /// The assistant whose conversation lives in `session_id`, if any.
    pub async fn assistant_for_session(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<Assistant>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                // The Assistant an install had before this table is adopted
                // on first read, whichever read that is.
                ensure_default(conn)?;
                Ok(assistants::table
                    .filter(assistants::session_id.eq(&session_id))
                    .select(Assistant::as_select())
                    .first(conn)
                    .optional()?)
            })
        })
        .await
    }

    /// Add an assistant after the existing ones. Refuses a name already in
    /// use (ignoring case) and more than [`MAX_ASSISTANTS`].
    pub async fn create_assistant(&self, new: NewAssistant) -> anyhow::Result<Assistant> {
        let name = validate_name(&new.name)?;
        let job = validate_job(&new.job)?;
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                ensure_default(conn)?;
                let all = load_all(conn)?;
                if all.len() >= MAX_ASSISTANTS {
                    anyhow::bail!(
                        "there are already {MAX_ASSISTANTS} assistants; delete one first"
                    );
                }
                if name_taken(&all, &name, None) {
                    anyhow::bail!("an assistant named \"{name}\" already exists");
                }
                let at = now();
                let row = Assistant {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    job,
                    voice: clean(new.voice),
                    color: clean(new.color),
                    icon: clean(new.icon),
                    template: clean(new.template),
                    session_id: None,
                    is_default: false,
                    position: all.iter().map(|a| a.position).max().unwrap_or(0) + 1,
                    created_at: at.clone(),
                    updated_at: at,
                };
                diesel::insert_into(assistants::table)
                    .values(&row)
                    .execute(conn)?;
                Ok(row)
            })
        })
        .await
    }

    /// Apply `patch`; `None` when there is no such assistant.
    pub async fn update_assistant(
        &self,
        id: &str,
        patch: AssistantPatch,
    ) -> anyhow::Result<Option<Assistant>> {
        let name = patch.name.as_deref().map(validate_name).transpose()?;
        let job = patch.job.as_deref().map(validate_job).transpose()?;
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let Some(mut row) = load_one(conn, &id)? else {
                    return Ok(None);
                };
                if let Some(name) = name {
                    if name_taken(&load_all(conn)?, &name, Some(&id)) {
                        anyhow::bail!("an assistant named \"{name}\" already exists");
                    }
                    row.name = name;
                }
                if let Some(job) = job {
                    row.job = job;
                }
                if let Some(voice) = patch.voice {
                    row.voice = clean(voice);
                }
                if let Some(color) = patch.color {
                    row.color = clean(color);
                }
                if let Some(icon) = patch.icon {
                    row.icon = clean(icon);
                }
                if let Some(position) = patch.position {
                    row.position = position;
                }
                row.updated_at = now();
                diesel::update(assistants::table.find(&id))
                    .set((
                        assistants::name.eq(&row.name),
                        assistants::job.eq(&row.job),
                        assistants::voice.eq(&row.voice),
                        assistants::color.eq(&row.color),
                        assistants::icon.eq(&row.icon),
                        assistants::position.eq(row.position),
                        assistants::updated_at.eq(&row.updated_at),
                    ))
                    .execute(conn)?;
                Ok(Some(row))
            })
        })
        .await
    }

    /// Make `id` the default assistant; `None` when there is no such one.
    pub async fn set_default_assistant(&self, id: &str) -> anyhow::Result<Option<Assistant>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                if load_one(conn, &id)?.is_none() {
                    return Ok(None);
                }
                // Clear first: the partial unique index allows one default.
                diesel::update(assistants::table)
                    .set(assistants::is_default.eq(false))
                    .execute(conn)?;
                diesel::update(assistants::table.find(&id))
                    .set((
                        assistants::is_default.eq(true),
                        assistants::updated_at.eq(now()),
                    ))
                    .execute(conn)?;
                Ok(load_one(conn, &id)?)
            })
        })
        .await
    }

    /// Record (or clear) the voice session holding `id`'s conversation.
    /// False when there is no such assistant.
    pub async fn set_assistant_session(
        &self,
        id: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        let (id, session_id) = (id.to_string(), session_id.map(str::to_string));
        self.with_conn(move |conn| {
            let n = diesel::update(assistants::table.find(&id))
                .set((
                    assistants::session_id.eq(&session_id),
                    assistants::updated_at.eq(now()),
                ))
                .execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    /// Delete an assistant and return its row (the caller removes its
    /// session). The default assistant can't be deleted: make another one
    /// the default first. `None` when there is no such assistant.
    pub async fn delete_assistant(&self, id: &str) -> anyhow::Result<Option<Assistant>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let Some(row) = load_one(conn, &id)? else {
                    return Ok(None);
                };
                if row.is_default {
                    anyhow::bail!(
                        "\"{}\" is the default assistant; make another one the default first",
                        row.name
                    );
                }
                diesel::delete(assistants::table.find(&id)).execute(conn)?;
                Ok(Some(row))
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    /// Seed a voice session; the first call (the February one) also makes
    /// the folder it lives in.
    async fn voice_session(db: &Db, id: &str, created_at: &str) {
        if created_at.starts_with("2026-02") {
            db.create_folder(NewFolder {
                id: "f".into(),
                name: "f".into(),
                path: ".".into(),
                created_at: created_at.into(),
            })
            .await
            .unwrap();
        }
        db.create_session(NewSession {
            id: id.into(),
            name: "Assistant".into(),
            folder_id: "f".into(),
            created_at: created_at.into(),
            last_activity: created_at.into(),
            is_expert: true,
            expert_kind: Some(VOICE_EXPERT_KIND.into()),
            is_permanent: true,
            ..Default::default()
        })
        .await
        .unwrap();
    }

    fn named(name: &str) -> NewAssistant {
        NewAssistant {
            name: name.into(),
            ..Default::default()
        }
    }

    /// The Assistant an install already has becomes the default row and
    /// keeps its session; on a new install the default has no session yet.
    #[tokio::test]
    async fn default_assistant_adopts_the_existing_voice_session() {
        let fresh = Db::in_memory().unwrap();
        let d = fresh.default_assistant().await.unwrap();
        assert_eq!(d.name, DEFAULT_ASSISTANT_NAME);
        assert_eq!(d.session_id, None);
        assert_eq!(fresh.default_assistant().await.unwrap().id, d.id);

        let db = Db::in_memory().unwrap();
        voice_session(&db, "newer", "2026-02-01T00:00:00Z").await;
        voice_session(&db, "older", "2026-01-01T00:00:00Z").await;
        let d = db.default_assistant().await.unwrap();
        assert_eq!(d.session_id.as_deref(), Some("older"));
        assert_eq!(
            db.assistant_for_session("older").await.unwrap().unwrap().id,
            d.id
        );
        assert_eq!(db.list_assistants().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn assistants_crud_validates_names_and_keeps_one_default() {
        let db = Db::in_memory().unwrap();
        let first = db.default_assistant().await.unwrap();
        let nova = db
            .create_assistant(NewAssistant {
                name: "  Nova   Prime ".into(),
                job: " Reviews pull requests. ".into(),
                voice: Some("kokoro:af_heart".into()),
                color: Some(" ".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(nova.name, "Nova Prime");
        assert_eq!(nova.job, "Reviews pull requests.");
        assert_eq!(nova.color, None);
        assert!(!nova.is_default && nova.position > first.position);

        // Names: unique ignoring case, speakable.
        assert!(db.create_assistant(named("nova prime")).await.is_err());
        assert!(db.create_assistant(named("assistant")).await.is_err());
        assert!(db.create_assistant(named("")).await.is_err());
        assert!(db.create_assistant(named("7 of 9")).await.is_err());
        assert!(db.create_assistant(named("Max!")).await.is_err());
        assert!(
            db.create_assistant(named(&"x".repeat(MAX_ASSISTANT_NAME_CHARS + 1)))
                .await
                .is_err()
        );

        // Update: rename to itself in another case is fine, to a taken name is not.
        let patch = |name: &str| AssistantPatch {
            name: Some(name.into()),
            ..Default::default()
        };
        assert!(
            db.update_assistant(&nova.id, patch("ASSISTANT"))
                .await
                .is_err()
        );
        let renamed = db
            .update_assistant(
                &nova.id,
                AssistantPatch {
                    name: Some("NOVA prime".into()),
                    voice: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.name, "NOVA prime");
        assert_eq!(renamed.voice, None);
        assert_eq!(renamed.job, "Reviews pull requests.");
        assert!(
            db.update_assistant("nope", patch("X"))
                .await
                .unwrap()
                .is_none()
        );

        // Session pointer.
        assert!(
            db.set_assistant_session(&nova.id, Some("s9"))
                .await
                .unwrap()
        );
        assert_eq!(
            db.assistant_for_session("s9").await.unwrap().unwrap().id,
            nova.id
        );

        // Default: exactly one, and it can't be deleted.
        assert!(db.delete_assistant(&first.id).await.is_err());
        db.set_default_assistant(&nova.id).await.unwrap().unwrap();
        let all = db.list_assistants().await.unwrap();
        assert_eq!(all.iter().filter(|a| a.is_default).count(), 1);
        assert_eq!(db.default_assistant().await.unwrap().id, nova.id);
        assert!(db.delete_assistant(&nova.id).await.is_err());
        assert_eq!(
            db.delete_assistant(&first.id).await.unwrap().unwrap().id,
            first.id
        );
        assert!(db.delete_assistant(&first.id).await.unwrap().is_none());

        // Cap.
        for i in 0..MAX_ASSISTANTS - 1 {
            db.create_assistant(named(&format!("Bot {}", (b'a' + i as u8) as char)))
                .await
                .unwrap();
        }
        assert!(db.create_assistant(named("One Too Many")).await.is_err());
    }
}
