//! Shared rules for the user's saved views (widget dashboards), used by both
//! the REST routes (`routes::views`) and the MCP view tools
//! (`mcp_server::handlers::views`): which refs a widget may hold, the full
//! JSON shape of a view, default widget sizes and auto-placement, and the
//! per-user `views-changed` notification sent after every write.

use serde_json::{Value, json};

use crate::db::Db;
use crate::db::crud::{ViewWidget, WidgetField, WidgetKind, widget_refs};
use crate::db::models::{SessionView, Terminal};
use crate::ws::broadcaster::{Broadcaster, UserAudience, WsEvent};

/// Columns of the view grid (`validate_widgets`' bound).
const GRID_COLS: i32 = 12;

/// Per-user WS frame: one of the user's views was created / updated /
/// deleted, so open lists and editors refresh.
pub const VIEWS_CHANGED_EVENT: &str = "views-changed";

/// What happened to a view, for [`notify_changed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewAction {
    Created,
    Updated,
    Deleted,
}

impl ViewAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ViewAction::Created => "created",
            ViewAction::Updated => "updated",
            ViewAction::Deleted => "deleted",
        }
    }
}

/// Tell every open tab of `user_id` that `view_id` changed. `origin` is the
/// writing browser tab's id (the `X-Peckboard-Tab` header), echoed so that
/// tab can ignore its own autosaves; `None` for agent writes.
pub fn notify_changed(
    broadcaster: &Broadcaster,
    user_id: &str,
    view_id: &str,
    action: ViewAction,
    origin: Option<&str>,
) -> usize {
    broadcaster.send_to_user(
        user_id,
        UserAudience::AllTabs,
        WsEvent {
            event_type: VIEWS_CHANGED_EVENT.into(),
            session_id: String::new(),
            data: json!({ "view_id": view_id, "action": action.as_str(), "origin": origin }),
        },
    )
}

/// Why a widget ref was refused.
#[derive(Debug)]
pub enum RefError {
    /// A referenced object is missing or not the caller's (same answer for
    /// both, so ids can't be probed). Carries the user-facing message.
    NotFound(&'static str),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for RefError {
    fn from(e: anyhow::Error) -> Self {
        RefError::Internal(e)
    }
}

impl std::fmt::Display for RefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefError::NotFound(m) => f.write_str(m),
            RefError::Internal(e) => write!(f, "{e}"),
        }
    }
}

pub fn may_use_terminal(user_id: &str, is_admin: bool, t: &Terminal) -> bool {
    t.user_id == user_id || is_admin
}

/// Every session the widgets reference must be one the caller may open
/// ([`crate::auth::access::may_access_session`]), every terminal must exist
/// and be the caller's (admins: anyone's), every project must exist, and
/// every card must exist in its widget's project. Unknown and foreign ids
/// answer the same [`RefError::NotFound`]. Soft-closed terminals are allowed
/// — their widget shows a reopen placeholder.
pub async fn check_widget_refs(
    db: &Db,
    user_id: &str,
    is_admin: bool,
    widgets: &[ViewWidget],
) -> Result<(), RefError> {
    for id in widget_refs(widgets, WidgetField::SessionId) {
        match db.get_session(id).await? {
            Some(s)
                if crate::auth::access::may_access_session(
                    is_admin,
                    user_id,
                    s.user_id.as_deref(),
                    s.project_id.as_deref(),
                ) => {}
            _ => return Err(RefError::NotFound("session not found")),
        }
    }
    for id in widget_refs(widgets, WidgetField::TerminalId) {
        match db.get_terminal(id).await? {
            Some(t) if may_use_terminal(user_id, is_admin, &t) => {}
            _ => return Err(RefError::NotFound("terminal not found")),
        }
    }
    let projects = widget_refs(widgets, WidgetField::ProjectId);
    if !projects.is_empty() {
        let found = db
            .project_names(projects.iter().map(|s| s.to_string()).collect())
            .await?;
        if projects.iter().any(|p| !found.contains_key(*p)) {
            return Err(RefError::NotFound("project not found"));
        }
    }
    let cards = widget_refs(widgets, WidgetField::CardId);
    if !cards.is_empty() {
        let found = db
            .card_titles(cards.iter().map(|s| s.to_string()).collect())
            .await?;
        let in_project = |w: &ViewWidget| match (&w.card_id, &w.project_id) {
            (Some(c), Some(p)) => found.get(c).is_some_and(|(_, cp)| cp == p),
            (Some(_), None) => false,
            (None, _) => true,
        };
        if !widgets.iter().all(in_project) {
            return Err(RefError::NotFound("card not found"));
        }
    }
    Ok(())
}

pub fn summary_json(v: &SessionView) -> Value {
    json!({
        "id": v.id,
        "name": v.name,
        "created_at": v.created_at,
        "updated_at": v.updated_at,
    })
}

/// The full view, plus `terminals: {id: {name, host_label, plugin_id,
/// host_id, closed}}`, `projects: {id: {name}}`, and `cards: {id:
/// {title}}` for the targets its widgets reference.
pub async fn full_json(
    db: &Db,
    user_id: &str,
    is_admin: bool,
    (v, widgets): (SessionView, Vec<ViewWidget>),
) -> anyhow::Result<Value> {
    let mut terminals = serde_json::Map::new();
    for id in widget_refs(&widgets, WidgetField::TerminalId) {
        if let Some(t) = db.get_terminal(id).await?
            && may_use_terminal(user_id, is_admin, &t)
        {
            terminals.insert(
                t.id.clone(),
                json!({
                    "name": t.name,
                    "host_label": t.host_label,
                    "plugin_id": t.plugin_id,
                    "host_id": t.host_id,
                    "closed": t.closed_at.is_some(),
                }),
            );
        }
    }
    let ids = |field| -> Vec<String> {
        widget_refs(&widgets, field)
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    let project_ids = ids(WidgetField::ProjectId);
    let projects: serde_json::Map<String, Value> = if project_ids.is_empty() {
        serde_json::Map::new()
    } else {
        db.project_names(project_ids)
            .await?
            .into_iter()
            .map(|(id, name)| (id, json!({ "name": name })))
            .collect()
    };
    let card_ids = ids(WidgetField::CardId);
    let cards: serde_json::Map<String, Value> = if card_ids.is_empty() {
        serde_json::Map::new()
    } else {
        db.card_titles(card_ids)
            .await?
            .into_iter()
            .map(|(id, (title, _))| (id, json!({ "title": title })))
            .collect()
    };
    let mut out = summary_json(&v);
    out["widgets"] = json!(widgets);
    out["terminals"] = Value::Object(terminals);
    out["projects"] = Value::Object(projects);
    out["cards"] = Value::Object(cards);
    Ok(out)
}

/// The size (`w`, `h`) a new widget of `kind` gets — the same defaults as
/// the web Add-widget menu (`WIDGET_SPECS` in `dashboard/registry.ts`).
pub fn default_size(kind: WidgetKind) -> (i32, i32) {
    match kind {
        WidgetKind::Session | WidgetKind::Terminal | WidgetKind::Report => (6, 10),
        WidgetKind::Project
        | WidgetKind::Todos
        | WidgetKind::Attention
        | WidgetKind::ReviewQueue => (4, 8),
        WidgetKind::Dependencies => (6, 9),
        WidgetKind::Workers | WidgetKind::ReviewQuality | WidgetKind::Worktrees => (6, 7),
        WidgetKind::Background => (6, 6),
        WidgetKind::Repeating | WidgetKind::Note => (4, 6),
        WidgetKind::Orchestrators | WidgetKind::Prs | WidgetKind::SshActivity => (6, 8),
        WidgetKind::SshHosts => (4, 7),
    }
}

/// Top-left-most free `(x, y)` for a `w`×`h` widget among `widgets`
/// (row-major scan, like the web grid's `findFreeSlot`); below everything
/// when no gap fits. `w` must already be within the grid.
pub fn free_slot(widgets: &[ViewWidget], w: i32, h: i32) -> (i32, i32) {
    let bottom = widgets.iter().map(|o| o.y + o.h).max().unwrap_or(0);
    let collides = |x: i32, y: i32| {
        widgets
            .iter()
            .any(|o| x < o.x + o.w && o.x < x + w && y < o.y + o.h && o.y < y + h)
    };
    for y in 0..=bottom {
        for x in 0..=(GRID_COLS - w) {
            if !collides(x, y) {
                return (x, y);
            }
        }
    }
    (0, bottom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(id: &str, x: i32, y: i32, w: i32, h: i32) -> ViewWidget {
        ViewWidget {
            id: id.into(),
            kind: WidgetKind::Note,
            x,
            y,
            w,
            h,
            ..Default::default()
        }
    }

    #[test]
    fn free_slot_fills_gaps_then_goes_below() {
        assert_eq!(free_slot(&[], 6, 10), (0, 0));
        let one = [rect("a", 0, 0, 6, 10)];
        assert_eq!(free_slot(&one, 6, 10), (6, 0));
        let full_row = [rect("a", 0, 0, 6, 10), rect("b", 6, 0, 6, 10)];
        assert_eq!(free_slot(&full_row, 4, 6), (0, 10));
        // A short neighbour leaves room underneath it.
        let stepped = [rect("a", 0, 0, 6, 4), rect("b", 6, 0, 6, 10)];
        assert_eq!(free_slot(&stepped, 6, 6), (0, 4));
    }
}
