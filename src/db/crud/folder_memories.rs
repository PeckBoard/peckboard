//! Long-term memory of a folder (`folder_memories`).
//!
//! What every session working in a folder should know and nobody should
//! have to rediscover: architecture, design decisions, code structure,
//! project layout, working conventions. Shared by all sessions of the
//! folder, never expires. Every accessor is scoped by `folder_id` — an id
//! from another folder is simply "not found".

use diesel::connection::Connection;
use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::{FolderMemory, NewFolderMemory};
use crate::db::schema::folder_memories;

/// The categories a long-term note can have, in the order they are listed.
/// Mirrors the CHECK constraint on `folder_memories.category`.
pub const FOLDER_MEMORY_CATEGORIES: &[&str] = &[
    "architecture",
    "decision",
    "structure",
    "layout",
    "convention",
];

/// Longest single long-term note, in chars.
pub const MAX_FOLDER_MEMORY_CHARS: usize = 4_000;
/// Longest title, in chars.
pub const MAX_FOLDER_MEMORY_TITLE_CHARS: usize = 120;
/// Most long-term notes one folder may hold.
pub const MAX_FOLDER_MEMORY_ENTRIES: usize = 300;

fn validate_category(raw: &str) -> anyhow::Result<String> {
    let category = raw.trim().to_ascii_lowercase();
    if !FOLDER_MEMORY_CATEGORIES.contains(&category.as_str()) {
        anyhow::bail!(
            "category must be one of: {}",
            FOLDER_MEMORY_CATEGORIES.join(", ")
        );
    }
    Ok(category)
}

fn validate_title(raw: &str) -> anyhow::Result<String> {
    // One line: the title is what the prompt shows when notes are listed
    // by title only.
    let title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        anyhow::bail!("a long-term note needs a title");
    }
    let len = title.chars().count();
    if len > MAX_FOLDER_MEMORY_TITLE_CHARS {
        anyhow::bail!("title is {len} chars; the limit is {MAX_FOLDER_MEMORY_TITLE_CHARS}");
    }
    Ok(title)
}

fn validate_content(raw: &str) -> anyhow::Result<String> {
    let content = raw.trim();
    if content.is_empty() {
        anyhow::bail!("memory content must not be empty");
    }
    let len = content.chars().count();
    if len > MAX_FOLDER_MEMORY_CHARS {
        anyhow::bail!(
            "long-term note is {len} chars; the limit is {MAX_FOLDER_MEMORY_CHARS}. \
             Split it into separate notes."
        );
    }
    Ok(content.to_string())
}

fn category_rank(category: &str) -> usize {
    FOLDER_MEMORY_CATEGORIES
        .iter()
        .position(|c| *c == category)
        .unwrap_or(FOLDER_MEMORY_CATEGORIES.len())
}

fn load_folder(conn: &mut SqliteConnection, folder_id: &str) -> QueryResult<Vec<FolderMemory>> {
    let mut rows: Vec<FolderMemory> = folder_memories::table
        .filter(folder_memories::folder_id.eq(folder_id))
        .order((folder_memories::created_at.asc(), folder_memories::id.asc()))
        .select(FolderMemory::as_select())
        .load(conn)?;
    // Category order is the listing order, not alphabetical. Stable sort
    // keeps oldest-first inside a category.
    rows.sort_by_key(|m| category_rank(&m.category));
    Ok(rows)
}

impl Db {
    /// Every long-term note of `folder_id`, grouped by category
    /// ([`FOLDER_MEMORY_CATEGORIES`] order), oldest first inside each.
    pub async fn list_folder_memories(&self, folder_id: &str) -> anyhow::Result<Vec<FolderMemory>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| load_folder(conn, &folder_id).map_err(Into::into))
            .await
    }

    /// One long-term note, or `None` when `id` is not a note of `folder_id`.
    pub async fn get_folder_memory(
        &self,
        folder_id: &str,
        id: &str,
    ) -> anyhow::Result<Option<FolderMemory>> {
        let folder_id = folder_id.to_string();
        let id = id.to_string();
        self.with_conn(move |conn| {
            folder_memories::table
                .filter(folder_memories::id.eq(&id))
                .filter(folder_memories::folder_id.eq(&folder_id))
                .select(FolderMemory::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Add one long-term note to `folder_id`.
    pub async fn add_folder_memory(
        &self,
        folder_id: &str,
        category: &str,
        title: &str,
        content: &str,
        source_session_id: Option<&str>,
    ) -> anyhow::Result<FolderMemory> {
        let folder_id = folder_id.to_string();
        let category = validate_category(category)?;
        let title = validate_title(title)?;
        let content = validate_content(content)?;
        let source_session_id = source_session_id.map(str::to_string);
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let count: i64 = folder_memories::table
                    .filter(folder_memories::folder_id.eq(&folder_id))
                    .count()
                    .get_result(conn)?;
                if count as usize >= MAX_FOLDER_MEMORY_ENTRIES {
                    anyhow::bail!(
                        "this folder already holds {count} long-term notes; the limit is \
                         {MAX_FOLDER_MEMORY_ENTRIES}. Merge or remove some first."
                    );
                }
                let now = chrono::Utc::now().to_rfc3339();
                let new = NewFolderMemory {
                    id: uuid::Uuid::new_v4().to_string(),
                    folder_id: folder_id.clone(),
                    category,
                    title,
                    content,
                    source_session_id,
                    created_at: now.clone(),
                    updated_at: now,
                };
                diesel::insert_into(folder_memories::table)
                    .values(&new)
                    .returning(FolderMemory::as_returning())
                    .get_result(conn)
                    .map_err(Into::into)
            })
        })
        .await
    }

    /// Change a long-term note; fields left `None` keep their value.
    /// `Ok(None)` when `id` is not a note of `folder_id`.
    pub async fn update_folder_memory(
        &self,
        folder_id: &str,
        id: &str,
        category: Option<&str>,
        title: Option<&str>,
        content: Option<&str>,
    ) -> anyhow::Result<Option<FolderMemory>> {
        let folder_id = folder_id.to_string();
        let id = id.to_string();
        let category = category.map(validate_category).transpose()?;
        let title = title.map(validate_title).transpose()?;
        let content = content.map(validate_content).transpose()?;
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let existing: Option<FolderMemory> = folder_memories::table
                    .filter(folder_memories::id.eq(&id))
                    .filter(folder_memories::folder_id.eq(&folder_id))
                    .select(FolderMemory::as_select())
                    .first(conn)
                    .optional()?;
                let Some(existing) = existing else {
                    return Ok(None);
                };
                diesel::update(folder_memories::table.filter(folder_memories::id.eq(&id)))
                    .set((
                        folder_memories::category.eq(category.unwrap_or(existing.category)),
                        folder_memories::title.eq(title.unwrap_or(existing.title)),
                        folder_memories::content.eq(content.unwrap_or(existing.content)),
                        folder_memories::updated_at.eq(chrono::Utc::now().to_rfc3339()),
                    ))
                    .execute(conn)?;
                folder_memories::table
                    .find(&id)
                    .select(FolderMemory::as_select())
                    .first(conn)
                    .optional()
                    .map_err(Into::into)
            })
        })
        .await
    }

    /// Delete the given long-term notes of `folder_id`. Returns how many
    /// rows went; unknown ids and ids of another folder are skipped.
    pub async fn remove_folder_memories(
        &self,
        folder_id: &str,
        ids: &[String],
    ) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let folder_id = folder_id.to_string();
        let ids = ids.to_vec();
        self.with_conn(move |conn| {
            diesel::delete(
                folder_memories::table
                    .filter(folder_memories::folder_id.eq(&folder_id))
                    .filter(folder_memories::id.eq_any(&ids)),
            )
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewFolder;

    async fn db_with_folders(ids: &[&str]) -> Db {
        let db = Db::in_memory().unwrap();
        for id in ids {
            db.create_folder(NewFolder {
                id: id.to_string(),
                name: id.to_string(),
                path: format!("/tmp/{id}"),
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
        }
        db
    }

    #[tokio::test]
    async fn notes_are_grouped_by_category_and_scoped_to_their_folder() {
        let db = db_with_folders(&["f1", "f2"]).await;
        let conv = db
            .add_folder_memory("f1", "convention", "Tabs", "indent with tabs", None)
            .await
            .unwrap();
        let arch = db
            .add_folder_memory(
                "f1",
                "Architecture",
                "  One   binary ",
                "axum + sqlite",
                None,
            )
            .await
            .unwrap();
        assert_eq!(arch.category, "architecture");
        assert_eq!(arch.title, "One binary");

        let listed = db.list_folder_memories("f1").await.unwrap();
        assert_eq!(
            listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [arch.id.as_str(), conv.id.as_str()],
            "architecture lists before convention"
        );

        // Another folder sees nothing and cannot touch f1's notes.
        assert!(db.list_folder_memories("f2").await.unwrap().is_empty());
        assert!(
            db.update_folder_memory("f2", &arch.id, None, None, Some("stolen"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.remove_folder_memories("f2", std::slice::from_ref(&arch.id))
                .await
                .unwrap(),
            0
        );

        let updated = db
            .update_folder_memory("f1", &arch.id, None, None, Some("axum + diesel"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.content, "axum + diesel");
        assert_eq!(updated.title, "One binary", "unset fields keep their value");

        let err = db
            .add_folder_memory("f1", "gossip", "x", "y", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("category must be one of"), "got: {err}");
    }
}
