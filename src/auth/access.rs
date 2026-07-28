//! Shared session-ownership rule for the REST and WebSocket layers.
//!
//! Sessions are per-user (`sessions.user_id`, backfilled for legacy rows by
//! `db::repair::backfill_session_owners`); projects, cards, and folders are
//! not (see the "any logged-in user can read every other user's sessions"
//! card — projects/cards/folders scoping was deliberately deferred to a
//! follow-up). A session attached to a project (`project_id.is_some()`,
//! i.e. a worker or expert session driving a card) is reachable by any
//! logged-in user, matching the board it belongs to being shared; a plain
//! chat session is reachable only by its owner. Admins always have full
//! access.

/// Whether a logged-in user may read or act on a session.
///
/// - `is_admin`: superuser, always allowed.
/// - `user_id`: the caller's id.
/// - `session_user_id`: the session's `user_id` column (`None` for legacy /
///   ambiguously-owned rows).
/// - `session_project_id`: the session's `project_id` column; `Some` means
///   this is a worker/expert session tied to a (shared) board.
pub fn may_access_session(
    is_admin: bool,
    user_id: &str,
    session_user_id: Option<&str>,
    session_project_id: Option<&str>,
) -> bool {
    is_admin || session_project_id.is_some() || session_user_id == Some(user_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_may_access_anything() {
        assert!(may_access_session(
            true,
            "admin",
            Some("someone-else"),
            None
        ));
        assert!(may_access_session(true, "admin", None, None));
    }

    #[test]
    fn owner_may_access_own_session() {
        assert!(may_access_session(false, "u1", Some("u1"), None));
    }

    #[test]
    fn non_owner_may_not_access_a_plain_session() {
        assert!(!may_access_session(false, "u1", Some("u2"), None));
    }

    #[test]
    fn unowned_plain_session_never_matches_a_non_admin() {
        assert!(!may_access_session(false, "u1", None, None));
    }

    #[test]
    fn board_attached_session_is_shared_regardless_of_owner() {
        // Worker/expert sessions carry project_id — reachable by any
        // logged-in user, same as the board itself.
        assert!(may_access_session(false, "u1", Some("u2"), Some("proj-1")));
        assert!(may_access_session(false, "u1", None, Some("proj-1")));
    }
}
