//! Skills storage (`service::skills`): sources, installed skills with their
//! immutable version history, subscriptions, groups, and per-session
//! assignments. Version ordering (semver) and every policy decision live in
//! the service; this layer is plain rows.

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    // ── Sources ─────────────────────────────────────────────────────────

    /// Every configured skill source, oldest first (the seeded public
    /// source leads).
    pub async fn list_skill_sources(&self) -> anyhow::Result<Vec<SkillSourceRow>> {
        self.with_conn(move |conn| {
            skill_sources::table
                .order((skill_sources::created_at.asc(), skill_sources::id.asc()))
                .select(SkillSourceRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_skill_source(&self, id: &str) -> anyhow::Result<Option<SkillSourceRow>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            skill_sources::table
                .find(&id)
                .select(SkillSourceRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_skill_source_by_name(
        &self,
        name: &str,
    ) -> anyhow::Result<Option<SkillSourceRow>> {
        let name = name.to_string();
        self.with_conn(move |conn| {
            skill_sources::table
                .filter(skill_sources::name.eq(&name))
                .select(SkillSourceRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn insert_skill_source(&self, row: SkillSourceRow) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(skill_sources::table)
                .values(&row)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Delete a source; its skills, groups (and through them their versions,
    /// subscriptions and session assignments) cascade.
    pub async fn delete_skill_source(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let n = diesel::delete(skill_sources::table.find(&id)).execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    /// Record the outcome of a sync: `last_sync_at` is only advanced on
    /// success; `last_error` is replaced either way (None clears it).
    pub async fn set_skill_source_sync(
        &self,
        id: &str,
        synced_at: Option<String>,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            if let Some(at) = synced_at {
                diesel::update(skill_sources::table.find(&id))
                    .set((
                        skill_sources::last_sync_at.eq(Some(at)),
                        skill_sources::last_error.eq(error),
                    ))
                    .execute(conn)?;
            } else {
                diesel::update(skill_sources::table.find(&id))
                    .set(skill_sources::last_error.eq(error))
                    .execute(conn)?;
            }
            Ok(())
        })
        .await
    }

    // ── Skills + versions ───────────────────────────────────────────────

    pub async fn list_skills(&self) -> anyhow::Result<Vec<SkillRow>> {
        self.with_conn(move |conn| {
            skills::table
                .order((skills::name.asc(), skills::id.asc()))
                .select(SkillRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_skill(&self, id: &str) -> anyhow::Result<Option<SkillRow>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            skills::table
                .find(&id)
                .select(SkillRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// The installed skill named `name` from `source_id` (None = local).
    pub async fn find_skill(
        &self,
        source_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<Option<SkillRow>> {
        let source_id = source_id.map(str::to_string);
        let name = name.to_string();
        self.with_conn(move |conn| {
            let mut q = skills::table
                .filter(skills::name.eq(&name))
                .select(SkillRow::as_select())
                .into_boxed();
            q = match &source_id {
                Some(s) => q.filter(skills::source_id.eq(s.clone())),
                None => q.filter(skills::source_id.is_null()),
            };
            q.first(conn).optional().map_err(Into::into)
        })
        .await
    }

    /// Insert a skill together with its first stored version, atomically.
    pub async fn insert_skill_with_version(
        &self,
        skill: SkillRow,
        version: SkillVersionRow,
    ) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::insert_into(skills::table)
                    .values(&skill)
                    .execute(conn)?;
                diesel::insert_into(skill_versions::table)
                    .values(&version)
                    .execute(conn)?;
                Ok(())
            })
        })
        .await
    }

    /// Store a new immutable version and, when `make_current`, move the
    /// skill's head (version + metadata) to it. Fails on a duplicate
    /// (skill_id, version) — versions are never overwritten.
    pub async fn add_skill_version(
        &self,
        version: SkillVersionRow,
        make_current: bool,
        description: String,
        tags_json: String,
    ) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::insert_into(skill_versions::table)
                    .values(&version)
                    .execute(conn)?;
                if make_current {
                    diesel::update(skills::table.find(&version.skill_id))
                        .set((
                            skills::version.eq(&version.version),
                            skills::description.eq(&description),
                            skills::tags.eq(&tags_json),
                            skills::updated_at.eq(&version.created_at),
                        ))
                        .execute(conn)?;
                }
                Ok(())
            })
        })
        .await
    }

    /// Update the head metadata without a new version (remote sync of a
    /// skill whose latest version is already stored).
    pub async fn update_skill_head(
        &self,
        id: &str,
        version: &str,
        description: &str,
        tags_json: &str,
    ) -> anyhow::Result<()> {
        let (id, version, description, tags_json) = (
            id.to_string(),
            version.to_string(),
            description.to_string(),
            tags_json.to_string(),
        );
        self.with_conn(move |conn| {
            diesel::update(skills::table.find(&id))
                .set((
                    skills::version.eq(&version),
                    skills::description.eq(&description),
                    skills::tags.eq(&tags_json),
                    skills::updated_at.eq(chrono::Utc::now().to_rfc3339()),
                ))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    pub async fn delete_skill(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let n = diesel::delete(skills::table.find(&id)).execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    /// Every stored version of a skill (unordered — callers sort by semver).
    pub async fn list_skill_versions(
        &self,
        skill_id: &str,
    ) -> anyhow::Result<Vec<SkillVersionRow>> {
        let skill_id = skill_id.to_string();
        self.with_conn(move |conn| {
            skill_versions::table
                .filter(skill_versions::skill_id.eq(&skill_id))
                .select(SkillVersionRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Stored version labels for every skill, `(skill_id, version)`.
    pub async fn list_all_skill_version_labels(&self) -> anyhow::Result<Vec<(String, String)>> {
        self.with_conn(move |conn| {
            skill_versions::table
                .select((skill_versions::skill_id, skill_versions::version))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_skill_version(
        &self,
        skill_id: &str,
        version: &str,
    ) -> anyhow::Result<Option<SkillVersionRow>> {
        let (skill_id, version) = (skill_id.to_string(), version.to_string());
        self.with_conn(move |conn| {
            skill_versions::table
                .filter(skill_versions::skill_id.eq(&skill_id))
                .filter(skill_versions::version.eq(&version))
                .select(SkillVersionRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    // ── Subscriptions ───────────────────────────────────────────────────

    pub async fn list_skill_subscriptions(&self) -> anyhow::Result<Vec<SkillSubscriptionRow>> {
        self.with_conn(move |conn| {
            skill_subscriptions::table
                .select(SkillSubscriptionRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_skill_subscription(
        &self,
        skill_id: &str,
    ) -> anyhow::Result<Option<SkillSubscriptionRow>> {
        let skill_id = skill_id.to_string();
        self.with_conn(move |conn| {
            skill_subscriptions::table
                .find(&skill_id)
                .select(SkillSubscriptionRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Create or re-pin a subscription (`None` = follow latest).
    pub async fn upsert_skill_subscription(
        &self,
        skill_id: &str,
        pinned_version: Option<String>,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let row = SkillSubscriptionRow {
            skill_id: skill_id.to_string(),
            pinned_version,
            created_at: now.clone(),
            updated_at: now,
        };
        self.with_conn(move |conn| {
            diesel::insert_into(skill_subscriptions::table)
                .values(&row)
                .on_conflict(skill_subscriptions::skill_id)
                .do_update()
                .set((
                    skill_subscriptions::pinned_version.eq(&row.pinned_version),
                    skill_subscriptions::updated_at.eq(&row.updated_at),
                ))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    // ── Groups ──────────────────────────────────────────────────────────

    pub async fn list_skill_groups(&self) -> anyhow::Result<Vec<SkillGroupRow>> {
        self.with_conn(move |conn| {
            skill_groups::table
                .order((skill_groups::slug.asc(), skill_groups::id.asc()))
                .select(SkillGroupRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_skill_group(&self, id: &str) -> anyhow::Result<Option<SkillGroupRow>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            skill_groups::table
                .find(&id)
                .select(SkillGroupRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// The group with `slug` from `source_id` (None = local).
    pub async fn find_skill_group(
        &self,
        source_id: Option<&str>,
        slug: &str,
    ) -> anyhow::Result<Option<SkillGroupRow>> {
        let source_id = source_id.map(str::to_string);
        let slug = slug.to_string();
        self.with_conn(move |conn| {
            let mut q = skill_groups::table
                .filter(skill_groups::slug.eq(&slug))
                .select(SkillGroupRow::as_select())
                .into_boxed();
            q = match &source_id {
                Some(s) => q.filter(skill_groups::source_id.eq(s.clone())),
                None => q.filter(skill_groups::source_id.is_null()),
            };
            q.first(conn).optional().map_err(Into::into)
        })
        .await
    }

    /// Every group membership row.
    pub async fn list_skill_group_members(&self) -> anyhow::Result<Vec<SkillGroupMemberRow>> {
        self.with_conn(move |conn| {
            skill_group_members::table
                .order((
                    skill_group_members::group_id.asc(),
                    skill_group_members::skill_name.asc(),
                ))
                .select(SkillGroupMemberRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Insert a group, or (same id) replace its metadata, and set its
    /// members to exactly `members`, atomically.
    pub async fn save_skill_group(
        &self,
        group: SkillGroupRow,
        members: Vec<String>,
    ) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::insert_into(skill_groups::table)
                    .values(&group)
                    .on_conflict(skill_groups::id)
                    .do_update()
                    .set((
                        skill_groups::slug.eq(&group.slug),
                        skill_groups::name.eq(&group.name),
                        skill_groups::description.eq(&group.description),
                        skill_groups::updated_at.eq(&group.updated_at),
                    ))
                    .execute(conn)?;
                diesel::delete(
                    skill_group_members::table.filter(skill_group_members::group_id.eq(&group.id)),
                )
                .execute(conn)?;
                let mut seen = std::collections::BTreeSet::new();
                for m in members {
                    if seen.insert(m.clone()) {
                        diesel::insert_into(skill_group_members::table)
                            .values(&SkillGroupMemberRow {
                                group_id: group.id.clone(),
                                skill_name: m,
                            })
                            .execute(conn)?;
                    }
                }
                Ok(())
            })
        })
        .await
    }

    /// Add one skill name to a group (idempotent).
    pub async fn add_skill_group_member(&self, group_id: &str, name: &str) -> anyhow::Result<()> {
        let row = SkillGroupMemberRow {
            group_id: group_id.to_string(),
            skill_name: name.to_string(),
        };
        self.with_conn(move |conn| {
            diesel::insert_or_ignore_into(skill_group_members::table)
                .values(&row)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Drop `name` from every LOCAL group (re-tagging a local skill).
    pub async fn remove_skill_from_local_groups(&self, name: &str) -> anyhow::Result<()> {
        let name = name.to_string();
        self.with_conn(move |conn| {
            let local_ids = skill_groups::table
                .filter(skill_groups::source_id.is_null())
                .select(skill_groups::id);
            diesel::delete(
                skill_group_members::table
                    .filter(skill_group_members::skill_name.eq(&name))
                    .filter(skill_group_members::group_id.eq_any(local_ids)),
            )
            .execute(conn)?;
            Ok(())
        })
        .await
    }

    pub async fn delete_skill_group(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let n = diesel::delete(skill_groups::table.find(&id)).execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    // ── Session assignments ─────────────────────────────────────────────

    pub async fn list_session_skills(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<SessionSkillRow>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            session_skills::table
                .filter(session_skills::session_id.eq(&session_id))
                .order((session_skills::created_at.asc(), session_skills::id.asc()))
                .select(SessionSkillRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Replace a session's assignments with exactly `skill_ids` + `group_ids`
    /// (duplicates collapse), atomically. Unknown ids fail the FK check and
    /// roll the whole replacement back.
    pub async fn set_session_skills(
        &self,
        session_id: &str,
        skill_ids: Vec<String>,
        group_ids: Vec<String>,
    ) -> anyhow::Result<()> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::delete(
                    session_skills::table.filter(session_skills::session_id.eq(&session_id)),
                )
                .execute(conn)?;
                let now = chrono::Utc::now().to_rfc3339();
                let mut seen = std::collections::BTreeSet::new();
                let rows = skill_ids
                    .into_iter()
                    .map(|s| (Some(s), None))
                    .chain(group_ids.into_iter().map(|g| (None, Some(g))));
                for (skill_id, group_id) in rows {
                    if !seen.insert((skill_id.clone(), group_id.clone())) {
                        continue;
                    }
                    diesel::insert_into(session_skills::table)
                        .values(&SessionSkillRow {
                            id: uuid::Uuid::new_v4().to_string(),
                            session_id: session_id.clone(),
                            skill_id,
                            group_id,
                            created_at: now.clone(),
                        })
                        .execute(conn)?;
                }
                Ok(())
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(id: &str, name: &str, source: Option<&str>) -> SkillRow {
        let now = chrono::Utc::now().to_rfc3339();
        SkillRow {
            id: id.into(),
            scope: if source.is_some() { "team" } else { "local" }.into(),
            source_id: source.map(str::to_string),
            name: name.into(),
            description: "d".into(),
            tags: "[]".into(),
            version: "1.0.0".into(),
            created_by: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    fn version(skill_id: &str, v: &str) -> SkillVersionRow {
        SkillVersionRow {
            id: uuid::Uuid::new_v4().to_string(),
            skill_id: skill_id.into(),
            version: v.into(),
            body: format!("body {v}"),
            sha256: "x".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[tokio::test]
    async fn skills_versions_are_immutable_and_names_unique_per_source() {
        let db = Db::in_memory().unwrap();
        // The migration seeds the public source.
        let sources = db.list_skill_sources().await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, "public");

        db.insert_skill_with_version(skill("s1", "alpha", None), version("s1", "1.0.0"))
            .await
            .unwrap();
        // A second local skill with the same name is refused.
        assert!(
            db.insert_skill_with_version(skill("s2", "alpha", None), version("s2", "1.0.0"))
                .await
                .is_err()
        );
        // …but the same name from another source is fine.
        db.insert_skill_with_version(skill("s3", "alpha", Some("public")), version("s3", "1.0.0"))
            .await
            .unwrap();

        db.add_skill_version(version("s1", "1.1.0"), true, "new".into(), "[]".into())
            .await
            .unwrap();
        // Re-storing an existing version is refused, the head is untouched.
        assert!(
            db.add_skill_version(version("s1", "1.1.0"), true, "x".into(), "[]".into())
                .await
                .is_err()
        );
        let head = db.get_skill("s1").await.unwrap().unwrap();
        assert_eq!(head.version, "1.1.0");
        assert_eq!(head.description, "new");
        assert_eq!(db.list_skill_versions("s1").await.unwrap().len(), 2);

        // Deleting the source cascades its skills.
        assert!(db.delete_skill_source("public").await.unwrap());
        assert!(db.get_skill("s3").await.unwrap().is_none());
        assert!(db.get_skill("s1").await.unwrap().is_some());
    }
}
