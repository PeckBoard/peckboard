//! Saved multi-session split views (`/api/me/views`). A view is a named,
//! per-user layout tree stored as `session_view_nodes` rows; the wire
//! shape is the nested [`ViewLayout`].

use std::collections::HashMap;

use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// Deepest allowed layout nesting (a lone leaf is depth 1).
pub const MAX_VIEW_DEPTH: usize = 8;
/// Most leaves (session panes) one view may hold.
pub const MAX_VIEW_LEAVES: usize = 16;
/// Longest allowed view name, in characters.
pub const MAX_VIEW_NAME_CHARS: usize = 100;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    Row,
    Col,
}

impl SplitDir {
    fn as_str(self) -> &'static str {
        match self {
            SplitDir::Row => "row",
            SplitDir::Col => "col",
        }
    }
}

/// Wire layout of a saved view: `{"kind":"split","dir","children","ratios"}`
/// or `{"kind":"leaf","sessionId"}`. `ratios[i]` is `children[i]`'s share.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ViewLayout {
    Split {
        dir: SplitDir,
        children: Vec<ViewLayout>,
        ratios: Vec<f64>,
    },
    Leaf {
        #[serde(rename = "sessionId", default)]
        session_id: Option<String>,
    },
}

impl ViewLayout {
    /// Structural limits: splits have ≥ 2 children and one positive,
    /// finite ratio per child; depth ≤ [`MAX_VIEW_DEPTH`]; leaves ≤
    /// [`MAX_VIEW_LEAVES`].
    pub fn validate(&self) -> Result<(), String> {
        let mut leaves = 0usize;
        self.validate_at(1, &mut leaves)?;
        if leaves > MAX_VIEW_LEAVES {
            return Err(format!("layout has more than {MAX_VIEW_LEAVES} panes"));
        }
        Ok(())
    }

    fn validate_at(&self, depth: usize, leaves: &mut usize) -> Result<(), String> {
        if depth > MAX_VIEW_DEPTH {
            return Err(format!("layout is nested deeper than {MAX_VIEW_DEPTH}"));
        }
        match self {
            ViewLayout::Leaf { .. } => {
                *leaves += 1;
                Ok(())
            }
            ViewLayout::Split {
                children, ratios, ..
            } => {
                if children.len() < 2 {
                    return Err("a split needs at least 2 children".into());
                }
                if ratios.len() != children.len() {
                    return Err("a split needs exactly one ratio per child".into());
                }
                if ratios.iter().any(|r| !r.is_finite() || *r <= 0.0) {
                    return Err("ratios must be positive numbers".into());
                }
                children
                    .iter()
                    .try_for_each(|c| c.validate_at(depth + 1, leaves))
            }
        }
    }
}

/// Trim and bound a view name; `Err` carries the user-facing reason.
pub fn validate_view_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.chars().count() > MAX_VIEW_NAME_CHARS {
        return Err(format!(
            "name must be at most {MAX_VIEW_NAME_CHARS} characters"
        ));
    }
    Ok(name.to_string())
}

/// Insert `layout` as the node rooted under `parent`. A leaf pointing at a
/// session that no longer exists is stored blank — the same state
/// `ON DELETE SET NULL` leaves behind when the session goes later.
fn insert_layout(
    conn: &mut SqliteConnection,
    view_id: &str,
    parent: Option<&str>,
    position: i32,
    ratio: f64,
    layout: &ViewLayout,
) -> anyhow::Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    let (kind, dir, session_id) = match layout {
        ViewLayout::Split { dir, .. } => ("split", Some(dir.as_str().to_string()), None),
        ViewLayout::Leaf { session_id } => {
            let existing = match session_id {
                Some(sid) => sessions::table
                    .find(sid)
                    .select(sessions::id)
                    .first::<String>(conn)
                    .optional()?,
                None => None,
            };
            ("leaf", None, existing)
        }
    };
    diesel::insert_into(session_view_nodes::table)
        .values(&SessionViewNode {
            id: id.clone(),
            view_id: view_id.to_string(),
            parent_node_id: parent.map(str::to_string),
            position,
            kind: kind.to_string(),
            dir,
            ratio,
            session_id,
        })
        .execute(conn)?;
    if let ViewLayout::Split {
        children, ratios, ..
    } = layout
    {
        for (i, (child, r)) in children.iter().zip(ratios).enumerate() {
            insert_layout(conn, view_id, Some(&id), i as i32, *r, child)?;
        }
    }
    Ok(())
}

/// Rebuild the nested layout from a view's node rows. A view with no root
/// row (never expected) reads back as a single empty pane.
fn load_layout(conn: &mut SqliteConnection, view_id: &str) -> anyhow::Result<ViewLayout> {
    let nodes: Vec<SessionViewNode> = session_view_nodes::table
        .filter(session_view_nodes::view_id.eq(view_id))
        .select(SessionViewNode::as_select())
        .order(session_view_nodes::position.asc())
        .load(conn)?;
    let mut by_parent: HashMap<Option<String>, Vec<&SessionViewNode>> = HashMap::new();
    for n in &nodes {
        by_parent
            .entry(n.parent_node_id.clone())
            .or_default()
            .push(n);
    }
    fn build(
        node: &SessionViewNode,
        by_parent: &HashMap<Option<String>, Vec<&SessionViewNode>>,
    ) -> ViewLayout {
        if node.kind != "split" {
            return ViewLayout::Leaf {
                session_id: node.session_id.clone(),
            };
        }
        let kids = by_parent
            .get(&Some(node.id.clone()))
            .map(Vec::as_slice)
            .unwrap_or_default();
        ViewLayout::Split {
            dir: if node.dir.as_deref() == Some("col") {
                SplitDir::Col
            } else {
                SplitDir::Row
            },
            children: kids.iter().map(|k| build(k, by_parent)).collect(),
            ratios: kids.iter().map(|k| k.ratio).collect(),
        }
    }
    Ok(match by_parent.get(&None).and_then(|roots| roots.first()) {
        Some(root) => build(root, &by_parent),
        None => ViewLayout::Leaf { session_id: None },
    })
}

fn find_owned_view(
    conn: &mut SqliteConnection,
    user_id: &str,
    id: &str,
) -> anyhow::Result<Option<SessionView>> {
    session_views::table
        .find(id)
        .filter(session_views::user_id.eq(user_id))
        .select(SessionView::as_select())
        .first(conn)
        .optional()
        .map_err(Into::into)
}

impl Db {
    /// A user's saved views, oldest first (no layouts).
    pub async fn list_session_views(&self, user_id: &str) -> anyhow::Result<Vec<SessionView>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            session_views::table
                .filter(session_views::user_id.eq(&user_id))
                .select(SessionView::as_select())
                .order((session_views::created_at.asc(), session_views::id.asc()))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// One of `user_id`'s views with its layout; `None` when it doesn't
    /// exist or belongs to someone else.
    pub async fn get_session_view(
        &self,
        user_id: &str,
        id: &str,
    ) -> anyhow::Result<Option<(SessionView, ViewLayout)>> {
        let (user_id, id) = (user_id.to_string(), id.to_string());
        self.with_conn(move |conn| {
            let Some(view) = find_owned_view(conn, &user_id, &id)? else {
                return Ok(None);
            };
            let layout = load_layout(conn, &view.id)?;
            Ok(Some((view, layout)))
        })
        .await
    }

    /// Create a view. Callers validate `name` / `layout` first.
    pub async fn create_session_view(
        &self,
        user_id: &str,
        name: &str,
        layout: ViewLayout,
    ) -> anyhow::Result<(SessionView, ViewLayout)> {
        let now = chrono::Utc::now().to_rfc3339();
        let view = SessionView {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name: name.to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::insert_into(session_views::table)
                    .values(&view)
                    .execute(conn)?;
                insert_layout(conn, &view.id, None, 0, 1.0, &layout)?;
                let layout = load_layout(conn, &view.id)?;
                Ok((view, layout))
            })
        })
        .await
    }

    /// Rename and/or replace the layout of one of `user_id`'s views. A
    /// layout replace deletes every node and reinserts the tree in the
    /// same transaction. `None` when the view isn't the user's.
    pub async fn update_session_view(
        &self,
        user_id: &str,
        id: &str,
        name: Option<String>,
        layout: Option<ViewLayout>,
    ) -> anyhow::Result<Option<(SessionView, ViewLayout)>> {
        let (user_id, id) = (user_id.to_string(), id.to_string());
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let Some(mut view) = find_owned_view(conn, &user_id, &id)? else {
                    return Ok(None);
                };
                if let Some(name) = name {
                    view.name = name;
                }
                view.updated_at = now;
                diesel::update(session_views::table.find(&view.id))
                    .set((
                        session_views::name.eq(&view.name),
                        session_views::updated_at.eq(&view.updated_at),
                    ))
                    .execute(conn)?;
                if let Some(layout) = layout {
                    diesel::delete(
                        session_view_nodes::table.filter(session_view_nodes::view_id.eq(&view.id)),
                    )
                    .execute(conn)?;
                    insert_layout(conn, &view.id, None, 0, 1.0, &layout)?;
                }
                let layout = load_layout(conn, &view.id)?;
                Ok(Some((view, layout)))
            })
        })
        .await
    }

    /// Delete one of `user_id`'s views (nodes cascade). `false` when it
    /// isn't theirs or doesn't exist.
    pub async fn delete_session_view(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        let (user_id, id) = (user_id.to_string(), id.to_string());
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                if find_owned_view(conn, &user_id, &id)?.is_none() {
                    return Ok(false);
                }
                // Explicit even though the FK cascades, so the delete never
                // depends on the connection's foreign_keys pragma.
                diesel::delete(
                    session_view_nodes::table.filter(session_view_nodes::view_id.eq(&id)),
                )
                .execute(conn)?;
                let n = diesel::delete(session_views::table.find(&id)).execute(conn)?;
                Ok(n > 0)
            })
        })
        .await
    }
}
