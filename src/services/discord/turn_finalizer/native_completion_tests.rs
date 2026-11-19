use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::{inflight, scheduled_turn::ScheduledTurnMark};
use crate::services::scheduled_messages::turn_completion::{
    ScheduledTurnOutcome, tests::launch_agent_delivery,
};
use crate::services::tui_o::writer::input_facts::native::{Evidence, Run};
use serde_json::json;

const NS: &str = "native-fixture";

#[path = "native_completion_work_tests.rs"]
mod work_dispatch;

fn proof(f: &Fixture, body: Option<&str>, disposition: Disposition) -> Evidence {
    let assistant = |id: &str, text: &str| {
        format!(
            "{}\n",
            json!({"type":"assistant","uuid":id,"sessionId":"native",
            "message":{"id":id,"role":"assistant","content":[{"type":"text","text":text}]}})
        )
    };
    let opening = format!(
        "{}\n",
        json!({"type":"user","uuid":"own","sessionId":"native",
        "message":{"content":"한국어 원본 입력"}})
    );
    let opening_end = opening.len() as u64;
    let earlier = body.map_or(String::new(), |_| assistant("earlier", "earlier answer"));
    let start = opening_end + earlier.len() as u64;
    let mut text = opening + &earlier;
    if let Some(body) = body {
        text += &assistant("answer", body);
        text += &format!(
            "{}\n",
            json!({"type":"user","uuid":"follower","sessionId":"native",
            "message":{"content":"follower prompt"}})
        );
        text += &assistant("continuation", "continued answer");
    }
    let closer_start = text.len() as u64;
    text += &format!(
        "{}\n",
        json!({"type":"system","subtype":"turn_duration","sessionId":"native"})
    );
    let end = text.len() as u64;
    std::fs::write(&f.ticket.source.as_ref().unwrap().transcript, text).unwrap();
    Evidence {
        namespace: NS.into(),
        ticket: f.ticket.clone(),
        run: Some(Run {
            start,
            native_id: None,
        }),
        range: (opening_end, end),
        membership_range: Some((0, opening_end)),
        membership_row: Some("own".into()),
        closer_range: Some((closer_start, end)),
        disposition,
        supersedes: None,
        raw: None,
    }
}

fn durable(f: &Fixture, evidence: &Evidence) {
    f.history.note_native_any_key(NS, &f.ticket).unwrap();
    let outcome = crate::services::tui_input::human_frame::history::v2::native::Outcome {
        disposition: evidence.disposition.clone(),
        range: Some([evidence.range.0, evidence.range.1]),
        extra: serde_json::Map::from_iter([("native_proof".into(), json!(evidence))]),
    };
    assert!(
        f.history
            .record_native_outcome(NS, &f.ticket, &outcome)
            .unwrap()
    );
}

fn turn(row: &inflight::InflightTurnState) -> String {
    format!("discord:{}:{}", row.channel_id, row.user_msg_id)
}

async fn transcript(pool: &sqlx::PgPool, turn_id: &str) -> Option<(String, bool)> {
    sqlx::query_as(
        "SELECT assistant_message, channel_context FROM session_transcripts WHERE turn_id = $1",
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn scheduled(
    pool: &sqlx::PgPool,
    turn_id: &str,
) -> Option<(String, chrono::DateTime<chrono::Utc>)> {
    sqlx::query_as("SELECT turn_outcome, turn_outcome_at FROM scheduled_message_deliveries WHERE turn_id = $1 AND turn_outcome IS NOT NULL")
        .bind(turn_id).fetch_optional(pool).await.unwrap()
}

#[tokio::test]
async fn native_8148_completion_records_only_its_own_run_once_pg() {
    use super::super::native_supply::native_completion::record;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = b2b_shared(Some(pool.clone()));
    for (index, case) in [
        "shared",
        "empty",
        "scheduled-isolated",
        "legacy-snapshot",
        "scope-failed",
        "cancelled",
        "replaced",
        "cleared",
        "clear-fence",
        "unreadable",
        "follower",
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::at(
            runtime.path().join(format!("runtime/{case}/frames.json")),
            81480500 + index as u64,
            case != "follower",
        );
        let (mut row, key) = if case == "follower" {
            let source = f.ticket.source.as_ref().unwrap();
            let row = inflight::InflightTurnState::new(
                ProviderKind::Claude,
                f.ticket.channel_id,
                None,
                7,
                f.ticket.channel_id + 1,
                0,
                "follower".into(),
                Some(source.session_id.clone()),
                None,
                Some(source.transcript.display().to_string()),
                None,
                0,
            );
            (
                row,
                TurnKey::new(
                    ChannelId::new(f.ticket.channel_id),
                    f.ticket.channel_id + 1,
                    1,
                ),
            )
        } else {
            let row = b2b_row(&f);
            let key = TurnKey::new(
                ChannelId::new(row.channel_id),
                row.user_msg_id,
                row.born_generation,
            )
            .with_episode_nonce(row.turn_nonce.as_deref());
            (row, key)
        };
        row.born_generation = key.generation;
        row.hosted_execution_nonce = Some("incarnation-1".into());
        let turn_id = turn(&row);
        let legacy = case == "legacy-snapshot";
        launch_agent_delivery(&pool, case, &turn_id, legacy, "fail").await;
        row.scheduled = if legacy {
            ScheduledTurnMark::Legacy
        } else {
            ScheduledTurnMark::Scheduled {
                turn_id: turn_id.clone(),
                isolated: matches!(case, "scheduled-isolated" | "scope-failed"),
                outcome: Some(ScheduledTurnOutcome::ProviderError),
            }
        };
        inflight::save_inflight_state(&row).unwrap();
        let evidence = proof(
            &f,
            (case != "empty").then_some("own answer"),
            if case == "cancelled" {
                Disposition::Cancelled
            } else {
                Disposition::Executed
            },
        );
        durable(&f, &evidence);
        if case == "scheduled-isolated" {
            sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('native-completion', 'fixture', 'fixture')").execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO routine_runs (id, routine_id, turn_id, result_json) VALUES ('run', 'native-completion', $1, $2)")
                .bind(&turn_id).bind(json!({"channel_id":row.channel_id.to_string()})).execute(&pool).await.unwrap();
        }
        if case == "scope-failed" {
            sqlx::query("ALTER TABLE routine_runs RENAME TO unavailable_routine_runs")
                .execute(&pool)
                .await
                .unwrap();
        }
        match case {
            "replaced" => {
                let mut successor = row.clone();
                successor.turn_start_offset = Some(1);
                inflight::save_inflight_state(&successor).unwrap();
            }
            "cleared" => assert!(inflight::clear_inflight_state(
                &ProviderKind::Claude,
                row.channel_id
            )),
            "clear-fence" => crate::db::session_transcripts::record_channel_clear_boundary(
                Some(&pool),
                &row.channel_id.to_string(),
            )
            .await
            .unwrap(),
            "unreadable" => {
                std::fs::write(&f.ticket.source.as_ref().unwrap().transcript, "").unwrap()
            }
            _ => {}
        }
        record(&shared, &f.history, &evidence, key, Some(&row), None).await;
        let first = scheduled(&pool, &turn_id).await;
        record(&shared, &f.history, &evidence, key, Some(&row), None).await;
        assert_eq!(scheduled(&pool, &turn_id).await, first, "{case}");
        if case == "scope-failed" {
            sqlx::query("ALTER TABLE unavailable_routine_runs RENAME TO routine_runs")
                .execute(&pool)
                .await
                .unwrap();
        }
        if matches!(case, "shared" | "scheduled-isolated") {
            let (body, channel_context) = transcript(&pool, &turn_id).await.expect(case);
            assert!(
                body.contains("own answer") && body.contains("continued answer"),
                "{case}: {body}"
            );
            assert!(!body.contains("earlier answer"), "{case}: {body}");
            assert!(channel_context, "{case}");
        } else {
            assert_eq!(transcript(&pool, &turn_id).await, None, "{case}");
        }
        let expected = match case {
            "shared" | "scheduled-isolated" | "legacy-snapshot" | "scope-failed"
            | "clear-fence" => Some("delivered"),
            "empty" => Some("empty_response"),
            _ => None,
        };
        assert_eq!(
            first.as_ref().map(|(outcome, _)| outcome.as_str()),
            expected,
            "{case}"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_completion_empty_boot_ignores_watcher_mirror_and_retries_once_pg() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let root = runtime.path().join("runtime");
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let f = Fixture::at(root.join("injected_frames.json"), 81480520, true);
    let mut row = b2b_row(&f);
    let turn_id = turn(&row);
    launch_agent_delivery(&pool, "native-empty", &turn_id, false, "fail").await;
    row.scheduled = ScheduledTurnMark::Scheduled {
        turn_id: turn_id.clone(),
        isolated: false,
        outcome: Some(ScheduledTurnOutcome::ProviderError),
    };
    row.terminal_delivery_committed = true;
    row.full_response = "wrong watcher outcome".into();
    inflight::save_inflight_state(&row).unwrap();
    let mut shared = b2b_shared(Some(pool.clone()));
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 2;
    let proof = proof(&f, None, Disposition::Executed);
    f.history.note_native_any_key(NS, &f.ticket).unwrap();
    let boot = super::super::super::native_prepare::BootInputs::prepare(&root, 2).unwrap();
    boot.restore(&shared).await.unwrap();
    let (kept, _) =
        crate::services::discord::receipt_settle::settle_boot(&shared, vec![row.clone()]).await;
    assert_eq!(kept.len(), 1);
    assert_eq!(
        scheduled(&pool, &turn_id).await,
        None,
        "a native row's old mirror cannot settle it at boot"
    );

    durable(&f, &proof);
    let restore = || async {
        super::super::super::native_prepare::BootInputs::prepare(&root, 2)
            .unwrap()
            .restore(&shared)
            .await
            .unwrap();
    };
    restore().await;
    let first = scheduled(&pool, &turn_id)
        .await
        .expect("empty scheduled outcome");
    assert_eq!(first.0, "empty_response");
    restore().await;
    assert_eq!(scheduled(&pool, &turn_id).await, Some(first));
    assert_eq!(transcript(&pool, &turn_id).await, None);
    assert!(
        f.history
            .native_settlement(NS, &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    pool.close().await;
    db.drop().await;
}
