//! `run_command` — a general CLI executor gated by per-command **user
//! approval**.
//!
//! Unlike `git` / `run_tests` (which only ever invoke a fixed allowlist), this
//! tool can run any bare program — but only after a permission check:
//!
//! 1. If the program has a persisted **"always" approval** (recorded when a
//!    user answered *Approve always*, and manageable from Settings), it runs
//!    immediately via the unrestricted exec (`exec_any` semantics).
//! 2. Otherwise the caller emits an interactive prompt to the user (*Approve
//!    once / Approve always / Deny*) and returns `awaiting_approval` — the
//!    worker turn ends. When the user answers, the session resumes and the
//!    agent re-calls `run_command`; this time [`decide`] reads the user's real
//!    answer and runs the command, refuses it, or (for *always*) records a
//!    persisted grant keyed by program name.
//!
//! The pending request is correlated across the two calls by a key derived
//! from the caller's session + the exact command, so a re-call with the same
//! command finds its in-flight approval rather than prompting again.
//!
//! This module is host-native: all store / exec / answer operations call the
//! `crate::plugin::host::*_impl` functions directly under the
//! [`super::host_bridge::NS`] namespace. It is intentionally synchronous so it
//! can run inside `spawn_blocking`; the async parts (emitting the question via
//! the broadcaster) live in the handler.

use serde_json::Value;

use super::host_bridge::{HostCtx, NS};
use crate::db::Db;
use crate::plugin::host::{
    InvocationContext, exec_impl, get_answer_impl, store_delete_impl, store_get_impl,
    store_put_impl,
};

/// Document-store collections backing approvals (keyed under `NS`).
const ALWAYS_COLLECTION: &str = "cli_always"; // key = program name
const PENDING_COLLECTION: &str = "cli_pending"; // key = pending correlation key

const APPROVE_ONCE: &str = "Approve once";
const APPROVE_ALWAYS: &str = "Approve always";
const DENY: &str = "Deny";

/// The outcome of a single `decide` call, mapped to an MCP response by the
/// handler. `Ran` carries the (decorated) exec result; `NeedsPrompt` asks the
/// handler to emit the interactive question; `StillWaiting`/`Denied` are the
/// re-call outcomes.
pub enum Decision {
    Ran(Value),
    Denied(String),
    NeedsPrompt {
        token: String,
        display: String,
        options: Vec<String>,
    },
    StillWaiting(String),
}

/// The synchronous core of `run_command`, safe to run inside `spawn_blocking`.
/// Ports the plugin's two-step approval flow, minus the operator allowlist.
pub fn decide(
    db: &Db,
    inv: &InvocationContext,
    session_id: &str,
    command: &str,
    argv: &[String],
    timeout: Option<u64>,
) -> Result<Decision, String> {
    // 1. Persisted "always" approval → run now (unrestricted exec).
    if always_approved(db, command)? {
        return Ok(Decision::Ran(run_now(
            db, inv, command, argv, timeout, "always",
        )?));
    }

    // 2. Interactive approval, correlated across the two-step flow.
    let key = pending_key(session_id, command, argv);
    match pending_token(db, &key)? {
        None => {
            // First call: park a pending request and ask the handler to prompt.
            let token = HostCtx::gen_id();
            store_put(
                db,
                PENDING_COLLECTION,
                &key,
                serde_json::json!({ "token": token, "command": command, "args": argv }),
            )?;
            Ok(Decision::NeedsPrompt {
                token,
                display: display(command, argv),
                options: vec![APPROVE_ONCE.into(), APPROVE_ALWAYS.into(), DENY.into()],
            })
        }
        Some(token) => {
            // Re-call: read the user's real answer. A status other than
            // "answered" ("pending", or "unknown" during the brief window
            // before the question event lands) means the user hasn't decided
            // yet — keep waiting, and never re-prompt (the pending record is
            // our proof we already asked).
            let ans = get_answer(db, inv, &token)?;
            let status = ans.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status != "answered" {
                return Ok(Decision::StillWaiting(display(command, argv)));
            }

            let rejected = ans
                .get("rejected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let answer = ans.get("answer").and_then(|v| v.as_str()).unwrap_or("");
            // Consume the one-shot pending record now that it is decided.
            clear_pending(db, &key)?;
            if rejected || answer == DENY {
                return Ok(Decision::Denied(format!(
                    "the user denied running `{}`",
                    display(command, argv)
                )));
            }
            if answer.starts_with(APPROVE_ALWAYS) {
                store_put(
                    db,
                    ALWAYS_COLLECTION,
                    command,
                    serde_json::json!({ "approved": true }),
                )?;
                Ok(Decision::Ran(run_now(
                    db,
                    inv,
                    command,
                    argv,
                    timeout,
                    "approved_always",
                )?))
            } else if answer.starts_with(APPROVE_ONCE) {
                Ok(Decision::Ran(run_now(
                    db,
                    inv,
                    command,
                    argv,
                    timeout,
                    "approved_once",
                )?))
            } else {
                Ok(Decision::Denied(format!(
                    "the user did not approve running `{}` (answer: {answer})",
                    display(command, argv)
                )))
            }
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────

fn run_now(
    db: &Db,
    inv: &InvocationContext,
    command: &str,
    argv: &[String],
    timeout: Option<u64>,
    approved_via: &str,
) -> Result<Value, String> {
    let mut req = serde_json::json!({ "command": command, "args": argv });
    if let Some(t) = timeout {
        req["timeout_secs"] = serde_json::json!(t);
    }
    // exec_any semantics: any bare executable, folder-pinned cwd.
    let out = exec_impl(db, &req.to_string(), inv, false);
    let mut result = parse_envelope(&out)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "command".to_string(),
            serde_json::json!(display(command, argv)),
        );
        obj.insert("approved_via".to_string(), serde_json::json!(approved_via));
    }
    Ok(result)
}

fn always_approved(db: &Db, program: &str) -> Result<bool, String> {
    let v = store_get(db, ALWAYS_COLLECTION, program)?;
    Ok(v["value"]["approved"].as_bool().unwrap_or(false))
}

fn pending_token(db: &Db, key: &str) -> Result<Option<String>, String> {
    let v = store_get(db, PENDING_COLLECTION, key)?;
    Ok(v["value"]["token"].as_str().map(str::to_string))
}

fn clear_pending(db: &Db, key: &str) -> Result<(), String> {
    let out = store_delete_impl(
        db,
        NS,
        &serde_json::json!({ "collection": PENDING_COLLECTION, "key": key }).to_string(),
    );
    parse_envelope(&out)?;
    Ok(())
}

fn store_put(db: &Db, collection: &str, key: &str, data: Value) -> Result<(), String> {
    let out = store_put_impl(
        db,
        NS,
        &serde_json::json!({ "collection": collection, "key": key, "data": data }).to_string(),
    );
    parse_envelope(&out)?;
    Ok(())
}

fn store_get(db: &Db, collection: &str, key: &str) -> Result<Value, String> {
    let out = store_get_impl(
        db,
        NS,
        &serde_json::json!({ "collection": collection, "key": key }).to_string(),
    );
    parse_envelope(&out)
}

fn get_answer(db: &Db, inv: &InvocationContext, token: &str) -> Result<Value, String> {
    let out = get_answer_impl(db, inv, &serde_json::json!({ "token": token }).to_string());
    parse_envelope(&out)
}

fn parse_envelope(out: &str) -> Result<Value, String> {
    let v: Value =
        serde_json::from_str(out).map_err(|e| format!("host returned invalid json: {e}"))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    Ok(v)
}

/// A human-readable rendering of the command for prompts and output.
pub fn display(command: &str, argv: &[String]) -> String {
    if argv.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", argv.join(" "))
    }
}

/// Correlation key for an in-flight approval: the caller's session + the exact
/// command, hashed so it stays within the store's key-length limit. Approvals
/// are remembered per program name; this key only matches a *re-call of the
/// same command in the same session* to its pending prompt.
pub fn pending_key(session_id: &str, command: &str, argv: &[String]) -> String {
    let mut h = Fnv::new();
    h.write(session_id.as_bytes());
    h.write(&[0]);
    h.write(command.as_bytes());
    for a in argv {
        h.write(&[0]);
        h.write(a.as_bytes());
    }
    format!("{command}.{:016x}", h.finish())
}

/// FNV-1a 64-bit — a tiny, dependency-free hash for the correlation key.
struct Fnv(u64);
impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf29ce484222325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_key_is_stable_and_command_specific() {
        let a = pending_key("s1", "rg", &["foo".into()]);
        let b = pending_key("s1", "rg", &["foo".into()]);
        let c = pending_key("s1", "rg", &["bar".into()]);
        let d = pending_key("s2", "rg", &["foo".into()]);
        assert_eq!(a, b, "same inputs → same key");
        assert_ne!(a, c, "different args → different key");
        assert_ne!(a, d, "different session → different key");
        assert!(a.starts_with("rg."));
    }

    #[test]
    fn display_joins_argv() {
        assert_eq!(display("ls", &[]), "ls");
        assert_eq!(display("rg", &["-n".into(), "foo".into()]), "rg -n foo");
    }
}
