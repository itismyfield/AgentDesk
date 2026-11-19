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

fn durable(
    f: &Fixture,
    evidence: &Evidence,
) -> crate::services::tui_input::human_frame::history::v2::native::Settlement {
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
    f.history.try_native_settlement(NS, &f.ticket).unwrap()
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
        let settlement = durable(&f, &evidence);
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
        record(&shared, &settlement, &evidence, key, Some(&row), None).await;
        let first = scheduled(&pool, &turn_id).await;
        record(&shared, &settlement, &evidence, key, Some(&row), None).await;
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
    row.user_text = "DISPATCH: unrelated native boot quote".into();
    assert!(row.dispatch_id.is_none());
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
    assert!(
        kept.is_empty(),
        "native row reached legacy restore dispatch inference"
    );
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id).is_some()
    );
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

#[tokio::test]
async fn native_8148_fix1_boot_dynamic_channel_transcript_pg() {
    use super::super::super::native_prepare::BootInputs;
    use super::super::native_supply::native_completion::RECORD_REGISTRY_LOCK;
    use std::sync::atomic::{AtomicBool, Ordering};
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let root = runtime.path().join("runtime");
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared = b2b_shared(Some(pool.clone()));
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 2;
    for (index, case) in [
        "primary",
        "alias",
        "foreign",
        "unregistered-key",
        "scheduled-snapshot",
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::at(
            root.join("injected_frames.json"),
            81480530 + index as u64,
            true,
        );
        let mut row = b2b_row(&f);
        let channel = ChannelId::new(row.channel_id);
        assert!(
            !shared.core.lock().await.sessions.contains_key(&channel),
            "{case}"
        );
        assert!(
            crate::services::discord::adk_session::registered_channel_fallback_name(
                channel,
                &ProviderKind::Claude
            )
            .is_none(),
            "{case}"
        );
        let session_key = format!("fixture-host:{case}");
        let primary = match case {
            "alias" => "fixture-host:new-alias",
            "unregistered-key" => "fixture-host:different-primary",
            _ => &session_key,
        };
        let owner = if case == "foreign" {
            row.channel_id + 500
        } else {
            row.channel_id
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id)
             VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3) RETURNING id",
        )
        .bind(primary).bind(&shared.token_hash).bind(owner.to_string()).fetch_one(&pool).await.unwrap();
        if case == "alias" {
            sqlx::query(
                "INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)",
            )
            .bind(&session_key)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
        row.session_key = Some(session_key);
        let turn_id = turn(&row);
        if case == "scheduled-snapshot" {
            row.scheduled = ScheduledTurnMark::Scheduled {
                turn_id: turn_id.clone(),
                isolated: true,
                outcome: None,
            };
        }
        inflight::save_inflight_state(&row).unwrap();
        let evidence = proof(&f, Some("restored answer"), Disposition::Executed);
        durable(&f, &evidence);
        let boot = BootInputs::prepare(&root, 2).unwrap();
        if case == "alias" {
            let held = Arc::new(AtomicBool::new(false));
            RECORD_REGISTRY_LOCK
                .scope(
                    (root.join("injected_frames.json"), held.clone()),
                    boot.restore(&shared),
                )
                .await
                .unwrap();
            assert!(
                held.load(Ordering::SeqCst),
                "boot reached the registry lock boundary"
            );
        } else {
            boot.restore(&shared).await.unwrap();
        }
        assert!(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id)
                .is_none()
        );
        let transcript = transcript(&pool, &turn_id).await;
        if matches!(case, "primary" | "alias") {
            assert!(
                transcript
                    .is_some_and(|(body, context)| body.contains("restored answer") && context),
                "dynamic channel loses its transcript at boot: {case}"
            );
        } else {
            assert_eq!(transcript, None, "{case}");
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_fix1_live_record_survives_registry_contention_pg() {
    use super::super::native_supply::native_completion::RECORD_REGISTRY_LOCK;
    use std::sync::atomic::{AtomicBool, Ordering};
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let path = runtime.path().join("runtime/injected_frames.json");
    let f = Fixture::at(path.clone(), 81480535, true);
    let shared = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &shared).await;
    let mut row = b2b_row(&f);
    let turn_id = turn(&row);
    launch_agent_delivery(&pool, "native-contention", &turn_id, false, "fail").await;
    row.scheduled = ScheduledTurnMark::Scheduled {
        turn_id: turn_id.clone(),
        isolated: false,
        outcome: None,
    };
    inflight::save_inflight_state(&row).unwrap();
    let prepared =
        NativePrepared::capture(&f.history, NS, f.ticket.clone(), key, Some(actor), &shared)
            .await
            .unwrap();
    let batch = b2b_batch(&f);
    prepared.any_key(&f.history).unwrap();
    let target = &f.ticket.source.as_ref().unwrap().target;
    let work = prepared
        .admit(&batch, &batch, 0, target, &f.history, &shared)
        .await
        .unwrap();
    let held = Arc::new(AtomicBool::new(false));
    assert!(
        RECORD_REGISTRY_LOCK
            .scope((path, held.clone()), b2b_submit(&shared, work, &f.history))
            .await
            .unwrap()
    );
    assert!(
        held.load(Ordering::SeqCst),
        "registry lock fixture ran at the record boundary"
    );
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id).is_none()
    );
    assert!(
        f.history
            .try_native_settlement(NS, &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    assert_eq!(
        scheduled(&pool, &turn_id)
            .await
            .as_ref()
            .map(|(status, _)| status.as_str()),
        Some("empty_response"),
        "registry contention skipped native completion before cleanup"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_fix1_marked_receipt_cannot_remove_native_row_pg() {
    use super::super::super::native_prepare::BootInputs;
    use crate::services::discord::receipt_settle;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let root = runtime.path().join("runtime");
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let f = Fixture::at(root.join("injected_frames.json"), 81480536, true);
    let mut row = b2b_row(&f);
    let mut shared = b2b_shared(Some(pool.clone()));
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 2;
    row.input_origin_ids = vec!["native-receipt-origin".into()];
    inflight::save_inflight_state(&row).unwrap();
    let unavailable = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://fixture@127.0.0.1:1/unavailable")
        .unwrap();
    unavailable.close().await;
    assert!(
        !receipt_settle::record_or_stamp(
            Some(&unavailable),
            &row,
            crate::db::turns::TerminalOutcome::Completed,
        )
        .await
    );
    let row =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id).unwrap();
    assert!(
        row.terminal_receipt.is_some(),
        "failed PG write left a durable receipt stamp"
    );
    let evidence = proof(
        &f,
        Some("native after marked receipt"),
        Disposition::Executed,
    );
    f.history.note_native_any_key(NS, &f.ticket).unwrap();
    BootInputs::prepare(&root, 2)
        .unwrap()
        .restore(&shared)
        .await
        .unwrap();
    assert!(
        crate::services::discord::turn_finalizer::native_prepare::holds_row(&shared, &row).await
    );
    let admitted =
        receipt_settle::settle_marked_row(&shared, &ProviderKind::Claude, row.channel_id).await;
    let (kept, _) = receipt_settle::settle_boot(&shared, vec![row.clone()]).await;
    assert!(
        kept.is_empty(),
        "the marked row is not handed to ordinary recovery"
    );
    assert!(
        !admitted
            && inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id)
                .is_some(),
        "watcher receipt retry deleted the native row"
    );
    let origins: Vec<String> =
        sqlx::query_scalar("SELECT consumed_origin_ids FROM turns WHERE turn_id = $1")
            .bind(turn(&row))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        origins, row.input_origin_ids,
        "receipt persisted independently of the row clear"
    );
    durable(&f, &evidence);
    BootInputs::prepare(&root, 2)
        .unwrap()
        .restore(&shared)
        .await
        .unwrap();
    assert!(
        transcript(&pool, &turn(&row))
            .await
            .is_some_and(|(body, _)| body.contains("native after marked receipt"))
    );
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id).is_none()
    );
    pool.close().await;
    db.drop().await;
}
