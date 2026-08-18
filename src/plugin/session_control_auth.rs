//! Cross-folder authorization for the `session_control` host functions.
//!
//! Same-folder control is free. Cross-folder control requires a persisted
//! **Always** grant for the caller session, or a one-shot **Once** grant the
//! session-control plugin writes after the user answers Approve once.
//!
//! Grants live in the calling plugin's document store (normally
//! `session-control`):
//!
//! | Collection | Key | Meaning |
//! |---|---|---|
//! | `cross_folder_always` | `{caller_session_id}` | Always allow cross-folder from this caller |
//! | `cross_folder_once` | `{caller}\|{target}` | One-shot; deleted when consumed |

use crate::db::Db;
use crate::db::models::Session;
use crate::plugin::host::InvocationContext;

pub(crate) const ALWAYS_COLLECTION: &str = "cross_folder_always";
pub(crate) const ONCE_COLLECTION: &str = "cross_folder_once";

const CROSS_FOLDER_REFUSAL: &str = "cross-folder control requires user approval \
    (Approve once / Approve always)";

/// Authorize a mutating session-control action against `target`.
pub(crate) fn authorize(
    db: &Db,
    plugin_id: &str,
    inv: &InvocationContext,
    target: &Session,
) -> Result<(), String> {
    // Controlling yourself is never cross-folder.
    if inv.session_id.as_deref() == Some(target.id.as_str()) {
        return Ok(());
    }
    // Same folder → free.
    if let Some(caller_folder) = inv.folder_id.as_deref()
        && !caller_folder.is_empty()
        && caller_folder == target.folder_id
    {
        return Ok(());
    }

    let Some(caller_id) = inv.session_id.as_deref().filter(|s| !s.is_empty()) else {
        return Err("cross-folder session control requires a caller session \
             (call during an MCP tool invocation)"
            .into());
    };

    if has_always(db, plugin_id, caller_id)? {
        return Ok(());
    }
    if consume_once(db, plugin_id, caller_id, &target.id)? {
        return Ok(());
    }
    Err(CROSS_FOLDER_REFUSAL.into())
}

/// True when the caller session has a persisted Always grant.
pub(crate) fn has_always(
    db: &Db,
    plugin_id: &str,
    caller_session_id: &str,
) -> Result<bool, String> {
    match db.plugin_store_get_blocking(plugin_id, ALWAYS_COLLECTION, caller_session_id) {
        Ok(Some(raw)) => Ok(grant_flag(&raw)),
        Ok(None) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// Persist an Always grant for `caller_session_id` (plugin / tests).
pub fn grant_always(db: &Db, plugin_id: &str, caller_session_id: &str) -> Result<(), String> {
    db.plugin_store_put_blocking(
        plugin_id,
        ALWAYS_COLLECTION,
        caller_session_id,
        r#"{"approved":true}"#,
    )
    .map_err(|e| e.to_string())
}

/// Persist a one-shot grant the next authorize for `(caller, target)` will consume.
pub fn grant_once(
    db: &Db,
    plugin_id: &str,
    caller_session_id: &str,
    target_session_id: &str,
) -> Result<(), String> {
    let key = once_key(caller_session_id, target_session_id);
    db.plugin_store_put_blocking(plugin_id, ONCE_COLLECTION, &key, r#"{"approved":true}"#)
        .map_err(|e| e.to_string())
}

fn consume_once(
    db: &Db,
    plugin_id: &str,
    caller_session_id: &str,
    target_session_id: &str,
) -> Result<bool, String> {
    let key = once_key(caller_session_id, target_session_id);
    match db.plugin_store_get_blocking(plugin_id, ONCE_COLLECTION, &key) {
        Ok(Some(raw)) if grant_flag(&raw) => {
            let _ = db.plugin_store_delete_blocking(plugin_id, ONCE_COLLECTION, &key);
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

fn once_key(caller: &str, target: &str) -> String {
    format!("{caller}|{target}")
}

fn grant_flag(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("approved").and_then(|a| a.as_bool()))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewFolder;
    use crate::plugin::host::InvocationContext;

    fn seed() -> (Db, Session, Session) {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            db.create_folder(NewFolder {
                id: "fa".into(),
                name: "A".into(),
                path: "/tmp/a".into(),
                created_at: ts.clone(),
            })
            .await
            .unwrap();
            db.create_folder(NewFolder {
                id: "fb".into(),
                name: "B".into(),
                path: "/tmp/b".into(),
                created_at: ts.clone(),
            })
            .await
            .unwrap();
            db.create_session(crate::db::models::NewSession {
                id: "caller".into(),
                name: "caller".into(),
                folder_id: "fa".into(),
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
            db.create_session(crate::db::models::NewSession {
                id: "same".into(),
                name: "same".into(),
                folder_id: "fa".into(),
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
            db.create_session(crate::db::models::NewSession {
                id: "other".into(),
                name: "other".into(),
                folder_id: "fb".into(),
                created_at: ts.clone(),
                last_activity: ts,
                ..Default::default()
            })
            .await
            .unwrap();
        });
        let same = db.get_session_blocking("same").unwrap().unwrap();
        let other = db.get_session_blocking("other").unwrap().unwrap();
        (db, same, other)
    }

    fn caller_inv() -> InvocationContext {
        InvocationContext {
            session_id: Some("caller".into()),
            project_id: None,
            folder_id: Some("fa".into()),
            authority: false,
        }
    }

    #[test]
    fn same_folder_is_free() {
        let (db, same, _) = seed();
        assert!(authorize(&db, "session-control", &caller_inv(), &same).is_ok());
    }

    #[test]
    fn cross_folder_refused_without_grant() {
        let (db, _, other) = seed();
        let err = authorize(&db, "session-control", &caller_inv(), &other).unwrap_err();
        assert!(err.contains("user approval"), "{err}");
    }

    #[test]
    fn once_grant_is_consumed() {
        let (db, _, other) = seed();
        grant_once(&db, "session-control", "caller", "other").unwrap();
        assert!(authorize(&db, "session-control", &caller_inv(), &other).is_ok());
        let err = authorize(&db, "session-control", &caller_inv(), &other).unwrap_err();
        assert!(err.contains("user approval"), "{err}");
    }

    #[test]
    fn always_grant_persists() {
        let (db, _, other) = seed();
        grant_always(&db, "session-control", "caller").unwrap();
        assert!(authorize(&db, "session-control", &caller_inv(), &other).is_ok());
        assert!(authorize(&db, "session-control", &caller_inv(), &other).is_ok());
    }

    #[test]
    fn self_control_is_always_ok() {
        let (db, _, _) = seed();
        let self_sess = db.get_session_blocking("caller").unwrap().unwrap();
        let inv = InvocationContext {
            session_id: Some("caller".into()),
            project_id: None,
            folder_id: Some("fb".into()), // even a mismatched folder_id
            authority: false,
        };
        assert!(authorize(&db, "session-control", &inv, &self_sess).is_ok());
    }
}
