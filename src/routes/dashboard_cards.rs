//! Card-centric dashboard widget reads (auth required, all `GET`, optional
//! `?project_id=` scope):
//!
//! - `/api/dashboard/attention` — what needs a human: unresolved worker
//!   questions, proposed plans, blocked cards, cards whose last worker run
//!   crashed, and unmerged worktrees. Newest first, max 50.
//! - `/api/dashboard/review-queue` — cards in review, plus the 10 most
//!   recently reviewed.
//! - `/api/dashboard/review-quality?days=N` — per-UTC-day review verdicts
//!   and worker crashes over the last N days (1..=180, default 30).
//!
//! Projects and cards are shared across users, like `/api/projects`; only
//! chat plans are filtered to their owner (admins see all). Crashes and
//! retries come from `card_sessions` run outcomes — see
//! [`Db::dashboard_review_quality`](crate::db::Db::dashboard_review_quality).

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    routing::get,
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::crud::dashboard_cards::ATTENTION_LIMIT;
use crate::service::questions::ASK_USER_BLOCK_REASON;
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

/// Longest attention `title`, in characters.
const MAX_TITLE_CHARS: usize = 200;
/// `recent` reviews returned by the review queue.
const RECENT_REVIEWS: i64 = 10;
const DEFAULT_QUALITY_DAYS: i64 = 30;
const MAX_QUALITY_DAYS: i64 = 180;

#[derive(Deserialize)]
struct ScopeQuery {
    #[serde(default)]
    project_id: Option<String>,
}

#[derive(Deserialize)]
struct QualityQuery {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    days: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/dashboard/attention", get(attention))
        .route("/api/dashboard/review-queue", get(review_queue))
        .route("/api/dashboard/review-quality", get(review_quality))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn internal(e: anyhow::Error) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// `?project_id=` with blank treated as unscoped.
fn scope(project_id: Option<String>) -> Option<String> {
    project_id.filter(|p| !p.trim().is_empty())
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Sort key for an RFC3339 timestamp; unparseable sorts oldest.
fn instant(at: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(at)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// The first question's text from a `question` event's data.
fn question_title(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|d| {
            d.get("questions")?
                .get(0)?
                .get("question")?
                .as_str()
                .map(str::to_string)
        })
        .filter(|q| !q.trim().is_empty())
        .map(|q| truncate(q.trim(), MAX_TITLE_CHARS))
        .unwrap_or_else(|| "Worker question".into())
}

struct Item {
    at: DateTime<Utc>,
    json: serde_json::Value,
}

#[allow(clippy::too_many_arguments)]
fn item(
    kind: &str,
    project_id: Option<&str>,
    project_name: Option<&str>,
    card_id: Option<&str>,
    card_title: Option<&str>,
    session_id: Option<&str>,
    title: &str,
    detail: Option<String>,
    at: DateTime<Utc>,
) -> Item {
    Item {
        at,
        json: serde_json::json!({
            "kind": kind,
            "project_id": project_id,
            "project_name": project_name,
            "card_id": card_id,
            "card_title": card_title,
            "session_id": session_id,
            "title": title,
            "detail": detail,
            "at": at.to_rfc3339(),
        }),
    }
}

/// GET /api/dashboard/attention — `{items: [{kind, project_id,
/// project_name, card_id, card_title, session_id, title, detail, at}]}`.
/// A card parked on a worker question shows only as the question; a
/// blocked card never also shows as `worker_error`.
async fn attention(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope(q.project_id);
    let db = &state.db;
    let questions = db
        .dashboard_pending_questions(scope.clone())
        .await
        .map_err(internal)?;
    let plans = db
        .dashboard_proposed_plans(scope.clone(), &user.user_id, user.is_admin())
        .await
        .map_err(internal)?;
    let cards = db.dashboard_card_attention(scope).await.map_err(internal)?;

    let mut items = Vec::new();
    let mut asking: HashSet<&str> = HashSet::new();
    for r in &questions {
        if let Some(c) = &r.card_id {
            asking.insert(c);
        }
        let at = DateTime::<Utc>::from_timestamp_millis(r.ts).unwrap_or(DateTime::<Utc>::MIN_UTC);
        items.push(item(
            "question",
            r.project_id.as_deref(),
            r.project_name.as_deref(),
            r.card_id.as_deref(),
            r.card_title.as_deref(),
            Some(&r.session_id),
            &question_title(&r.data),
            None,
            at,
        ));
    }
    for p in &plans {
        items.push(item(
            "plan",
            p.project_id.as_deref(),
            p.project_name.as_deref(),
            p.card_id.as_deref(),
            p.card_title.as_deref(),
            Some(&p.session_id),
            &truncate(&p.title, MAX_TITLE_CHARS),
            None,
            instant(&p.updated_at),
        ));
    }
    for c in &cards {
        let open = c.step != "done" && c.step != "wont_do";
        let card_session = c
            .worker_session_id
            .as_deref()
            .or(c.last_worker_session_id.as_deref());
        let base = |kind, session, title, detail, at| {
            item(
                kind,
                Some(&c.project_id),
                Some(&c.project_name),
                Some(&c.id),
                Some(&c.title),
                session,
                title,
                detail,
                at,
            )
        };
        if open && c.blocked {
            let parked_on_question = c.block_reason.as_deref() == Some(ASK_USER_BLOCK_REASON)
                && asking.contains(c.id.as_str());
            if !parked_on_question {
                items.push(base(
                    "blocked",
                    card_session,
                    "Card blocked",
                    c.block_reason.clone(),
                    instant(&c.updated_at),
                ));
            }
        } else if open
            && c.last_outcome.as_deref() == Some("crashed")
            && c.worker_session_id.is_none()
        {
            items.push(base(
                "worker_error",
                c.last_session_id.as_deref().or(card_session),
                "Worker crashed",
                c.last_summary.clone(),
                instant(c.last_ended_at.as_deref().unwrap_or(&c.updated_at)),
            ));
        }
        if let Some(reason) = &c.worktree_unmerged_reason {
            let detail = match c.worktree_unmerged_detail.as_deref() {
                Some(d) if !d.is_empty() => format!("{reason}: {d}"),
                _ => reason.clone(),
            };
            items.push(base(
                "unmerged",
                card_session,
                "Worktree not merged",
                Some(detail),
                instant(&c.updated_at),
            ));
        }
    }
    items.sort_by_key(|i| std::cmp::Reverse(i.at));
    items.truncate(ATTENTION_LIMIT as usize);
    Ok(Json(serde_json::json!({
        "items": items.into_iter().map(|i| i.json).collect::<Vec<_>>(),
    })))
}

/// GET /api/dashboard/review-queue — `{in_review: [{card_id, title,
/// project_id, project_name, worker_session_id, session_running,
/// updated_at}], recent: [{card_id, title, project_id, project_name,
/// verdict, reviewed_at, reviewer_model, summary}]}`.
async fn review_queue(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope(q.project_id);
    let in_review = state
        .db
        .dashboard_in_review(scope.clone(), crate::workflow::REVIEW_STEP)
        .await
        .map_err(internal)?;
    let recent = state
        .db
        .dashboard_recent_reviews(scope, RECENT_REVIEWS)
        .await
        .map_err(internal)?;
    let mut queue = Vec::with_capacity(in_review.len());
    for r in in_review {
        let running = match &r.worker_session_id {
            Some(sid) => state.session_manager.is_running(sid).await,
            None => false,
        };
        queue.push(serde_json::json!({
            "card_id": r.card_id,
            "title": r.title,
            "project_id": r.project_id,
            "project_name": r.project_name,
            "worker_session_id": r.worker_session_id,
            "session_running": running,
            "updated_at": r.updated_at,
        }));
    }
    let recent: Vec<_> = recent
        .into_iter()
        .map(|r| {
            let verdict = if r.verdict == "changes_requested" {
                "changes_requested"
            } else {
                "pass"
            };
            serde_json::json!({
                "card_id": r.card_id,
                "title": r.title,
                "project_id": r.project_id,
                "project_name": r.project_name,
                "verdict": verdict,
                "reviewed_at": r.reviewed_at,
                "reviewer_model": r.reviewer_model,
                "summary": r.summary,
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "in_review": queue, "recent": recent }),
    ))
}

/// GET /api/dashboard/review-quality — `{days: [{date, pass,
/// changes_requested, crashes}], totals: {pass, changes_requested,
/// crashes, retries}, by_project: [{project_id, project_name, pass,
/// changes_requested}]}`. Every UTC day of the window is present, oldest
/// first, today last.
async fn review_quality(
    State(state): State<Arc<AppState>>,
    Query(q): Query<QualityQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let days = match q.days.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        None => DEFAULT_QUALITY_DAYS,
        Some(d) => d
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=MAX_QUALITY_DAYS).contains(n))
            .ok_or_else(|| {
                err(
                    StatusCode::BAD_REQUEST,
                    format!("days must be an integer in 1..={MAX_QUALITY_DAYS}"),
                )
            })?,
    };
    let today = Utc::now().date_naive();
    let start: NaiveDate = today - Duration::days(days - 1);
    let quality = state
        .db
        .dashboard_review_quality(scope(q.project_id), &start.format("%Y-%m-%d").to_string())
        .await
        .map_err(internal)?;
    let by_day: HashMap<&str, _> = quality.days.iter().map(|d| (d.date.as_str(), d)).collect();
    let (mut pass, mut changes, mut crashes) = (0i64, 0i64, 0i64);
    let series: Vec<_> = start
        .iter_days()
        .take(days as usize)
        .map(|day| {
            let date = day.format("%Y-%m-%d").to_string();
            let (p, c, x) = by_day
                .get(date.as_str())
                .map(|d| (d.pass, d.changes_requested, d.crashes))
                .unwrap_or_default();
            pass += p;
            changes += c;
            crashes += x;
            serde_json::json!({
                "date": date,
                "pass": p,
                "changes_requested": c,
                "crashes": x,
            })
        })
        .collect();
    let by_project: Vec<_> = quality
        .by_project
        .iter()
        .map(|p| {
            serde_json::json!({
                "project_id": p.project_id,
                "project_name": p.project_name,
                "pass": p.pass,
                "changes_requested": p.changes_requested,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "days": series,
        "totals": {
            "pass": pass,
            "changes_requested": changes,
            "crashes": crashes,
            "retries": quality.retries,
        },
        "by_project": by_project,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::crud::CardRunStart;
    use crate::db::models::{NewCard, NewFolder, NewProject, NewSession, NewUser, UpdateCard};

    fn user(id: &str, role: &str) -> AuthUser {
        AuthUser {
            user_id: id.into(),
            role: role.into(),
            session_id: "s".into(),
        }
    }

    fn unscoped() -> Query<ScopeQuery> {
        Query(ScopeQuery { project_id: None })
    }

    async fn seed(db: &Db) {
        let ts = Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        for u in ["u1", "u2"] {
            db.create_user(NewUser {
                id: u.into(),
                username: u.into(),
                email: None,
                password_hash: "h".into(),
                role: "user".into(),
                created_at: ts.clone(),
                updated_at: ts.clone(),
            })
            .await
            .unwrap();
        }
        db.create_project(NewProject {
            id: "p1".into(),
            name: "Alpha".into(),
            context: "".into(),
            folder_id: "f".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();
    }

    async fn card(db: &Db, id: &str, step: &str, update: UpdateCard) {
        let ts = Utc::now().to_rfc3339();
        db.create_card(NewCard {
            id: id.into(),
            project_id: "p1".into(),
            title: format!("Card {id}"),
            description: "".into(),
            step: step.into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts,
            system_prompt_name: None,
        })
        .await
        .unwrap();
        let update = UpdateCard {
            updated_at: Some(Utc::now().to_rfc3339()),
            ..update
        };
        db.update_card(id, update).await.unwrap();
    }

    async fn session(db: &Db, id: &str, card: Option<&str>, owner: Option<&str>) {
        let ts = Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: id.into(),
            name: id.into(),
            folder_id: "f".into(),
            is_worker: card.is_some(),
            project_id: card.map(|_| "p1".into()),
            card_id: card.map(str::to_string),
            user_id: owner.map(str::to_string),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
    }

    async fn run(db: &Db, card: &str, sid: &str, role: &str, outcome: &str, at: &str) {
        db.open_card_run(
            card,
            sid,
            CardRunStart {
                step: "in_progress".into(),
                role: role.into(),
                model: Some("claude:opus".into()),
            },
            at,
        )
        .await
        .unwrap();
        db.close_card_run(card, sid, outcome, Some(format!("{outcome} summary")))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn attention_lists_questions_plans_blocked_errors_and_unmerged() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        let db = &state.db;
        seed(db).await;
        // Parked on a question: shows only as the question.
        card(
            db,
            "cq",
            "in_progress",
            UpdateCard {
                blocked: Some(true),
                block_reason: Some(Some(ASK_USER_BLOCK_REASON.into())),
                ..Default::default()
            },
        )
        .await;
        session(db, "sq", Some("cq"), None).await;
        let q = serde_json::json!({ "questions": [{ "question": "Which DB?" }] });
        db.append_event("sq", "question", q.clone()).await.unwrap();
        let answered = db.append_event("sq", "question", q).await.unwrap();
        db.append_event(
            "sq",
            "question-resolved",
            serde_json::json!({ "questionId": answered.id }),
        )
        .await
        .unwrap();
        card(
            db,
            "cb",
            "todo",
            UpdateCard {
                blocked: Some(true),
                block_reason: Some(Some("needs creds".into())),
                ..Default::default()
            },
        )
        .await;
        card(db, "ce", "in_progress", UpdateCard::default()).await;
        session(db, "se", Some("ce"), None).await;
        run(db, "ce", "se", "work", "crashed", &Utc::now().to_rfc3339()).await;
        card(
            db,
            "cu",
            "done",
            UpdateCard {
                worktree_unmerged_reason: Some(Some("conflict".into())),
                worktree_unmerged_detail: Some(Some("src/a.rs".into())),
                ..Default::default()
            },
        )
        .await;
        card(db, "cok", "todo", UpdateCard::default()).await;
        db.upsert_plan("sq", Some("cq"), Some("p1"), "Board plan", "md")
            .await
            .unwrap();
        // A chat plan is its owner's (and admins') only.
        session(db, "chat", None, Some("u2")).await;
        db.upsert_plan("chat", None, None, "Chat plan", "md")
            .await
            .unwrap();

        let get = |u: AuthUser, scope: Option<&str>| {
            let state = state.clone();
            let scope = scope.map(str::to_string);
            async move {
                attention(
                    State(state),
                    Extension(u),
                    Query(ScopeQuery { project_id: scope }),
                )
                .await
                .unwrap()
                .0
            }
        };
        let body = get(user("u1", "user"), None).await;
        let items = body["items"].as_array().unwrap();
        let mut kinds: Vec<(&str, &str)> = items
            .iter()
            .map(|i| (i["kind"].as_str().unwrap(), i["title"].as_str().unwrap()))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [
                ("blocked", "Card blocked"),
                ("plan", "Board plan"),
                ("question", "Which DB?"),
                ("unmerged", "Worktree not merged"),
                ("worker_error", "Worker crashed"),
            ]
        );
        let by = |k: &str| items.iter().find(|i| i["kind"] == k).unwrap();
        assert_eq!(by("question")["card_id"], "cq");
        assert_eq!(by("question")["card_title"], "Card cq");
        assert_eq!(by("question")["project_name"], "Alpha");
        assert_eq!(by("question")["session_id"], "sq");
        assert_eq!(by("blocked")["detail"], "needs creds");
        assert_eq!(by("worker_error")["session_id"], "se");
        assert_eq!(by("worker_error")["detail"], "crashed summary");
        assert_eq!(by("unmerged")["detail"], "conflict: src/a.rs");
        let ats: Vec<DateTime<Utc>> = items
            .iter()
            .map(|i| instant(i["at"].as_str().unwrap()))
            .collect();
        assert!(ats.windows(2).all(|w| w[0] >= w[1]), "newest first");

        let admin = get(user("u1", "admin"), None).await;
        assert_eq!(admin["items"].as_array().unwrap().len(), 6);
        let owner = get(user("u2", "user"), None).await;
        assert!(
            owner["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["title"] == "Chat plan")
        );
        let other = get(user("u2", "user"), Some("nope")).await;
        assert!(other["items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn review_queue_lists_in_review_and_recent_verdicts() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        let db = &state.db;
        seed(db).await;
        session(db, "sr", None, None).await;
        card(
            db,
            "cr",
            crate::workflow::REVIEW_STEP,
            UpdateCard {
                worker_session_id: Some(Some("sr".into())),
                ..Default::default()
            },
        )
        .await;
        for (id, verdict, at) in [
            ("c1", "pass", "2026-10-01T10:00:00+00:00"),
            ("c2", "changes_requested", "2026-10-02T10:00:00+00:00"),
        ] {
            card(
                db,
                id,
                "done",
                UpdateCard {
                    review_verdict: Some(Some(verdict.into())),
                    reviewed_at: Some(Some(at.into())),
                    review_summary: Some(Some(format!("{id} review"))),
                    ..Default::default()
                },
            )
            .await;
        }
        run(
            db,
            "c2",
            "rev",
            "review",
            "changes_requested",
            "2026-10-02T09:00:00+00:00",
        )
        .await;

        let body = review_queue(State(state.clone()), unscoped())
            .await
            .unwrap()
            .0;
        let queue = body["in_review"].as_array().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0]["card_id"], "cr");
        assert_eq!(queue[0]["project_name"], "Alpha");
        assert_eq!(queue[0]["worker_session_id"], "sr");
        assert_eq!(queue[0]["session_running"], false);
        let recent = body["recent"].as_array().unwrap();
        let ids: Vec<&str> = recent
            .iter()
            .map(|r| r["card_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["c2", "c1"], "newest review first");
        assert_eq!(recent[0]["verdict"], "changes_requested");
        assert_eq!(recent[0]["reviewer_model"], "claude:opus");
        assert_eq!(recent[0]["summary"], "c2 review");
        assert_eq!(recent[1]["verdict"], "pass");
        assert_eq!(recent[1]["reviewer_model"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn review_quality_fills_every_day_and_counts_crashes_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        let db = &state.db;
        seed(db).await;
        card(db, "c1", "done", UpdateCard::default()).await;
        let at = |s: i64| (Utc::now() - Duration::seconds(60 - s)).to_rfc3339();
        run(db, "c1", "w1", "work", "crashed", &at(1)).await;
        run(db, "c1", "w2", "work", "advanced", &at(2)).await;
        run(db, "c1", "r1", "review", "changes_requested", &at(3)).await;
        run(db, "c1", "r2", "review", "reviewed", &at(4)).await;

        let get = |days: Option<&str>| {
            let state = state.clone();
            let days = days.map(str::to_string);
            async move {
                review_quality(
                    State(state),
                    Query(QualityQuery {
                        project_id: None,
                        days,
                    }),
                )
                .await
            }
        };
        let body = get(Some("7")).await.unwrap().0;
        let days = body["days"].as_array().unwrap();
        assert_eq!(days.len(), 7);
        let today = Utc::now().date_naive().format("%Y-%m-%d").to_string();
        assert_eq!(days[6]["date"], today.as_str());
        assert_eq!(days[0]["pass"], 0);
        assert_eq!(days[6]["pass"], 1);
        assert_eq!(days[6]["changes_requested"], 1);
        assert_eq!(days[6]["crashes"], 1);
        assert_eq!(
            body["totals"],
            serde_json::json!({ "pass": 1, "changes_requested": 1, "crashes": 1, "retries": 1 })
        );
        assert_eq!(
            body["by_project"],
            serde_json::json!([{
                "project_id": "p1", "project_name": "Alpha", "pass": 1, "changes_requested": 1
            }])
        );
        assert_eq!(
            get(None).await.unwrap().0["days"].as_array().unwrap().len(),
            30
        );
        for bad in ["0", "181", "x"] {
            let (status, _) = get(Some(bad)).await.unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
    }
}
