//! Saved views (`/api/me/views`). A view is a named, per-user dashboard of
//! [`ViewWidget`]s on a 12-column grid, stored as `view_widgets` rows.
//!
//! Views saved before widgets existed hold a split tree in
//! `session_view_nodes` (wire shape [`ViewLayout`]). Such a view is
//! converted to widgets once, on first read ([`ViewLayout::to_widgets`]),
//! and `view_widgets_converted` records that its widget rows are now
//! authoritative. The node rows are never deleted.

use std::collections::{HashMap, HashSet};

use diesel::prelude::*;
use serde::{Deserialize, Serialize, Serializer};

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// Deepest allowed layout nesting (a lone leaf is depth 1).
pub const MAX_VIEW_DEPTH: usize = 8;
/// Most leaves (session panes) one view may hold.
pub const MAX_VIEW_LEAVES: usize = 16;
/// Longest allowed view name, in characters.
pub const MAX_VIEW_NAME_CHARS: usize = 100;
/// Columns in the widget grid.
pub const VIEW_GRID_COLS: i32 = 12;
/// Most widgets one view may hold.
pub const MAX_VIEW_WIDGETS: usize = 24;
/// Widget height bounds, in grid rows.
pub const MIN_WIDGET_H: i32 = 2;
pub const MAX_WIDGET_H: i32 = 40;
/// Longest allowed client-generated widget id, in characters.
const MAX_WIDGET_ID_CHARS: usize = 100;
/// Grid height a legacy full-height column converts to.
const LEGACY_VIEW_ROWS: i32 = 16;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    Row,
    Col,
}

impl SplitDir {
    #[cfg(test)]
    fn as_str(self) -> &'static str {
        match self {
            SplitDir::Row => "row",
            SplitDir::Col => "col",
        }
    }
}

/// Wire layout of a legacy split view: `{"kind":"split","dir","children",
/// "ratios"}` or `{"kind":"leaf","sessionId"}` / `{"kind":"leaf",
/// "terminalId"}`. `ratios[i]` is `children[i]`'s share. A leaf with
/// neither id is an empty pane; one with both is rejected by
/// [`ViewLayout::validate`]. Still accepted in request bodies, converted
/// to widgets by [`ViewLayout::to_widgets`].
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
        #[serde(
            rename = "terminalId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        terminal_id: Option<String>,
    },
}

impl ViewLayout {
    /// Structural limits: splits have ≥ 2 children and one positive,
    /// finite ratio per child; depth ≤ [`MAX_VIEW_DEPTH`]; leaves ≤
    /// [`MAX_VIEW_LEAVES`]; no leaf names both a session and a terminal.
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
            ViewLayout::Leaf {
                session_id: Some(_),
                terminal_id: Some(_),
            } => Err("a pane can show a session or a terminal, not both".into()),
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

    /// The tree as grid widgets: the root fills 12 columns ×
    /// [`LEGACY_VIEW_ROWS`] rows and each split divides its rect by the
    /// ratios (`row` side by side, `col` stacked), rounded to whole cells.
    /// When rounding can't fit a pane (too many panes for the cells) the
    /// leaves fall back to a two-column flow of 6×8 tiles in tree order.
    /// Each widget gets a fresh id.
    pub fn to_widgets(&self) -> Vec<ViewWidget> {
        let mut out = Vec::new();
        self.place(0, 0, VIEW_GRID_COLS, LEGACY_VIEW_ROWS, &mut out);
        if validate_widgets(&out).is_ok() {
            return out;
        }
        out.truncate(MAX_VIEW_WIDGETS);
        for (i, w) in out.iter_mut().enumerate() {
            let i = i as i32;
            (w.x, w.y, w.w, w.h) = ((i % 2) * 6, (i / 2) * 8, 6, 8);
        }
        out
    }

    fn place(&self, x: i32, y: i32, w: i32, h: i32, out: &mut Vec<ViewWidget>) {
        match self {
            ViewLayout::Leaf {
                session_id,
                terminal_id,
            } => {
                let kind = if terminal_id.is_some() {
                    WidgetKind::Terminal
                } else {
                    WidgetKind::Session
                };
                out.push(ViewWidget {
                    id: uuid::Uuid::new_v4().to_string(),
                    kind,
                    x,
                    y,
                    w,
                    h,
                    session_id: if kind == WidgetKind::Session {
                        session_id.clone()
                    } else {
                        None
                    },
                    terminal_id: terminal_id.clone(),
                    project_id: None,
                });
            }
            ViewLayout::Split {
                dir,
                children,
                ratios,
            } => {
                let total: f64 = ratios.iter().sum();
                let len = if *dir == SplitDir::Row { w } else { h };
                let (mut acc, mut start) = (0.0, 0);
                for (i, (child, r)) in children.iter().zip(ratios).enumerate() {
                    acc += r;
                    let end = if i + 1 == children.len() {
                        len
                    } else {
                        ((acc / total) * len as f64).round() as i32
                    };
                    let span = (end - start).max(0);
                    match dir {
                        SplitDir::Row => child.place(x + start, y, span, h, out),
                        SplitDir::Col => child.place(x, y + start, w, span, out),
                    }
                    start = end.max(start);
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WidgetKind {
    Session,
    Terminal,
    Project,
}

impl WidgetKind {
    fn as_str(self) -> &'static str {
        match self {
            WidgetKind::Session => "session",
            WidgetKind::Terminal => "terminal",
            WidgetKind::Project => "project",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "terminal" => WidgetKind::Terminal,
            "project" => WidgetKind::Project,
            _ => WidgetKind::Session,
        }
    }

    /// The wire key of this kind's ref field.
    fn ref_key(self) -> &'static str {
        match self {
            WidgetKind::Session => "sessionId",
            WidgetKind::Terminal => "terminalId",
            WidgetKind::Project => "projectId",
        }
    }
}

/// One widget of a saved view: a rect on the 12-column grid plus the ref
/// matching `kind` (`sessionId` / `terminalId` / `projectId`; `null` is an
/// empty placeholder). Serializes only the matching ref key, always
/// present (possibly `null`). [`validate_widgets`] rejects a mismatched ref.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct ViewWidget {
    pub id: String,
    pub kind: WidgetKind,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
    #[serde(rename = "terminalId", default)]
    pub terminal_id: Option<String>,
    #[serde(rename = "projectId", default)]
    pub project_id: Option<String>,
}

impl ViewWidget {
    /// The ref matching `kind`.
    pub fn target(&self) -> Option<&str> {
        match self.kind {
            WidgetKind::Session => self.session_id.as_deref(),
            WidgetKind::Terminal => self.terminal_id.as_deref(),
            WidgetKind::Project => self.project_id.as_deref(),
        }
    }

    fn overlaps(&self, o: &ViewWidget) -> bool {
        let (ax, ay, aw, ah) = (self.x as i64, self.y as i64, self.w as i64, self.h as i64);
        let (bx, by, bw, bh) = (o.x as i64, o.y as i64, o.w as i64, o.h as i64);
        ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
    }
}

impl Serialize for ViewWidget {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(7))?;
        m.serialize_entry("id", &self.id)?;
        m.serialize_entry("kind", &self.kind)?;
        m.serialize_entry("x", &self.x)?;
        m.serialize_entry("y", &self.y)?;
        m.serialize_entry("w", &self.w)?;
        m.serialize_entry("h", &self.h)?;
        m.serialize_entry(self.kind.ref_key(), &self.target())?;
        m.end()
    }
}

/// Every id of `kind` the widgets reference, in order, deduplicated.
pub fn widget_refs(widgets: &[ViewWidget], kind: WidgetKind) -> Vec<&str> {
    let mut seen = HashSet::new();
    widgets
        .iter()
        .filter(|w| w.kind == kind)
        .filter_map(ViewWidget::target)
        .filter(|id| seen.insert(*id))
        .collect()
}

/// Grid bounds (`x` 0..=11, `w` 1..=12, `x + w` ≤ 12, `y` ≥ 0, `h`
/// 2..=40), no overlaps, ≤ [`MAX_VIEW_WIDGETS`], unique non-empty ids, and
/// only the ref matching `kind` set. `Err` is the user-facing reason.
pub fn validate_widgets(widgets: &[ViewWidget]) -> Result<(), String> {
    if widgets.len() > MAX_VIEW_WIDGETS {
        return Err(format!("a view holds at most {MAX_VIEW_WIDGETS} widgets"));
    }
    let mut ids = HashSet::new();
    for w in widgets {
        if w.id.is_empty() || w.id.chars().count() > MAX_WIDGET_ID_CHARS {
            return Err(format!(
                "widget id must be 1-{MAX_WIDGET_ID_CHARS} characters"
            ));
        }
        if !ids.insert(w.id.as_str()) {
            return Err(format!("duplicate widget id '{}'", w.id));
        }
        if !(0..VIEW_GRID_COLS).contains(&w.x)
            || !(1..=VIEW_GRID_COLS).contains(&w.w)
            || w.x + w.w > VIEW_GRID_COLS
        {
            return Err(format!(
                "widget '{}' must fit the {VIEW_GRID_COLS}-column grid",
                w.id
            ));
        }
        if w.y < 0 || !(MIN_WIDGET_H..=MAX_WIDGET_H).contains(&w.h) {
            return Err(format!(
                "widget '{}' needs y >= 0 and h in {MIN_WIDGET_H}..={MAX_WIDGET_H}",
                w.id
            ));
        }
        let stray = match w.kind {
            WidgetKind::Session => w.terminal_id.is_some() || w.project_id.is_some(),
            WidgetKind::Terminal => w.session_id.is_some() || w.project_id.is_some(),
            WidgetKind::Project => w.session_id.is_some() || w.terminal_id.is_some(),
        };
        if stray {
            return Err(format!(
                "widget '{}' may only set {}",
                w.id,
                w.kind.ref_key()
            ));
        }
    }
    for (i, a) in widgets.iter().enumerate() {
        if let Some(b) = widgets[i + 1..].iter().find(|b| a.overlaps(b)) {
            return Err(format!("widgets '{}' and '{}' overlap", a.id, b.id));
        }
    }
    Ok(())
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

/// Replace every widget row of `view_id` with `widgets` and mark the view
/// converted. A ref to a session, terminal, or project that no longer
/// exists is stored blank — the same state `ON DELETE SET NULL` leaves
/// behind when the row goes later. A soft-closed terminal keeps its id so
/// the widget can offer to reopen it.
fn store_widgets(
    conn: &mut SqliteConnection,
    view_id: &str,
    widgets: &[ViewWidget],
) -> anyhow::Result<()> {
    let refs = |kind| -> Vec<String> {
        widget_refs(widgets, kind)
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    let live_sessions: HashSet<String> = sessions::table
        .filter(sessions::id.eq_any(refs(WidgetKind::Session)))
        .select(sessions::id)
        .load(conn)?
        .into_iter()
        .collect();
    let live_terminals: HashSet<String> = terminals::table
        .filter(terminals::id.eq_any(refs(WidgetKind::Terminal)))
        .select(terminals::id)
        .load(conn)?
        .into_iter()
        .collect();
    let live_projects: HashSet<String> = projects::table
        .filter(projects::id.eq_any(refs(WidgetKind::Project)))
        .select(projects::id)
        .load(conn)?
        .into_iter()
        .collect();
    let live = |id: &Option<String>, set: &HashSet<String>| id.clone().filter(|i| set.contains(i));
    let rows: Vec<ViewWidgetRow> = widgets
        .iter()
        .map(|w| ViewWidgetRow {
            id: w.id.clone(),
            view_id: view_id.to_string(),
            kind: w.kind.as_str().to_string(),
            x: w.x,
            y: w.y,
            w: w.w,
            h: w.h,
            session_id: live(&w.session_id, &live_sessions),
            terminal_id: live(&w.terminal_id, &live_terminals),
            project_id: live(&w.project_id, &live_projects),
        })
        .collect();
    diesel::delete(view_widgets::table.filter(view_widgets::view_id.eq(view_id))).execute(conn)?;
    if !rows.is_empty() {
        diesel::insert_into(view_widgets::table)
            .values(&rows)
            .execute(conn)?;
    }
    diesel::insert_or_ignore_into(view_widgets_converted::table)
        .values((
            view_widgets_converted::view_id.eq(view_id),
            view_widgets_converted::converted_at.eq(chrono::Utc::now().to_rfc3339()),
        ))
        .execute(conn)?;
    Ok(())
}

/// A view's widgets, top-left first. A legacy view (never converted)
/// with split-tree nodes is converted and stored first, once.
fn load_widgets(conn: &mut SqliteConnection, view_id: &str) -> anyhow::Result<Vec<ViewWidget>> {
    let converted = view_widgets_converted::table
        .find(view_id)
        .select(view_widgets_converted::view_id)
        .first::<String>(conn)
        .optional()?
        .is_some();
    if !converted {
        let has_nodes = session_view_nodes::table
            .filter(session_view_nodes::view_id.eq(view_id))
            .select(session_view_nodes::id)
            .first::<String>(conn)
            .optional()?
            .is_some();
        let widgets = if has_nodes {
            load_layout(conn, view_id)?.to_widgets()
        } else {
            Vec::new()
        };
        store_widgets(conn, view_id, &widgets)?;
    }
    let rows: Vec<ViewWidgetRow> = view_widgets::table
        .filter(view_widgets::view_id.eq(view_id))
        .select(ViewWidgetRow::as_select())
        .order((
            view_widgets::y.asc(),
            view_widgets::x.asc(),
            view_widgets::id.asc(),
        ))
        .load(conn)?;
    Ok(rows
        .into_iter()
        .map(|r| ViewWidget {
            id: r.id,
            kind: WidgetKind::parse(&r.kind),
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
            session_id: r.session_id,
            terminal_id: r.terminal_id,
            project_id: r.project_id,
        })
        .collect())
}

/// Insert `layout` as legacy `session_view_nodes` rows rooted under
/// `parent` — the pre-widget storage, kept so tests can build views the
/// way older releases saved them.
#[cfg(test)]
pub(crate) fn insert_layout(
    conn: &mut SqliteConnection,
    view_id: &str,
    parent: Option<&str>,
    position: i32,
    ratio: f64,
    layout: &ViewLayout,
) -> anyhow::Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    let (kind, dir, session_id, terminal_id) = match layout {
        ViewLayout::Split { dir, .. } => ("split", Some(dir.as_str().to_string()), None, None),
        ViewLayout::Leaf {
            session_id,
            terminal_id,
        } => ("leaf", None, session_id.clone(), terminal_id.clone()),
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
            terminal_id,
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
/// row reads back as a single empty pane.
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
                terminal_id: node.terminal_id.clone(),
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
        None => ViewLayout::Leaf {
            session_id: None,
            terminal_id: None,
        },
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
    /// A user's saved views, oldest first (no widgets).
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

    /// One of `user_id`'s views with its widgets (converting a legacy
    /// split tree on first read); `None` when it doesn't exist or belongs
    /// to someone else.
    pub async fn get_session_view(
        &self,
        user_id: &str,
        id: &str,
    ) -> anyhow::Result<Option<(SessionView, Vec<ViewWidget>)>> {
        let (user_id, id) = (user_id.to_string(), id.to_string());
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let Some(view) = find_owned_view(conn, &user_id, &id)? else {
                    return Ok(None);
                };
                let widgets = load_widgets(conn, &view.id)?;
                Ok(Some((view, widgets)))
            })
        })
        .await
    }

    /// Create a view. Callers validate `name` / `widgets` first.
    pub async fn create_session_view(
        &self,
        user_id: &str,
        name: &str,
        widgets: Vec<ViewWidget>,
    ) -> anyhow::Result<(SessionView, Vec<ViewWidget>)> {
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
                store_widgets(conn, &view.id, &widgets)?;
                let widgets = load_widgets(conn, &view.id)?;
                Ok((view, widgets))
            })
        })
        .await
    }

    /// Rename and/or replace all widgets of one of `user_id`'s views, in
    /// one transaction. `None` when the view isn't the user's.
    pub async fn update_session_view(
        &self,
        user_id: &str,
        id: &str,
        name: Option<String>,
        widgets: Option<Vec<ViewWidget>>,
    ) -> anyhow::Result<Option<(SessionView, Vec<ViewWidget>)>> {
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
                if let Some(widgets) = widgets {
                    store_widgets(conn, &view.id, &widgets)?;
                }
                let widgets = load_widgets(conn, &view.id)?;
                Ok(Some((view, widgets)))
            })
        })
        .await
    }

    /// Delete one of `user_id`'s views (nodes and widgets cascade). `false`
    /// when it isn't theirs or doesn't exist.
    pub async fn delete_session_view(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        let (user_id, id) = (user_id.to_string(), id.to_string());
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                if find_owned_view(conn, &user_id, &id)?.is_none() {
                    return Ok(false);
                }
                // Explicit even though the FKs cascade, so the delete never
                // depends on the connection's foreign_keys pragma.
                diesel::delete(
                    session_view_nodes::table.filter(session_view_nodes::view_id.eq(&id)),
                )
                .execute(conn)?;
                diesel::delete(view_widgets::table.filter(view_widgets::view_id.eq(&id)))
                    .execute(conn)?;
                diesel::delete(view_widgets_converted::table.find(&id)).execute(conn)?;
                let n = diesel::delete(session_views::table.find(&id)).execute(conn)?;
                Ok(n > 0)
            })
        })
        .await
    }

    /// `{id: name}` for the projects among `ids` that exist.
    pub async fn project_names(&self, ids: Vec<String>) -> anyhow::Result<HashMap<String, String>> {
        self.with_conn(move |conn| {
            Ok(projects::table
                .filter(projects::id.eq_any(&ids))
                .select((projects::id, projects::name))
                .load::<(String, String)>(conn)?
                .into_iter()
                .collect())
        })
        .await
    }
}
