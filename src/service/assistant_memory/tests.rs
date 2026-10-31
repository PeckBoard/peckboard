use chrono::{DateTime, Duration, TimeZone, Utc};

use super::lifecycle::{self, Boundary, ClearDecision, ClearInputs, EndedTopic, PendingClear};
use super::*;
use crate::db::models::{NewFolder, NewSession};
use crate::service::assistant_settings::{AssistantSettings, AutoClear};
use crate::service::voice_relay::VOICE_EXPERT_KIND;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap()
}

async fn seed_voice(db: &Db, voice_id: &str, folder_path: &str) {
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "f".into(),
        path: folder_path.into(),
        created_at: "now".into(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: voice_id.into(),
        name: "Assistant".into(),
        folder_id: "f1".into(),
        created_at: "now".into(),
        last_activity: "now".into(),
        is_expert: true,
        expert_kind: Some(VOICE_EXPERT_KIND.into()),
        system_prompt: Some(
            crate::service::voice_prompt::session_prompt(db)
                .await
                .unwrap(),
        ),
        ..Default::default()
    })
    .await
    .unwrap();
}

fn item(kind: MemoryKind, summary: &str) -> MemoryItem {
    MemoryItem {
        kind,
        summary: redact(summary).unwrap(),
        ref_id: None,
    }
}

fn topic(title: &str) -> TopicRef {
    TopicRef {
        id: Some(format!("topic-{title}")),
        title: title.into(),
    }
}

/// Retention and `max_entries` follow the injected clock: the oldest rows
/// are pruned on write, and a read sweeps whatever expired by `now`.
#[tokio::test]
async fn sweep_and_pruning_follow_the_injected_clock() {
    let db = Db::in_memory().unwrap();
    seed_voice(&db, "mem-voice", "/tmp").await;
    let settings = AssistantSettings {
        retention_hours: 1,
        max_entries: 5,
        ..Default::default()
    };
    for i in 0..7 {
        let at = t0() + Duration::minutes(i);
        write_items(
            &db,
            "mem-voice",
            &settings,
            &topic("T"),
            vec![item(MemoryKind::Did, &format!("fact {i}"))],
            at,
        )
        .await
        .unwrap();
    }
    let rows = list_live(&db, "mem-voice", t0() + Duration::minutes(30))
        .await
        .unwrap();
    let kept: Vec<&str> = rows.iter().map(|m| m.summary.as_str()).collect();
    assert_eq!(kept, ["fact 2", "fact 3", "fact 4", "fact 5", "fact 6"]);

    // fact 2..4 expire at 13:02..13:04 (inclusive at `now`).
    let rows = list_live(&db, "mem-voice", t0() + Duration::minutes(64))
        .await
        .unwrap();
    let kept: Vec<&str> = rows.iter().map(|m| m.summary.as_str()).collect();
    assert_eq!(kept, ["fact 5", "fact 6"]);
    assert_eq!(
        sweep(&db, t0() + Duration::hours(2)).await.unwrap(),
        2,
        "the periodic sweep deletes the rest"
    );

    // Memory off: nothing is written.
    let off = AssistantSettings {
        memory_enabled: false,
        ..Default::default()
    };
    assert!(
        write_items(
            &db,
            "mem-voice",
            &off,
            &topic("T"),
            vec![item(MemoryKind::Did, "x")],
            t0()
        )
        .await
        .is_err()
    );
    assert!(
        write_fallback(&db, "mem-voice", &off, "t", "T", None, t0())
            .await
            .unwrap()
            .is_none()
    );
}

fn quiet() -> ClearInputs {
    ClearInputs {
        now: t0() + Duration::minutes(3),
        auto_clear: AutoClear::AfterTopic,
        scheduled_at: t0(),
        due: t0() + Duration::minutes(2),
        last_utterance: t0() - Duration::minutes(1),
        last_focus_switch: None,
        turn_running: false,
        user_speaking: false,
        tts_playing: false,
        held_relay_for_topic: false,
    }
}

#[test]
fn auto_clear_fires_only_when_everything_is_quiet() {
    assert_eq!(lifecycle::decide(&quiet()), ClearDecision::Fire);
    let blockers: Vec<(&str, ClearInputs)> = vec![
        (
            "busy turn",
            ClearInputs {
                turn_running: true,
                ..quiet()
            },
        ),
        (
            "held relay",
            ClearInputs {
                held_relay_for_topic: true,
                ..quiet()
            },
        ),
        (
            "speaking",
            ClearInputs {
                user_speaking: true,
                ..quiet()
            },
        ),
        (
            "tts",
            ClearInputs {
                tts_playing: true,
                ..quiet()
            },
        ),
        (
            "auto_clear off",
            ClearInputs {
                auto_clear: AutoClear::Off,
                ..quiet()
            },
        ),
        (
            "delay not elapsed",
            ClearInputs {
                now: t0() + Duration::minutes(1),
                ..quiet()
            },
        ),
        (
            "utterance",
            ClearInputs {
                last_utterance: t0() + Duration::seconds(30),
                ..quiet()
            },
        ),
        (
            "focus elsewhere",
            ClearInputs {
                last_focus_switch: Some(t0() + Duration::seconds(30)),
                ..quiet()
            },
        ),
    ];
    for (why, inputs) in blockers {
        assert_ne!(lifecycle::decide(&inputs), ClearDecision::Fire, "{why}");
    }
    // An utterance / focus is final; the rest only wait.
    assert!(matches!(
        lifecycle::decide(&ClearInputs {
            last_utterance: t0() + Duration::seconds(1),
            ..quiet()
        }),
        ClearDecision::Cancel(_)
    ));
    assert!(matches!(
        lifecycle::decide(&ClearInputs {
            tts_playing: true,
            ..quiet()
        }),
        ClearDecision::Wait(_)
    ));
    // The idle boundary.
    assert!(!lifecycle::idle_due(t0() + Duration::minutes(29), t0(), 30));
    assert!(lifecycle::idle_due(t0() + Duration::minutes(30), t0(), 30));
}

/// The clear path: memory kept (plus a fallback for the topic that had
/// none), transcript cleared, the prompt rebuilt with only unexpired
/// entries, and a `system` event posted. Later memory writes and prompt
/// re-applies leave the snapshot alone (no agent restart).
#[tokio::test]
async fn clear_keeps_memory_clears_session_and_snapshots_the_prompt() {
    use crate::auth::middleware::tests::test_state;
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let voice_id = "clear-voice";
    seed_voice(&state.db, voice_id, &dir.path().to_string_lossy()).await;
    lifecycle::reset_for_test(voice_id);
    let settings = AssistantSettings::default();
    let now = Utc::now();

    state
        .db
        .append_event(voice_id, "user", serde_json::json!({ "text": "hello" }))
        .await
        .unwrap();
    write_items(
        &state.db,
        voice_id,
        &settings,
        &topic("Dinner plans"),
        vec![item(MemoryKind::Did, "Booked the table for eight.")],
        now,
    )
    .await
    .unwrap();
    write_items(
        &state.db,
        voice_id,
        &settings,
        &topic("Old thing"),
        vec![item(MemoryKind::Learned, "An expired fact.")],
        now - Duration::hours(40),
    )
    .await
    .unwrap();

    let pending = PendingClear {
        scheduled_at: now,
        due: now,
        reason: Boundary::Close,
        topics: vec![EndedTopic {
            id: "topic-garden".into(),
            title: "Garden".into(),
            summary: Some("tomatoes need staking".into()),
            kind: "conversation".into(),
            ref_id: None,
        }],
    };
    lifecycle::fire(&state, voice_id, &pending, &settings, now)
        .await
        .unwrap();

    let events = state.db.events_tail(voice_id, 50).await.unwrap();
    assert_eq!(events.len(), 1, "only the clear notice is left");
    assert_eq!(events[0].kind, "system");
    assert!(
        events[0]
            .data
            .contains("Context cleared after \\\"Garden\\\" · 2 memories kept"),
        "{}",
        events[0].data
    );

    let rows = state.db.list_assistant_memories(voice_id).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|m| m.source == "fallback" && m.summary == "tomatoes need staking")
    );

    let prompt = state
        .db
        .get_session(voice_id)
        .await
        .unwrap()
        .unwrap()
        .system_prompt
        .unwrap();
    assert!(prompt.contains(RECENT_MEMORY_HEADING), "{prompt}");
    assert!(prompt.contains("Booked the table for eight."));
    assert!(prompt.contains("tomatoes need staking"));
    assert!(!prompt.contains("An expired fact."));
    assert_eq!(prompt.matches(RECENT_MEMORY_HEADING).count(), 1);
    let st = lifecycle::status(voice_id);
    assert_eq!(st["last_clear"]["kept"], 2);
    assert_eq!(st["last_clear"]["topic_title"], "Garden");

    // A new memory and a prompt re-apply don't move the snapshot.
    write_items(
        &state.db,
        voice_id,
        &settings,
        &topic("Taxes"),
        vec![item(MemoryKind::Pending, "File the return.")],
        now,
    )
    .await
    .unwrap();
    assert!(
        !crate::service::voice_prompt::apply_to_voice_session(&state.db, None)
            .await
            .unwrap()
    );
    let after = state
        .db
        .get_session(voice_id)
        .await
        .unwrap()
        .unwrap()
        .system_prompt
        .unwrap();
    assert_eq!(after, prompt);
    lifecycle::reset_for_test(voice_id);
}
