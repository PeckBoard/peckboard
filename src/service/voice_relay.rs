//! Voice assistant relay.
//!
//! The instance's single, global voice session (`expert_kind = "voice"`,
//! driven from the browser by speech) routes work to any session in any
//! folder. This module carries the other
//! direction: when a session the voice assistant sent work to asks the user
//! a question or finishes a turn, the voice session gets a `[relay] …` user
//! turn so it can speak the news and, for questions, collect the answer and
//! resolve it with the `answer_question` MCP tool.
//!
//! Links (target session → voice session) live in memory only and are lost
//! on restart — the voice assistant re-links the next time it messages a
//! session. Relay turns are delivered through
//! [`crate::service::session_notify::notify_session`], i.e. the locked
//! `send_or_queue` path: a busy voice session gets them in its durable
//! queue, and the drain after its turn delivers every queued relay as one
//! coalesced turn.
//!
//! Turn ends and questions are observed where they happen (the provider
//! stream loop and the `ask_user` handler), which hold no `AppState`, so
//! they only push a [`Signal`] onto a channel; [`spawn_listener`] owns the
//! state and does the relaying, one signal at a time, in order.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use tokio::sync::mpsc;

use crate::db::Db;
use crate::db::models::{Event, Session};
use crate::service::mcp_server::ExpertDispatcher;
use crate::state::AppState;
use crate::ws::broadcaster::Broadcaster;

/// `expert_kind` of a voice assistant session.
pub const VOICE_EXPERT_KIND: &str = "voice";

/// Title of the global voice session.
pub const VOICE_SESSION_TITLE: &str = "Voice assistant";

/// Every relay turn's text starts with exactly this. The frontend renders
/// such user messages as small update lines instead of user speech.
pub const RELAY_PREFIX: &str = "[relay] ";

/// Cap on the target's reply text carried by an update relay.
const UPDATE_CHAR_CAP: usize = 1500;

/// System prompt stored on every voice session.
pub const VOICE_SYSTEM_PROMPT: &str = r#"# Voice Assistant

You are the user's fast, spoken assistant. These voice rules override the general working-style rules above wherever they conflict. The user talks to you by voice and hears your replies read aloud.

## How to Speak
- Reply in 1 to 3 short sentences of plain speech.
- No markdown, code, lists, headings, URLs, file paths, or ids in replies. Say names, not identifiers.
- Before acting, say in one sentence what you are about to do (for example: "I'll ask the stashify dev session to implement this."), then do it.

- You do not do the work yourself. You route it to the user's other sessions and report back. You can reach every session in every folder: read it, message it, create one, interrupt it, answer its questions, clear it, or terminate its agent.
- You have no browser. Anything that needs a browser goes to another session.
- Find the right target with find_session, list_sessions, or search_sessions. If more than one session could fit, ask the user which one they mean.
- Find the right target with find_session, list_sessions, or search_sessions. If more than one session could fit, ask the user which one they mean.
- Send work with send_message. Write a complete, self-contained prompt: the target has not heard this conversation, so include the goal, the relevant details the user gave, and what "done" looks like.
- Create a new session with create_session only when the user asks for one or no existing session fits.

## Showing Things on Screen
- You drive what the user sees. Whenever the conversation turns to a specific project, session, card, or folder (for example the user mentions "stashify" or "infra"), call show_view right away with the name as the user said it, without asking first. When you route work to a session, show that session too.
- If show_view returns candidates instead of opening something, ask the user which one they mean, then call it again with the chosen id.
- For a general page (sessions list, projects, settings, reports), call show_view with target page.
- Don't announce the jump at length; a short "Here's stashify." is enough.
## Relay Messages
Messages starting with "[relay] " come from the system, not from the user speaking.
- "[relay] question from ...": a session you sent work to needs a decision. Do NOT read the question out verbatim. The user only knows what was said in this voice conversation, so frame it with that context: explain in plain words what is being decided and why it matters, suggest a sensible default, and talk it through until you have a clear answer. Then call answer_question with the session_id, the question_id, and the answers keyed by question index (for example {"0": "Use Postgres"}). If the user wants to skip it, call answer_question with rejected set to true.
- "[relay] update from ...": a session finished a turn. Summarize briefly what changed and whether the work is done or still needs something.

## Destructive Actions
You run without permission prompts, so you are the safety check. Destructive tools are any delete tool, terminate_agent, clear_session, stop_background, and any remove or uninstall tool. Before calling one, say exactly what it will do (which session, card, or project, by name) and ask the user to confirm out loud. Only after an explicit yes, call it with confirmed set to true. Anything other than a clear yes means do not do it. Never set confirmed to true on your own initiative; the system refuses destructive calls without it.
"#;

#[derive(Default)]
struct Links {
    /// target session id → voice session id that sent it work.
    voice_for_target: HashMap<String, String>,
    /// Question event ids already relayed, so the immediate `ask_user`
    /// relay and the turn-end scan never announce the same question twice.
    relayed_questions: HashSet<String>,
}

static LINKS: LazyLock<Mutex<Links>> = LazyLock::new(Default::default);

fn links() -> std::sync::MutexGuard<'static, Links> {
    // A poisoned map only means a panic mid-insert; the data is still a
    // usable HashMap, so keep relaying rather than going dark.
    LINKS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Relay `target_session_id`'s questions and turn ends to `voice_session_id`
/// from now on (replacing any earlier voice link for that target).
pub fn link(target_session_id: &str, voice_session_id: &str) {
    if target_session_id.is_empty() || target_session_id == voice_session_id {
        return;
    }
    links()
        .voice_for_target
        .insert(target_session_id.to_string(), voice_session_id.to_string());
}

/// The voice session `target_session_id` relays to, if linked.
pub fn linked_voice(target_session_id: &str) -> Option<String> {
    links().voice_for_target.get(target_session_id).cloned()
}

fn unlink_voice(voice_session_id: &str) {
    links()
        .voice_for_target
        .retain(|_, v| v != voice_session_id);
}

/// True exactly once per question id.
fn claim_question(question_event_id: &str) -> bool {
    links()
        .relayed_questions
        .insert(question_event_id.to_string())
}

enum Signal {
    TurnEnded {
        session_id: String,
        outcome: String,
        reason: Option<String>,
    },
    Question {
        session_id: String,
        question_event_id: String,
    },
}

static SIGNALS: OnceLock<mpsc::UnboundedSender<Signal>> = OnceLock::new();

fn signal(sig: Signal) {
    if let Some(tx) = SIGNALS.get() {
        let _ = tx.send(sig);
    }
}

/// A session's agent turn ended (`outcome` is `"completed"` or `"crashed"`).
/// Cheap no-op unless the session is linked to a voice session.
pub fn note_turn_end(session_id: &str, outcome: &str, reason: Option<&str>) {
    if linked_voice(session_id).is_none() {
        return;
    }
    signal(Signal::TurnEnded {
        session_id: session_id.to_string(),
        outcome: outcome.to_string(),
        reason: reason.map(str::to_string),
    });
}

/// A session persisted a `question` event. Cheap no-op unless linked.
pub fn note_question(session_id: &str, question_event_id: &str) {
    if linked_voice(session_id).is_none() {
        return;
    }
    signal(Signal::Question {
        session_id: session_id.to_string(),
        question_event_id: question_event_id.to_string(),
    });
}

/// Start the relay listener. Called once at boot; later calls are no-ops.
pub fn spawn_listener(state: Arc<AppState>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    if SIGNALS.set(tx).is_err() {
        return;
    }
    tokio::spawn(async move {
        let dispatcher = crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
        while let Some(sig) = rx.recv().await {
            match sig {
                Signal::TurnEnded {
                    session_id,
                    outcome,
                    reason,
                } => {
                    relay_turn_end(
                        &state.db,
                        &state.broadcaster,
                        Some(&dispatcher),
                        &session_id,
                        &outcome,
                        reason.as_deref(),
                    )
                    .await;
                }
                Signal::Question {
                    session_id,
                    question_event_id,
                } => {
                    relay_question(
                        &state.db,
                        &state.broadcaster,
                        Some(&dispatcher),
                        &session_id,
                        &question_event_id,
                    )
                    .await;
                }
            }
        }
    });
}

/// Relay one freshly asked question from `target_session_id`.
pub async fn relay_question(
    db: &Db,
    broadcaster: &Broadcaster,
    dispatcher: Option<&dyn ExpertDispatcher>,
    target_session_id: &str,
    question_event_id: &str,
) {
    let Some(voice_id) = linked_voice(target_session_id) else {
        return;
    };
    let Ok(Some(target)) = db.get_session(target_session_id).await else {
        return;
    };
    let Ok(Some(event)) = db.get_event(question_event_id).await else {
        return;
    };
    if event.kind != "question" || event.session_id != target_session_id {
        return;
    }
    relay_question_event(db, broadcaster, dispatcher, &voice_id, &target, &event).await;
}

/// A linked target's turn ended: relay any not-yet-relayed pending
/// questions, or — when nothing is pending — an update carrying its reply.
pub async fn relay_turn_end(
    db: &Db,
    broadcaster: &Broadcaster,
    dispatcher: Option<&dyn ExpertDispatcher>,
    target_session_id: &str,
    outcome: &str,
    reason: Option<&str>,
) {
    let Some(voice_id) = linked_voice(target_session_id) else {
        return;
    };
    let Ok(Some(target)) = db.get_session(target_session_id).await else {
        return;
    };
    let pending = match crate::service::questions::pending_question_events(db, target_session_id)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(session_id = %target_session_id, "voice relay: event scan failed: {e}");
            return;
        }
    };
    if !pending.is_empty() {
        // Waiting on the user: the question is the news, not the turn end.
        for q in &pending {
            relay_question_event(db, broadcaster, dispatcher, &voice_id, &target, q).await;
        }
        return;
    }
    let text = if outcome == "completed" {
        let reply = crate::subagent::last_reply_text(db, target_session_id).await;
        format_update(&target.name, &target.id, &reply)
    } else {
        format_stopped(&target.name, &target.id, reason)
    };
    deliver(db, broadcaster, dispatcher, &voice_id, &text).await;
}

async fn relay_question_event(
    db: &Db,
    broadcaster: &Broadcaster,
    dispatcher: Option<&dyn ExpertDispatcher>,
    voice_id: &str,
    target: &Session,
    event: &Event,
) {
    if !claim_question(&event.id) {
        return;
    }
    let data = serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default();
    let text = format_question(&target.name, &target.id, &event.id, &data);
    deliver(db, broadcaster, dispatcher, voice_id, &text).await;
}

async fn deliver(
    db: &Db,
    broadcaster: &Broadcaster,
    dispatcher: Option<&dyn ExpertDispatcher>,
    voice_id: &str,
    text: &str,
) {
    if !matches!(db.get_session(voice_id).await, Ok(Some(_))) {
        // The voice session was deleted: stop relaying into the void.
        unlink_voice(voice_id);
        return;
    }
    if let Err(e) = crate::service::session_notify::notify_session(
        db,
        broadcaster,
        dispatcher,
        voice_id,
        text,
        serde_json::json!({ "source": "voice-relay" }),
    )
    .await
    {
        tracing::warn!(session_id = %voice_id, "voice relay append failed: {e}");
    }
}

/// Session titles are spoken context, not structure: keep them on one line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `[relay] question from "<title>" (session <id>, question <qid>): …` with
/// every question of the event, numbered by the index `answer_question`
/// keys its answers with.
pub fn format_question(
    title: &str,
    session_id: &str,
    question_id: &str,
    data: &serde_json::Value,
) -> String {
    let questions = data
        .get("questions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut parts = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        let text = one_line(q.get("question").and_then(|v| v.as_str()).unwrap_or(""));
        let options: Vec<&str> = q
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|o| o.as_str()).collect())
            .unwrap_or_default();
        let multi = q
            .get("multiSelect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut part = format!("[{i}] {text}");
        if !options.is_empty() {
            part.push_str(&format!(
                " (options{}: {})",
                if multi { ", pick any" } else { "" },
                options.join(" | ")
            ));
        }
        parts.push(part);
    }
    let body = if parts.is_empty() {
        "(no question text)".to_string()
    } else {
        parts.join(" ")
    };
    format!(
        "{RELAY_PREFIX}question from \"{}\" (session {session_id}, question {question_id}): {body}",
        one_line(title)
    )
}

/// `[relay] update from "<title>" (session <id>): <reply>` — the reply
/// trimmed to its last [`UPDATE_CHAR_CAP`] chars (the conclusion is at the
/// end).
pub fn format_update(title: &str, session_id: &str, reply: &str) -> String {
    let reply = reply.trim();
    let count = reply.chars().count();
    let body = if reply.is_empty() {
        "(finished its turn without a text reply)".to_string()
    } else if count > UPDATE_CHAR_CAP {
        let tail: String = reply.chars().skip(count - UPDATE_CHAR_CAP).collect();
        format!("\u{2026}{tail}")
    } else {
        reply.to_string()
    };
    format!(
        "{RELAY_PREFIX}update from \"{}\" (session {session_id}): {body}",
        one_line(title)
    )
}

/// Update for a turn that crashed or was stopped.
fn format_stopped(title: &str, session_id: &str, reason: Option<&str>) -> String {
    format!(
        "{RELAY_PREFIX}update from \"{}\" (session {session_id}): the agent stopped before finishing{}",
        one_line(title),
        reason
            .map(|r| format!(" ({})", one_line(r)))
            .unwrap_or_default()
    )
}

/// Record a voice → target link when a voice session hands another session
/// work through a session tool. Called after every successful MCP tool
/// call; returns immediately for any other tool.
pub async fn link_from_tool_call(
    db: &Db,
    caller_session_id: &str,
    tool_name: &str,
    target_from_args: Option<&str>,
    result: &serde_json::Value,
) {
    let target = match tool_name {
        "send_message" | "send_image" => target_from_args.map(str::to_string),
        "create_session" => result
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => return,
    };
    let Some(target) = target else {
        return;
    };
    // A cross-folder send parked on approval did not reach the target yet;
    // the re-call after the user approves links it.
    if result.get("status").and_then(|v| v.as_str()) == Some("awaiting_approval") {
        return;
    }
    let is_voice = |s: &Session| s.expert_kind.as_deref() == Some(VOICE_EXPERT_KIND);
    let caller_is_voice =
        matches!(db.get_session(caller_session_id).await, Ok(Some(s)) if is_voice(&s));
    // Never link one voice session to another: each one's turn end would
    // relay into the other forever.
    let target_is_voice = matches!(db.get_session(&target).await, Ok(Some(s)) if is_voice(&s));
    if caller_is_voice && !target_is_voice {
        link(&target, caller_session_id);
    }
}

/// Tools that destroy or discard work: deletes, removals, uninstalls,
/// killing an agent, wiping a session's context, stopping a background task.
/// The voice session runs with permissions skipped, so these are the calls
/// it must confirm out loud first (see [`require_voice_confirmation`]).
pub fn is_destructive_tool(name: &str) -> bool {
    name.starts_with("delete_")
        || name.starts_with("remove_")
        || name.ends_with("_remove")
        || name.starts_with("uninstall")
        || matches!(
            name,
            "terminate_agent" | "clear_session" | "stop_background"
        )
}

/// Argument a voice session must pass as `true` on a destructive tool, after
/// the user said yes out loud. Advertised in the voice session's tool
/// schemas by `ToolGate::input_schema`.
pub const CONFIRMED_ARG: &str = "confirmed";

/// Hard gate for the voice session's destructive calls: refuses unless
/// `args.confirmed == true`, and strips `confirmed` before dispatch so no
/// handler sees an argument it doesn't declare. A no-op for every other
/// session and every non-destructive tool.
pub async fn require_voice_confirmation(
    db: &Db,
    caller_session_id: &str,
    tool_name: &str,
    args: &mut serde_json::Value,
) -> anyhow::Result<()> {
    let destructive = is_destructive_tool(tool_name);
    if !destructive && args.get(CONFIRMED_ARG).is_none() {
        return Ok(());
    }
    let caller_is_voice = matches!(
        db.get_session(caller_session_id).await,
        Ok(Some(s)) if s.expert_kind.as_deref() == Some(VOICE_EXPERT_KIND)
    );
    if !caller_is_voice {
        return Ok(());
    }
    let confirmed = args
        .as_object_mut()
        .and_then(|o| o.remove(CONFIRMED_ARG))
        .and_then(|v| v.as_bool())
        == Some(true);
    if destructive && !confirmed {
        anyhow::bail!(
            "'{tool_name}' is destructive: tell the user out loud exactly what it will do, \
             ask them to confirm, and only after an explicit yes call it again with \
             \"{CONFIRMED_ARG}\": true"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    async fn seed(db: &Db, id: &str, name: &str, kind: Option<&str>) {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: id.into(),
            name: name.into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            is_expert: kind.is_some(),
            expert_kind: kind.map(str::to_string),
            user_id: Some("u1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    async fn relay_texts(db: &Db, voice_id: &str) -> Vec<String> {
        db.list_events_by_session(voice_id, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "user")
            .filter_map(|e| {
                serde_json::from_str::<serde_json::Value>(&e.data)
                    .ok()?
                    .get("text")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    #[tokio::test]
    async fn relays_question_then_update_into_voice_session() {
        let db = Db::in_memory().unwrap();
        let bc = Broadcaster::new();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        let voice = uuid::Uuid::new_v4().to_string();
        let target = uuid::Uuid::new_v4().to_string();
        seed(&db, &voice, VOICE_SESSION_TITLE, Some(VOICE_EXPERT_KIND)).await;
        seed(&db, &target, "stashify dev", None).await;

        // Only a voice caller links.
        link_from_tool_call(
            &db,
            &target,
            "send_message",
            Some(&voice),
            &serde_json::json!({"ok": true}),
        )
        .await;
        assert!(linked_voice(&voice).is_none());
        link_from_tool_call(
            &db,
            &voice,
            "send_message",
            Some(&target),
            &serde_json::json!({"ok": true}),
        )
        .await;
        assert_eq!(linked_voice(&target).as_deref(), Some(voice.as_str()));

        // Question: relayed once, with id + options; no update while pending.
        let q = db
            .append_event(
                &target,
                "question",
                serde_json::json!({"questions": [
                    {"question": "Which DB?", "options": ["Postgres", "SQLite"], "multiSelect": false}
                ]}),
            )
            .await
            .unwrap();
        relay_question(&db, &bc, None, &target, &q.id).await;
        relay_turn_end(&db, &bc, None, &target, "completed", None).await;
        let texts = relay_texts(&db, &voice).await;
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert_eq!(
            texts[0],
            format!(
                "[relay] question from \"stashify dev\" (session {target}, question {}): [0] Which DB? (options: Postgres | SQLite)",
                q.id
            )
        );

        // Answered; the next turn's reply comes back as an update.
        db.append_event(
            &target,
            "question-resolved",
            serde_json::json!({"question_id": q.id, "answers": {"0": "Postgres"}}),
        )
        .await
        .unwrap();
        db.append_event(&target, "user", serde_json::json!({"text": "Postgres"}))
            .await
            .unwrap();
        db.append_event(
            &target,
            "agent-text",
            serde_json::json!({"text": "Done, using Postgres."}),
        )
        .await
        .unwrap();
        relay_turn_end(&db, &bc, None, &target, "completed", None).await;
        let texts = relay_texts(&db, &voice).await;
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert_eq!(
            texts[1],
            format!(
                "[relay] update from \"stashify dev\" (session {target}): Done, using Postgres."
            )
        );
    }

    #[test]
    fn update_keeps_the_tail_of_a_long_reply() {
        let long = format!("{}END", "x".repeat(3000));
        let text = format_update("t", "s", &long);
        assert!(text.starts_with("[relay] update from \"t\" (session s): \u{2026}"));
        assert!(text.ends_with("END"));
        assert!(text.chars().count() < 1600);
    }

    #[tokio::test]
    async fn voice_destructive_calls_need_spoken_confirmation() {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        seed(&db, "voice", "Voice", Some(VOICE_EXPERT_KIND)).await;
        seed(&db, "chat", "Chat", None).await;

        // Voice + destructive + unconfirmed: refused, telling it how to proceed.
        let mut args = serde_json::json!({"session_id": "chat"});
        let err = require_voice_confirmation(&db, "voice", "terminate_agent", &mut args)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("\"confirmed\": true"), "{err}");

        // Confirmed: allowed, and the flag is stripped before dispatch.
        let mut args = serde_json::json!({"project_id": "p", "confirmed": true});
        require_voice_confirmation(&db, "voice", "delete_project", &mut args)
            .await
            .unwrap();
        assert_eq!(args, serde_json::json!({"project_id": "p"}));

        // Non-destructive voice calls and every non-voice caller: untouched.
        let mut args = serde_json::json!({"session_id": "chat", "text": "hi"});
        require_voice_confirmation(&db, "voice", "send_message", &mut args)
            .await
            .unwrap();
        require_voice_confirmation(&db, "chat", "delete_card", &mut args)
            .await
            .unwrap();

        // The voice session is told about the argument in its tool schema.
        let voice_row = db.get_session("voice").await.unwrap().unwrap();
        let gate = crate::service::mcp_server::ToolGate::from_session(&voice_row);
        let schema = gate.input_schema(
            "clear_session",
            &serde_json::json!({"type": "object", "properties": {}, "required": ["session_id"]}),
        );
        assert_eq!(schema["properties"]["confirmed"]["type"], "boolean");
        assert_eq!(
            schema["required"],
            serde_json::json!(["session_id", "confirmed"])
        );
    }
}
