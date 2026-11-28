use crate::services::discord::turn_finalizer::terminal_handler::native_supply::native_completion::{hook_tests::RECORD_REGISTRY_LOCK, record};
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::{inflight, scheduled_turn::ScheduledTurnMark};
use crate::services::discord::{receipt_settle, turn_finalizer::native_prepare::BootInputs};
use crate::services::scheduled_messages::turn_completion::{
    ScheduledTurnOutcome, tests::launch_agent_delivery,
};
use crate::services::tui_input::human_frame::history::v2::native::{Outcome, Settlement};
use crate::services::tui_o::writer::input_facts::native::Evidence;
use serde_json::json;
use sqlx::PgPool;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const NS: &str = "native-fixture";

#[path = "native_completion_work_tests.rs"]
mod work_dispatch;

struct CompletionFixture {
    _runtime: TestRuntimeRootGuard,
    db: TestPostgresDb,
    pool: PgPool,
    root: PathBuf,
}

impl CompletionFixture {
    async fn new() -> Self {
        let runtime = TestRuntimeRootGuard::new();
        Self::in_runtime(runtime).await
    }

    async fn in_runtime(runtime: TestRuntimeRootGuard) -> Self {
        let root = runtime.path().join("runtime");
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        Self {
            _runtime: runtime,
            db,
            pool,
            root,
        }
    }

    fn input(&self, channel: u64) -> Fixture {
        Fixture::at(self.root.join("injected_frames.json"), channel, true)
    }

    fn shared(&self, generation: u64) -> Arc<SharedData> {
        let mut shared = b2b_shared(Some(self.pool.clone()));
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = generation;
        shared
    }

    async fn restore(&self, shared: &Arc<SharedData>) {
        BootInputs::prepare(&self.root, 2)
            .unwrap()
            .restore(shared)
            .await
            .unwrap();
    }

    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn on_disk(channel: u64) -> Option<inflight::InflightTurnState> {
    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
}

fn cleanup_complete(f: &Fixture) -> bool {
    f.history
        .try_native_settlement(NS, &f.ticket)
        .unwrap()
        .cleanup_complete
}

async fn during_registry_lock<T>(path: &Path, action: impl Future<Output = T>) -> T {
    let held = Arc::new(AtomicBool::new(false));
    let result = RECORD_REGISTRY_LOCK
        .scope((path.to_path_buf(), held.clone()), action)
        .await;
    assert!(
        held.load(Ordering::SeqCst),
        "registry lock fixture ran at the record boundary"
    );
    result
}

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
    Evidence::completed_for_test(
        NS.into(),
        f.ticket.clone(),
        start,
        (opening_end, closer_start, end),
        "own".into(),
        disposition,
    )
}

fn durable(f: &Fixture, evidence: &Evidence) -> Settlement {
    f.history.note_native_any_key(NS, &f.ticket).unwrap();
    let outcome = Outcome {
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

async fn transcript(pool: &PgPool, turn_id: &str) -> Option<(String, bool)> {
    sqlx::query_as(
        "SELECT assistant_message, channel_context FROM session_transcripts WHERE turn_id = $1",
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn scheduled(
    pool: &PgPool,
    turn_id: &str,
) -> Option<(String, chrono::DateTime<chrono::Utc>)> {
    sqlx::query_as("SELECT turn_outcome, turn_outcome_at FROM scheduled_message_deliveries WHERE turn_id = $1 AND turn_outcome IS NOT NULL")
        .bind(turn_id).fetch_optional(pool).await.unwrap()
}

#[tokio::test]
async fn native_8148_completion_records_only_its_own_run_once_pg() {
    let fixture = CompletionFixture::new().await;
    let pool = &fixture.pool;
    let shared = fixture.shared(1);
    for (index, (case, outcome, writes_transcript)) in [
        ("shared", Some("delivered"), true),
        ("scheduled-isolated", Some("delivered"), true),
        ("clear-fence", Some("delivered"), false),
        ("cancelled", None, false),
        ("replaced", None, false),
        ("cleared", None, false),
        ("unreadable", None, false),
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::at(
            fixture.root.join(format!("{case}.json")),
            81480500 + index as u64,
            true,
        );
        let mut row = b2b_row(&f);
        let key = TurnKey::new(
            ChannelId::new(row.channel_id),
            row.user_msg_id,
            row.born_generation,
        )
        .with_episode_nonce(row.turn_nonce.as_deref());
        let turn_id = turn(&row);
        launch_agent_delivery(pool, case, &turn_id, false, "fail").await;
        row.scheduled = ScheduledTurnMark::Scheduled {
            turn_id: turn_id.clone(),
            isolated: case == "scheduled-isolated",
            outcome: Some(ScheduledTurnOutcome::ProviderError),
        };
        inflight::save_inflight_state(&row).unwrap();
        let evidence = proof(
            &f,
            Some("own answer"),
            if case == "cancelled" {
                Disposition::Cancelled
            } else {
                Disposition::Executed
            },
        );
        let settlement = durable(&f, &evidence);
        if case == "scheduled-isolated" {
            sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('native-completion', 'fixture', 'fixture')").execute(pool).await.unwrap();
            sqlx::query("INSERT INTO routine_runs (id, routine_id, turn_id, result_json) VALUES ('run', 'native-completion', $1, $2)")
                .bind(&turn_id).bind(json!({"channel_id":row.channel_id.to_string()})).execute(pool).await.unwrap();
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
                Some(pool),
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
        let first = scheduled(pool, &turn_id).await;
        record(&shared, &settlement, &evidence, key, Some(&row), None).await;
        assert_eq!(scheduled(pool, &turn_id).await, first, "{case}");
        if writes_transcript {
            let (body, channel_context) = transcript(pool, &turn_id).await.expect(case);
            assert!(
                body.contains("own answer") && body.contains("continued answer"),
                "{case}: {body}"
            );
            assert!(!body.contains("earlier answer"), "{case}: {body}");
            assert!(channel_context, "{case}");
        } else {
            assert_eq!(transcript(pool, &turn_id).await, None, "{case}");
        }
        assert_eq!(
            first.as_ref().map(|(status, _)| status.as_str()),
            outcome,
            "{case}"
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn native_8148_completion_empty_boot_ignores_watcher_mirror_and_retries_once_pg() {
    let fixture = CompletionFixture::new().await;
    let f = fixture.input(81480520);
    let shared = fixture.shared(2);
    let pool = &fixture.pool;
    let mut row = b2b_row(&f);
    let turn_id = turn(&row);
    launch_agent_delivery(pool, "native-empty", &turn_id, false, "fail").await;
    row.scheduled = ScheduledTurnMark::Scheduled {
        turn_id: turn_id.clone(),
        isolated: false,
        outcome: Some(ScheduledTurnOutcome::ProviderError),
    };
    row.terminal_delivery_committed = true;
    row.full_response = "wrong watcher outcome".into();
    row.user_text = "DISPATCH: unrelated native boot quote".into();
    inflight::save_inflight_state(&row).unwrap();
    let evidence = proof(&f, None, Disposition::Executed);
    f.history.note_native_any_key(NS, &f.ticket).unwrap();

    fixture.restore(&shared).await;
    let (kept, _) = receipt_settle::settle_boot(&shared, vec![row.clone()]).await;
    assert!(
        kept.is_empty(),
        "native row reached legacy restore dispatch inference"
    );
    assert!(on_disk(row.channel_id).is_some());
    assert_eq!(
        scheduled(pool, &turn_id).await,
        None,
        "native row's old mirror cannot settle it at boot"
    );

    let mut row = on_disk(row.channel_id).unwrap();
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
    let row = on_disk(row.channel_id).unwrap();
    assert!(row.terminal_receipt.is_some());
    let shared = fixture.shared(2);
    fixture.restore(&shared).await;
    let admitted =
        receipt_settle::settle_marked_row(&shared, &ProviderKind::Claude, row.channel_id).await;
    let (kept, _) = receipt_settle::settle_boot(&shared, vec![row.clone()]).await;
    assert!(
        kept.is_empty() && !admitted && on_disk(row.channel_id).is_some(),
        "watcher receipt retry deleted the native row"
    );
    let origins: Vec<String> =
        sqlx::query_scalar("SELECT consumed_origin_ids FROM turns WHERE turn_id = $1")
            .bind(&turn_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(origins, row.input_origin_ids);

    durable(&f, &evidence);
    fixture.restore(&shared).await;
    let first = scheduled(pool, &turn_id)
        .await
        .expect("empty scheduled outcome");
    assert_eq!(first.0, "empty_response");
    fixture.restore(&shared).await;
    assert_eq!(scheduled(pool, &turn_id).await, Some(first));
    assert_eq!(transcript(pool, &turn_id).await, None);
    assert!(cleanup_complete(&f));
    fixture.close().await;
}

#[tokio::test]
async fn native_8148_fix1_boot_dynamic_channel_transcript_pg() {
    let fixture = CompletionFixture::new().await;
    let shared = fixture.shared(2);
    let pool = &fixture.pool;
    for (index, (case, primary, channel_offset, records)) in [
        ("primary", "primary", 0, true),
        ("alias", "new-primary", 0, true),
        ("foreign", "foreign", 500, false),
        ("unregistered-key", "different-primary", 0, false),
        ("scheduled-snapshot", "scheduled-snapshot", 0, false),
    ]
    .into_iter()
    .enumerate()
    {
        let f = fixture.input(81480530 + index as u64);
        let mut row = b2b_row(&f);
        let channel = ChannelId::new(row.channel_id);
        assert!(
            !shared.core.lock().await.sessions.contains_key(&channel),
            "{case}"
        );
        assert!(
            crate::services::discord::adk_session::registered_channel_fallback_name(
                channel,
                &ProviderKind::Claude,
            )
            .is_none(),
            "{case}"
        );
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id)
             VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3) RETURNING id",
        )
        .bind(format!("fixture-host:{primary}")).bind(&shared.token_hash)
        .bind((row.channel_id + channel_offset).to_string()).fetch_one(pool).await.unwrap();
        let key = format!("fixture-host:{case}");
        if case == "alias" {
            sqlx::query(
                "INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)",
            )
            .bind(&key)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }
        row.session_key = Some(key);
        let turn_id = turn(&row);
        if case == "scheduled-snapshot" {
            row.scheduled = ScheduledTurnMark::Scheduled {
                turn_id: turn_id.clone(),
                isolated: true,
                outcome: None,
            };
        }
        inflight::save_inflight_state(&row).unwrap();
        durable(
            &f,
            &proof(&f, Some("restored answer"), Disposition::Executed),
        );
        if case == "alias" {
            during_registry_lock(
                &fixture.root.join("injected_frames.json"),
                fixture.restore(&shared),
            )
            .await;
        } else {
            fixture.restore(&shared).await;
        }
        assert!(on_disk(row.channel_id).is_none());
        let stored = transcript(pool, &turn_id).await;
        if records {
            assert!(
                stored.is_some_and(|(body, context)| body.contains("restored answer") && context),
                "dynamic channel loses its transcript at boot: {case}"
            );
        } else {
            assert_eq!(stored, None, "{case}");
        }
    }
    fixture.close().await;
}

#[tokio::test]
async fn native_8148_fix1_live_record_survives_registry_contention_pg() {
    let fixture = CompletionFixture::new().await;
    let f = fixture.input(81480535);
    let shared = fixture.shared(1);
    let (key, actor) = b2b_active(&f, &shared).await;
    let mut row = b2b_row(&f);
    let turn_id = turn(&row);
    launch_agent_delivery(&fixture.pool, "native-contention", &turn_id, false, "fail").await;
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
    assert!(
        during_registry_lock(
            &fixture.root.join("injected_frames.json"),
            b2b_submit(&shared, work, &f.history)
        )
        .await
        .unwrap()
    );
    assert!(cleanup_complete(&f) && on_disk(row.channel_id).is_none());
    assert_eq!(
        scheduled(&fixture.pool, &turn_id)
            .await
            .map(|(status, _)| status)
            .as_deref(),
        Some("empty_response"),
        "registry contention skipped native completion before cleanup"
    );
    fixture.close().await;
}

/// A native turn leaves its run's session as the channel selector the next turn and a restart
/// resume; a user clear, a pane that moved on and another channel's row are left as they are.
#[test]
fn native_8261_completion_stores_the_run_session_as_channel_selector_pg() {
    use crate::services::tui_prompt_dedupe as dedupe;
    let _rotations = dedupe::lock_claude_session_rotations_for_tests();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = CompletionFixture::new().await;
        let (pool, shared) = (&fixture.pool, fixture.shared(1));
        let cases = ["fresh", "boot", "cleared", "clear-boundary", "rotated", "foreign"];
        for (index, case) in cases.into_iter().enumerate() {
            let f = Fixture::at(
                fixture.root.join(format!("{case}.json")),
                82610500 + index as u64,
                true,
            );
            let mut row = b2b_row(&f);
            let (channel, key) = (ChannelId::new(row.channel_id), format!("fixture-host:{case}"));
            let run = f.ticket.source.as_ref().unwrap().session_id.clone();
            let owner = row.channel_id + if case == "foreign" { 500 } else { 0 };
            sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3)")
                .bind(&key).bind(&shared.token_hash).bind(owner.to_string()).execute(pool).await.unwrap();
            row.session_key = Some(key.clone());
            row.tmux_session_name = Some(format!("pane-{case}"));
            inflight::save_inflight_state(&row).unwrap();
            if case != "boot" {
                crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, case).await;
                shared.core.lock().await.sessions.get_mut(&channel).unwrap().cleared = case == "cleared";
            }
            if case == "clear-boundary" {
                let cleared = crate::db::session_transcripts::record_channel_clear_boundary;
                cleared(Some(pool), &row.channel_id.to_string()).await.unwrap();
            }
            if case == "rotated" {
                dedupe::record_claude_session_rotation(dedupe::ClaudeSessionRotation {
                    tmux_session_name: "pane-rotated".into(),
                    old_session_id: Some(run.clone()),
                    old_output_path: String::new(),
                    old_last_offset: 0,
                    new_session_id: "moved-on".into(),
                    new_output_path: String::new(),
                    observed_drain_frontier: 0,
                    polls_without_drain_progress: 0,
                    selector_followed: false,
                    left_session_ids: Vec::new(),
                });
            }
            let evidence = proof(&f, Some("own answer"), Disposition::Executed);
            let settlement = durable(&f, &evidence);
            let turn = TurnKey::new(channel, row.user_msg_id, row.born_generation)
                .with_episode_nonce(row.turn_nonce.as_deref());
            record(&shared, &settlement, &evidence, turn, Some(&row), None).await;
            let expected = match case {
                "fresh" | "boot" => Some(run),
                "rotated" => Some("moved-on".to_string()),
                _ => None,
            };
            let stored: (Option<String>, Option<String>) = sqlx::query_as("SELECT claude_session_id, raw_provider_session_id FROM sessions WHERE session_key = $1")
                .bind(&key).fetch_one(pool).await.unwrap();
            assert_eq!(stored, (expected.clone(), expected.clone()), "{case}");
            let cached = shared.core.lock().await.sessions.get(&channel).and_then(|live| live.session_id.clone());
            assert_eq!(cached, expected.filter(|_| case != "boot"), "{case}");
        }
        fixture.close().await;
    });
}
