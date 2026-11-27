//! Named voice assistants: turning "which assistant" into its voice session.
//!
//! An assistant is a row of `assistants` (`db::crud::assistants`) that owns
//! one voice session. Everything that used to ask for "the voice session"
//! asks here instead, and says which assistant it means: a specific one, or
//! the default (who answers when no name is said).

use crate::db::Db;
use crate::db::models::{Assistant, Session};

// Proof token: bearer names an assistant that exists AND whose voice session
// exists. The only constructors ([`resolve`], [`for_session`], [`all`]) look
// both up, so code holding one can't be acting on "whichever voice session
// happens to be oldest". See `routes::voice::voice_ref` for an example.
#[derive(Debug, Clone)]
pub struct AssistantRef {
    assistant: Assistant,
    session: Session,
}

impl AssistantRef {
    pub fn assistant(&self) -> &Assistant {
        &self.assistant
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_id(&self) -> &str {
        &self.session.id
    }
}

/// `assistant` with its session, or `None` while it has none (never
/// opened, or the session it pointed at is gone).
async fn with_session(db: &Db, assistant: Assistant) -> anyhow::Result<Option<AssistantRef>> {
    let Some(session_id) = assistant.session_id.as_deref() else {
        return Ok(None);
    };
    Ok(db
        .get_session(session_id)
        .await?
        .map(|session| AssistantRef { assistant, session }))
}

/// The assistant `id`, or the default one for `None`. `Ok(None)` when there
/// is no such assistant or it has no session yet.
pub async fn resolve(db: &Db, id: Option<&str>) -> anyhow::Result<Option<AssistantRef>> {
    let assistant = match id.map(str::trim).filter(|i| !i.is_empty()) {
        Some(id) => match db.get_assistant(id).await? {
            Some(a) => a,
            None => return Ok(None),
        },
        None => db.default_assistant().await?,
    };
    with_session(db, assistant).await
}

/// The assistant whose conversation is `session_id`, if it is one.
pub async fn for_session(db: &Db, session_id: &str) -> anyhow::Result<Option<AssistantRef>> {
    match db.assistant_for_session(session_id).await? {
        Some(assistant) => with_session(db, assistant).await,
        None => Ok(None),
    }
}

/// Every assistant that has a session, in display order.
pub async fn all(db: &Db) -> anyhow::Result<Vec<AssistantRef>> {
    let mut out = Vec::new();
    for assistant in db.list_assistants().await? {
        if let Some(r) = with_session(db, assistant).await? {
            out.push(r);
        }
    }
    Ok(out)
}

/// The "Who You Are" prompt section for `me`: its name, its job, and the
/// other assistants it can hand work to. Empty for an install that still
/// has only the unnamed default Assistant, so its prompt is unchanged.
pub fn identity_section(me: &Assistant, roster: &[Assistant]) -> String {
    use crate::db::crud::assistants::DEFAULT_ASSISTANT_NAME;
    let others: Vec<&Assistant> = roster.iter().filter(|a| a.id != me.id).collect();
    if others.is_empty() && me.job.is_empty() && me.name == DEFAULT_ASSISTANT_NAME {
        return String::new();
    }
    let mut out = format!(
        "## Who You Are\n\nYour name is {}. The user calls you by it.\n",
        me.name
    );
    if !me.job.is_empty() {
        out.push_str(&format!("\n### Your Job\n\n{}\n", me.job));
    }
    if !others.is_empty() {
        out.push_str(
            "\n### The Other Assistants\n\nThe user has other assistants, each with its own conversation and job. You are not them: answer only as yourself.\n\n",
        );
        for a in others {
            let job = a.job.lines().next().unwrap_or("").trim();
            if job.is_empty() {
                out.push_str(&format!("- {}\n", a.name));
            } else {
                out.push_str(&format!("- {}: {}\n", a.name, job));
            }
        }
        out.push_str(
            "\nWhen the user asks you to pass something to one of them, or asks for work that is clearly another assistant's job, give it the task with the assistant_handoff tool and tell the user you passed it on. A message that starts with \"[relay] hand-off from assistant\" is a task another assistant passed to you at the user's request: do it, then tell the user the result.\n",
        );
    }
    out
}

/// A ready-made assistant the user can add in one step: a suggested name
/// (they can change it) and a job written for it.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Template {
    pub id: &'static str,
    /// What the gallery calls it.
    pub title: &'static str,
    /// One line under the title.
    pub summary: &'static str,
    /// The name the new assistant starts with.
    pub name: &'static str,
    pub job: &'static str,
    pub icon: &'static str,
    pub color: &'static str,
}

/// The gallery shown by "Add assistant". Every job only describes work the
/// voice tool set can already do; a template grants nothing extra.
pub const TEMPLATES: &[Template] = &[
    Template {
        id: "coder",
        title: "Coder",
        summary: "Starts and steers coding sessions, and reports back when they finish.",
        name: "Dev",
        job: "You look after the user's coding work. When they describe a change, start or message a coding session in the right folder, keep the request specific, and tell them briefly when it finishes or needs a decision. You do not write code yourself in this conversation.",
        icon: "code",
        color: "#3b82f6",
    },
    Template {
        id: "researcher",
        title: "Researcher",
        summary: "Looks things up on the web and gives short, sourced answers.",
        name: "Scout",
        job: "You research questions for the user. Search the web, read the sources, and answer in a few spoken sentences with where it came from. Say plainly when sources disagree or you could not confirm something.",
        icon: "search",
        color: "#10b981",
    },
    Template {
        id: "project-manager",
        title: "Project Manager",
        summary: "Keeps track of projects and cards, and what is blocked.",
        name: "Planner",
        job: "You keep the user's projects moving. Know which cards are in progress, blocked or waiting for review, create and update cards when asked, and raise anything stuck without being asked twice.",
        icon: "kanban",
        color: "#f59e0b",
    },
    Template {
        id: "ops",
        title: "Ops",
        summary: "Watches servers and running work, and says when something is wrong.",
        name: "Ops",
        job: "You watch the user's machines and running sessions. Check status when asked, report failures and anything unusual in one or two sentences, and never restart, stop or delete anything without the user saying so.",
        icon: "server",
        color: "#ef4444",
    },
    Template {
        id: "briefing",
        title: "Daily Briefing",
        summary: "Sums up what happened and what needs you today.",
        name: "Brief",
        job: "You give the user short briefings: what finished since they last asked, what is waiting on them, and what is scheduled next. Lead with what needs a decision. Keep it under a minute unless they ask for more.",
        icon: "sun",
        color: "#8b5cf6",
    },
];
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::crud::assistants::NewAssistant;
    use crate::db::models::{NewFolder, NewSession};

    async fn session(db: &Db, id: &str) {
        db.create_session(NewSession {
            id: id.into(),
            name: id.into(),
            folder_id: "f".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            last_activity: "2026-01-01T00:00:00Z".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn resolve_needs_both_the_assistant_and_its_session() {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f".into(),
            name: "f".into(),
            path: ".".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        // The default exists but was never opened.
        assert!(resolve(&db, None).await.unwrap().is_none());
        assert!(all(&db).await.unwrap().is_empty());

        let default = db.default_assistant().await.unwrap();
        session(&db, "s-default").await;
        db.set_assistant_session(&default.id, Some("s-default"))
            .await
            .unwrap();
        let nova = db
            .create_assistant(NewAssistant {
                name: "Nova".into(),
                job: "Reviews pull requests.\nSecond line.".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        session(&db, "s-nova").await;
        db.set_assistant_session(&nova.id, Some("s-nova"))
            .await
            .unwrap();

        assert_eq!(
            resolve(&db, None).await.unwrap().unwrap().session_id(),
            "s-default"
        );
        assert_eq!(
            resolve(&db, Some(&nova.id))
                .await
                .unwrap()
                .unwrap()
                .session_id(),
            "s-nova"
        );
        assert!(resolve(&db, Some("nope")).await.unwrap().is_none());
        assert_eq!(
            for_session(&db, "s-nova")
                .await
                .unwrap()
                .unwrap()
                .assistant()
                .name,
            "Nova"
        );
        assert!(for_session(&db, "other").await.unwrap().is_none());
        assert_eq!(all(&db).await.unwrap().len(), 2);

        // A pointer at a session that no longer exists is "no session".
        db.set_assistant_session(&nova.id, Some("gone"))
            .await
            .unwrap();
        assert!(resolve(&db, Some(&nova.id)).await.unwrap().is_none());

        // Identity: nothing for a lone default; name, job and roster otherwise.
        let roster = db.list_assistants().await.unwrap();
        assert_eq!(identity_section(&default, &[default.clone()]), "");
        let mine = identity_section(&default, &roster);
        assert!(mine.contains("Your name is Assistant."));
        assert!(mine.contains("- Nova: Reviews pull requests.\n"));
        assert!(!mine.contains("Second line"));
        let novas = identity_section(&nova, &roster);
        assert!(novas.contains("### Your Job\n\nReviews pull requests.\nSecond line."));
        assert!(novas.contains("- Assistant\n"));
    }
}
