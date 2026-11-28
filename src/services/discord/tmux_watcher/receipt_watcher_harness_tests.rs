//! A watcher ending a managed execution on the real loop commits its receipt before the row
//! goes; a watcher cancelled for a replacement hands the turn over and records nothing.

use crate::services::discord::InflightTurnState;
use crate::services::discord::inflight::TurnSource;
use crate::services::discord::tmux::tmux_watcher::tests::streaming_harness_tests::{
    CLAUDE, Harness, field, isolated_in_module, said, stop, user,
};
use poise::serenity_prelude::{self as serenity, ChannelId};
use std::sync::{Arc, atomic::Ordering};

const T0: &str = "I7340 T0 delivered before the watcher started";
const BODY: &str = "I7340 answer the restored watcher relays";
const ORIGIN: &str = "G-watcher-origin";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i8432_herdr_soft_terminal_submits_and_releases_the_handed_off_turn_pg() {
    use crate::services::discord::inflight::herdr_turn_evidence_tests::HELD;

    let test = "i8432_herdr_soft_terminal_submits_and_releases_the_handed_off_turn_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    run_herdr_soft_terminal_case(&pool, test, 432, HELD[4], true).await;
    run_herdr_soft_terminal_case(&pool, test, 433, HELD[1], false).await;
    run_herdr_soft_terminal_case(&pool, test, 434, HELD[2], false).await;
    pool.close().await;
    db.drop().await;
}

async fn run_herdr_soft_terminal_case(
    pool: &sqlx::PgPool,
    test: &'static str,
    case: u64,
    hosted: crate::services::discord::inflight::herdr_turn_evidence_tests::HerdrCase,
    authorized: bool,
) {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::inflight::herdr_turn_evidence_tests::mark_herdr;
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, InflightTurnIdentity, RelayOwnerKind,
        stamp_runtime_handoff_if_matches_identity,
    };
    use crate::services::discord::turn_finalizer::{CompletionAdmissionPlan, TurnKey};

    let prefix = format!("{}{}{}", user("earlier question"), said(T0), stop());
    let start = prefix.len() as u64;
    let origin = format!("i8432-origin-{case}");
    let mut h = Harness::on(case, &prefix, Some(pool.clone())).await;
    h.commit(0, start);
    let mut prelaunch = InflightTurnState::new(
        CLAUDE,
        h.channel.get(),
        None,
        7,
        8_432_000_000 + case,
        0,
        "question".to_string(),
        None,
        Some(h.tmux.clone()),
        None,
        None,
        0,
    );
    prelaunch.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    prelaunch.turn_source = TurnSource::Managed;
    prelaunch.turn_nonce = Some(format!("i8432-turn-{case}"));
    prelaunch.input_origin_ids = vec![origin.clone()];
    mark_herdr(
        pool,
        &h.shared.token_hash,
        &mut prelaunch,
        std::path::Path::new(&h.path),
        hosted,
    )
    .await;
    std::fs::write(&h.path, prefix).unwrap();
    let saved = prelaunch.herdr_turn_start.as_mut().unwrap();
    saved.start.offset = start;
    let at = (saved.start.submitted_at.unwrap() + chrono::Duration::seconds(1)).to_rfc3339();
    h.save(&prelaunch);
    let prelaunch = h.row().unwrap();
    let mut handoff = prelaunch.clone();
    handoff.output_path = Some(h.path.clone());
    handoff.last_offset = start;
    handoff.set_relay_owner_kind(RelayOwnerKind::Watcher);
    assert_eq!(
        stamp_runtime_handoff_if_matches_identity(
            (&prelaunch, &mut handoff),
            &InflightTurnIdentity::from_state(&prelaunch),
            test,
        ),
        GuardedSaveOutcome::Saved
    );
    let restored = h.row().unwrap();
    assert_eq!(restored.turn_start_offset, Some(0));
    assert_eq!(
        restored.herdr_turn_start.as_ref().unwrap().start.offset,
        start
    );
    drop(start_live_turn(&h, &restored, restored.turn_nonce.clone()).await);
    let generation = h.shared.restart.current_generation;
    let key = TurnKey::new(h.channel, restored.user_msg_id, generation)
        .with_episode_nonce(restored.turn_nonce.as_deref());
    h.shared
        .turn_finalizer
        .register_start_with_completion_admission(
            key,
            CLAUDE,
            RelayOwnerKind::Watcher,
            CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
            &h.shared,
        );
    assert!(
        h.shared
            .turn_finalizer
            .has_live_watcher_pending(h.channel, generation)
            .await
    );
    h.spawn(start);
    let body = "i8432 answer from the handed-off watcher";
    let frame = [
        serde_json::json!({"type":"user", "timestamp":at, "message":{"role":"user","content":"question"}}),
        serde_json::json!({"type":"assistant", "timestamp":at, "message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":body}]}}),
        serde_json::json!({"type":"system", "timestamp":at, "subtype":"stop_hook_summary"}),
        serde_json::json!({"type":"system", "timestamp":at, "subtype":"turn_duration"}),
    ]
    .map(|line| format!("{line}\n"))
    .concat();
    h.append(frame.as_bytes());
    h.until("soft terminal relay decision", |h| !h.frames().is_empty())
        .await;
    assert!(
        h.events("relay flight recorder").iter().any(|line| {
            field(line, "terminal_kind").as_deref() == Some("soft_stop_hook_summary")
        })
    );
    let frames = h.frames();
    let route = if authorized {
        "watcher_direct"
    } else {
        "soft_terminal_no_authority:turn_start_outside_frame"
    };
    assert_eq!(frames[0].0, start, "{hosted:?}: {frames:?}");
    assert_eq!(frames[0].2, route, "{hosted:?}: {frames:?}");
    if !authorized {
        h.cancel();
        h.exited("watcher exit after authority denial").await;
        assert!(!h.showing(body));
        assert_eq!(receipt(pool, restored.user_msg_id, h.channel).await, None);
        return;
    }
    h.drained("terminal frame").await;
    h.until("reply and terminal retirement", |h| {
        h.showing(body) && h.row().is_none()
    })
    .await;
    assert_eq!(h.observe(&[body]).copies[0].len(), 1);
    assert!(
        h.shared
            .mailbox(h.channel)
            .snapshot()
            .await
            .cancel_token
            .is_none()
    );
    assert!(
        !h.shared
            .turn_finalizer
            .has_live_watcher_pending(h.channel, generation)
            .await
    );
    assert_eq!(
        receipt(pool, restored.user_msg_id, h.channel).await,
        Some((vec![origin], "completed".to_string()))
    );
    h.cancel();
    h.exited("watcher exit").await;
}

async fn receipt(
    pool: &sqlx::PgPool,
    user_msg_id: u64,
    channel: ChannelId,
) -> Option<(Vec<String>, String)> {
    sqlx::query_as(
        "SELECT consumed_origin_ids, terminal_outcome FROM turns
         WHERE turn_id = $1 AND terminal_outcome IS NOT NULL",
    )
    .bind(format!("discord:{}:{user_msg_id}", channel.get()))
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_watcher_records_nothing_and_the_ending_one_commits_before_the_row_goes_pg() {
    let test =
        "a_replaced_watcher_records_nothing_and_the_ending_one_commits_before_the_row_goes_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::on(75, &seed, Some(pool.clone())).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let placeholder = crate::services::discord::http::send_channel_message(&h.http, h.channel, "…")
        .await
        .unwrap()
        .id;
    let mut row = h.row_at(f);
    row.turn_source = TurnSource::Managed;
    row.user_msg_id = 7_340_000_101;
    row.current_msg_id = placeholder.get();
    row.input_origin_ids = vec![ORIGIN.to_string()];
    h.save(&row);

    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&row, &h.tmux, true);
    h.spawn_restoring(f, restored);
    h.cancel();
    h.exited("replaced watcher exit").await;
    assert_eq!(
        receipt(&pool, row.user_msg_id, h.channel).await,
        None,
        "custody moved, no ending"
    );
    assert!(h.row().is_some(), "the row stays for the next watcher");

    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&row, &h.tmux, true);
    h.spawn_restoring(f, restored);
    h.append(format!("{}{}{}", user("T1"), said(BODY), stop()).as_bytes());
    h.drained("terminal frame").await;
    assert!(h.showing(BODY), "the watcher delivered the answer");
    assert!(h.row().is_none(), "the watcher ended the execution");
    assert_eq!(
        receipt(&pool, row.user_msg_id, h.channel).await,
        Some((vec![ORIGIN.to_string()], "completed".to_string()))
    );
    h.cancel();
    h.exited("watcher exit").await;
    pool.close().await;
    db.drop().await;
}

/// While the receipt cannot commit, neither the pre-panel release nor the epilogue removes the
/// ended row: it stays marked with the watcher's terminal for boot or the next admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watcher_ending_whose_receipt_cannot_commit_keeps_the_row_marked() {
    let test = "a_watcher_ending_whose_receipt_cannot_commit_keeps_the_row_marked";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let blocked = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(50))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/blocked")
        .unwrap();
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::on(76, &seed, Some(blocked)).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let placeholder = crate::services::discord::http::send_channel_message(&h.http, h.channel, "…")
        .await
        .unwrap()
        .id;
    let mut row = h.row_at(f);
    row.turn_source = TurnSource::Managed;
    row.user_msg_id = 7_340_000_102;
    row.current_msg_id = placeholder.get();
    row.input_origin_ids = vec![ORIGIN.to_string()];
    h.save(&row);
    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&row, &h.tmux, true);
    h.spawn_restoring(f, restored);
    h.append(format!("{}{}{}", user("T1"), said(BODY), stop()).as_bytes());
    h.drained("terminal frame").await;
    assert!(h.showing(BODY), "the watcher delivered the answer");
    let kept = h.row().expect("the row stays while its receipt is owed");
    assert_eq!(
        kept.terminal_receipt.map(|mark| mark.outcome),
        Some(crate::db::turns::TerminalOutcome::Completed)
    );
    h.cancel();
    h.exited("watcher exit").await;
}

/// A managed turn mid-stream on a fresh harness, its mailbox turn started; `rebind` keeps the row
/// through any clear.
async fn streaming_turn(
    case: u64,
    pool: &sqlx::PgPool,
    rebind: bool,
) -> (Harness, InflightTurnState) {
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::on(case, &seed, Some(pool.clone())).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let mut row = h.row_at(f);
    row.turn_source = TurnSource::Managed;
    row.user_msg_id = 7_340_000_100 + case;
    row.input_origin_ids = vec![ORIGIN.to_string()];
    row.rebind_origin = rebind;
    h.save(&row);
    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&row, &h.tmux, true);
    h.spawn_restoring(f, restored);
    h.append(format!("{}{}", user("T1"), said(BODY)).as_bytes());
    h.until("streaming preview", |h| h.showing(BODY)).await;
    let token = Arc::new(crate::services::provider::CancelToken::new());
    token.bind_claude_tmux_session(&h.tmux);
    let user_msg = serenity::MessageId::new(row.user_msg_id);
    let author = serenity::UserId::new(7);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(&h.shared, h.channel, token, author, user_msg).await);
    (h, row)
}

/// Points the session's runtime output, where a stop reads its offset, at the transcript.
fn link_runtime_output(h: &Harness) {
    let runtime = crate::services::tmux_common::session_temp_path(&h.tmux, "jsonl");
    std::fs::hard_link(&h.path, runtime).unwrap();
}

/// Force-kills the harness turn on its own task and returns the recorded stop's offset; the pane
/// dies once that stop exists, as the provider exits on its interrupt, within the stop's wait.
async fn stop_then_die(h: &Harness) -> (tokio::task::JoinHandle<()>, Option<u64>) {
    let stop = force_stop(h).await;
    let offset = stop_recorded(h).await;
    h.pane("dead");
    (stop, offset)
}

/// The harness turn's runtime force stop, on its own task.
async fn force_stop(h: &Harness) -> tokio::task::JoinHandle<()> {
    let registry = crate::services::discord::health::HealthRegistry::new();
    registry
        .register("claude".to_string(), h.shared.clone())
        .await;
    let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
        provider: Some(CLAUDE),
        channel_id: Some(h.channel),
        tmux_name: h.tmux.clone(),
    };
    tokio::spawn(async move {
        let kill = crate::services::turn_lifecycle::force_kill_turn;
        kill(Some(&registry), &target, "i7340 stop", "i7340_test").await;
    })
}

/// Waits for the channel's stop record and returns its output offset.
async fn stop_recorded(h: &Harness) -> Option<u64> {
    let recorded = crate::services::discord::tmux::recent_turn_stop_for_channel;
    h.until("the stop record", |h| recorded(h.channel).is_some())
        .await;
    recorded(h.channel).and_then(|stop| stop.stop_output_offset)
}

/// A force stop's pane death is judged by the watcher, which consumes the stop record: its exit
/// records that judgment, cancelled, before the row goes. A rebind row keeps its row and no receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_pane_death_ends_cancelled_and_a_rebind_row_ends_nothing_pg() {
    let test = "a_stopped_pane_death_ends_cancelled_and_a_rebind_row_ends_nothing_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (case, rebind) in [(77, false), (78, true)] {
        let (mut h, row) = streaming_turn(case, &pool, rebind).await;
        let (stop, _) = stop_then_die(&h).await;
        h.exited("death after the stop").await;
        assert!(
            !stop.is_finished(),
            "case {case}: the watcher exit ran first"
        );
        let ended = receipt(&pool, row.user_msg_id, h.channel).await;
        if rebind {
            assert_eq!(ended, None, "a rebind row is not ended here");
            assert!(h.row().is_some(), "a rebind row stays");
        } else {
            let cancelled = (vec![ORIGIN.to_string()], "cancelled".to_string());
            assert_eq!(ended, Some(cancelled));
            assert!(h.row().is_none(), "the dead pane's turn ended");
        }
        stop.await.unwrap();
    }
    pool.close().await;
    db.drop().await;
}

/// A watcher that itself delivered T0 stays attached and takes T1 over; a force stop of T1 and
/// the pane death it brings end T1 cancelled, whatever T0's delivery left on that watcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_watcher_ends_the_next_stopped_turn_cancelled_pg() {
    let test = "a_reused_watcher_ends_the_next_stopped_turn_cancelled_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::on(79, &seed, Some(pool.clone())).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    link_runtime_output(&h);
    let mut first = h.row_at(f);
    first.turn_source = TurnSource::Managed;
    first.user_msg_id = 7_340_000_179;
    first.input_origin_ids = vec![ORIGIN.to_string()];
    h.save(&first);
    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&first, &h.tmux, true);
    h.spawn_restoring(f, restored);
    h.append(format!("{}{}{}", user("T0b"), said(BODY), stop()).as_bytes());
    h.drained("T0 terminal frame").await;
    assert!(h.row().is_none(), "the watcher ended T0 itself");
    let completed = Some((vec![ORIGIN.to_string()], "completed".to_string()));
    assert_eq!(
        receipt(&pool, first.user_msg_id, h.channel).await,
        completed
    );
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("still attached", |h| h.heartbeat() > seen).await;
    assert!(!h.watcher_finished(), "a live pane keeps the watcher");

    // T1 is handed to the same watcher, as a reuse resumes it at the new turn's start.
    let start = h.len();
    let mut next = h.row_at(start);
    next.turn_source = TurnSource::Managed;
    next.user_msg_id = 7_340_000_279;
    next.input_origin_ids = vec!["G-watcher-next".to_string()];
    h.save(&next);
    h.resume(start);
    let handle = h.shared.tmux_watchers.get(&h.channel).unwrap();
    handle.turn_delivered.store(false, Ordering::Relaxed);
    drop(handle);
    // No assistant text yet: a stop before T1's first output leaves only the death to end it.
    h.append(user("T1").as_bytes());
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("T1 read", |h| h.heartbeat() > seen).await;
    let token = Arc::new(crate::services::provider::CancelToken::new());
    token.bind_claude_tmux_session(&h.tmux);
    let author = serenity::UserId::new(7);
    let user_msg = serenity::MessageId::new(next.user_msg_id);
    let begin = crate::services::discord::mailbox_try_start_turn;
    assert!(begin(&h.shared, h.channel, token, author, user_msg).await);
    let (stop, stopped_at) = stop_then_die(&h).await;
    assert_eq!(
        stopped_at,
        Some(h.len()),
        "the stop ends T1's range past its start"
    );
    h.exited("T1 death after the stop").await;
    let cancelled = Some((vec!["G-watcher-next".to_string()], "cancelled".to_string()));
    assert_eq!(receipt(&pool, next.user_msg_id, h.channel).await, cancelled);
    assert!(h.row().is_none(), "the dead pane's turn ended");
    assert_eq!(
        receipt(&pool, first.user_msg_id, h.channel).await,
        completed
    );
    stop.await.unwrap();
    pool.close().await;
    db.drop().await;
}

/// A stop of the execution the mailbox runs, before its first byte: the stop's offset is that
/// execution's start, inside no output range, and its pane death still ends it cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_before_the_first_byte_ends_the_execution_cancelled_pg() {
    let test = "a_stop_before_the_first_byte_ends_the_execution_cancelled_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    // (case, the stop): the runtime force stop, which commits its own receipt first, or a mailbox
    // cancel, whose live token is all the watcher reads.
    for (case, via) in [(83, "force"), (89, "mailbox")] {
        let (mut h, row) = zero_byte_turn(case, &pool).await;
        let stop = match via {
            "force" => Some(force_stop(&h).await),
            _ => {
                let cancel = crate::services::discord::mailbox_cancel_active_turn;
                assert!(cancel(&h.shared, h.channel).await.token.is_some());
                None
            }
        };
        let stopped_at = stop_recorded(&h).await;
        assert_eq!(
            stopped_at, row.turn_start_offset,
            "case {case}: no byte since the start"
        );
        h.pane("dead");
        h.exited("death after the stop").await;
        let cancelled = Some((vec![ORIGIN.to_string()], "cancelled".to_string()));
        let ended = receipt(&pool, row.user_msg_id, h.channel).await;
        assert_eq!(ended, cancelled, "case {case} ({via})");
        assert!(h.row().is_none(), "case {case}: the dead pane's turn ended");
        if let Some(stop) = stop {
            stop.await.unwrap();
        }
    }
    pool.close().await;
    db.drop().await;
}

/// A managed execution at EOF with no byte of its own, watched and running as the mailbox turn on
/// a token of its own nonce.
async fn zero_byte_turn(case: u64, pool: &sqlx::PgPool) -> (Harness, InflightTurnState) {
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::on(case, &seed, Some(pool.clone())).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    link_runtime_output(&h);
    let mut row = h.row_at(f);
    row.turn_source = TurnSource::Managed;
    row.user_msg_id = 7_340_000_100 + case;
    row.input_origin_ids = vec![ORIGIN.to_string()];
    h.save(&row);
    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&row, &h.tmux, true);
    h.spawn_restoring(f, restored);
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("watching", |h| h.heartbeat() > seen).await;
    assert!(row.turn_nonce.is_some(), "the row names its execution");
    start_live_turn(&h, &row, row.turn_nonce.clone()).await;
    (h, row)
}

/// The runtime stop commits its cancel before its kill; the bridge's finalizer release of the token,
/// the paused watcher's tokenless death judgment and a queued turn leave that one receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_released_by_its_bridge_before_the_pane_dies_stays_cancelled_pg() {
    let test = "a_stop_released_by_its_bridge_before_the_pane_dies_stays_cancelled_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (mut h, row) = zero_byte_turn(87, &pool).await;
    let handle = h.shared.tmux_watchers.get(&h.channel).unwrap();
    handle.paused.store(true, Ordering::Release);
    drop(handle);
    let stop = force_stop(&h).await;
    let stopped_at = stop_recorded(&h).await;
    assert_eq!(stopped_at, row.turn_start_offset, "no byte since the start");
    // The stop's receipt, if it commits before its kill, is in place before the bridge's release.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
    while receipt(&pool, row.user_msg_id, h.channel).await.is_none()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let key = crate::services::discord::turn_finalizer::TurnKey::new(
        h.channel,
        row.effective_finalizer_turn_id(),
        h.shared.restart.current_generation,
    );
    let claim = crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row(&row);
    let _ = h
        .shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            key,
            CLAUDE,
            crate::services::discord::turn_finalizer::TerminalEvent::Cancel,
            crate::services::discord::turn_finalizer::FinalizeContext::bridge(),
            Some(claim),
            h.shared.clone(),
        )
        .await;
    let snapshot = h.shared.mailbox(h.channel).snapshot().await;
    assert!(
        snapshot.cancel_token.is_none(),
        "the bridge released the token"
    );
    assert!(h.row().is_some(), "the bridge left the row to its reader");
    let queued = Arc::new(crate::services::provider::CancelToken::new());
    let author = serenity::UserId::new(7);
    let next = serenity::MessageId::new(row.user_msg_id + 1);
    let begin = crate::services::discord::mailbox_try_start_turn;
    assert!(begin(&h.shared, h.channel, queued, author, next).await);
    h.pane("dead");
    h.exited("death after the release").await;
    stop.await.unwrap();
    let cancelled = Some((vec![ORIGIN.to_string()], "cancelled".to_string()));
    assert_eq!(receipt(&pool, row.user_msg_id, h.channel).await, cancelled);
    assert!(h.row().is_none(), "the dead pane's turn ended");
    pool.close().await;
    db.drop().await;
}

/// A stop that hands the execution over, keeping its session, commits no receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hand_over_stop_records_no_receipt_pg() {
    let test = "a_hand_over_stop_records_no_receipt_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (h, row) = zero_byte_turn(88, &pool).await;
    let policy = crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
        restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
    };
    let stop = crate::services::discord::health::stop_channel_runtime;
    stop(
        &h.shared,
        &CLAUDE,
        h.channel,
        "i7340 hand-over",
        policy,
        None,
    )
    .await;
    assert_eq!(receipt(&pool, row.user_msg_id, h.channel).await, None);
    pool.close().await;
    db.drop().await;
}

/// Starts `row`'s mailbox turn on a token of `nonce`.
async fn start_live_turn(
    h: &Harness,
    row: &InflightTurnState,
    nonce: Option<String>,
) -> Arc<crate::services::provider::CancelToken> {
    let token = crate::services::provider::CancelToken::from_persisted_turn_nonce(nonce);
    let token = Arc::new(token);
    token.bind_claude_tmux_session(&h.tmux);
    let author = serenity::UserId::new(7);
    let user_msg = serenity::MessageId::new(row.user_msg_id);
    let begin = crate::services::discord::mailbox_try_start_turn;
    assert!(begin(&h.shared, h.channel, token.clone(), author, user_msg).await);
    token
}

/// A stop left at the previous turn's end is outside the next execution's range, and no live
/// turn shows a stop of that execution: its pane death without a stop of its own ends it failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prior_turns_stop_leaves_the_next_turns_pane_death_failed_pg() {
    let test = "a_prior_turns_stop_leaves_the_next_turns_pane_death_failed_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    // (case, the mailbox turn): none, the execution's own still running, a stopped turn of
    // another nonce, or the execution's own whose cancel flag is a completion cleanup's.
    for (case, live) in [
        (80, "none"),
        (84, "running"),
        (85, "other"),
        (86, "cleanup"),
    ] {
        let seed = format!("{}{}{}", user("T0"), said(T0), stop());
        let mut h = Harness::on(case, &seed, Some(pool.clone())).await;
        let f = seed.len() as u64;
        h.commit(0, f);
        link_runtime_output(&h);
        let record = crate::services::discord::tmux::record_recent_turn_stop;
        record(h.channel, Some(&h.tmux), "i7340 T0 stop").await;
        let mut next = h.row_at(f);
        next.turn_source = TurnSource::Managed;
        next.user_msg_id = 7_340_000_200 + case;
        next.input_origin_ids = vec![ORIGIN.to_string()];
        h.save(&next);
        let own = next.turn_nonce.clone();
        match live {
            "running" => drop(start_live_turn(&h, &next, own).await),
            "other" => {
                let other = Some("i7340-other-nonce".to_string());
                start_live_turn(&h, &next, other)
                    .await
                    .publish_cancel("i7340 stop");
            }
            "cleanup" => {
                let token = start_live_turn(&h, &next, own).await;
                token.mark_completion_cleanup();
                token.publish_cancel("tmux_cleanup");
            }
            _ => {}
        }
        let restored = crate::services::discord::tmux::restored_watcher_turn_from_inflight(
            &next, &h.tmux, true,
        );
        h.spawn_restoring(f, restored);
        h.append(format!("{}{}", user("T1"), said(BODY)).as_bytes());
        h.until("streaming preview", |h| h.showing(BODY)).await;
        h.pane("dead");
        h.exited("death without a stop of its own").await;
        let failed = Some((vec![ORIGIN.to_string()], "failed".to_string()));
        let ended = receipt(&pool, next.user_msg_id, h.channel).await;
        assert_eq!(ended, failed, "case {case} ({live})");
        assert!(h.row().is_none(), "case {case}: the dead pane's turn ended");
    }
    pool.close().await;
    db.drop().await;
}

/// A Herdr ending on the real loop, its row managed with a started mailbox turn; `storage` is the
/// watcher's pool, `pool` holds the channel's hosted record.
async fn herdr_ending(
    case: u64,
    storage: sqlx::PgPool,
    pool: &sqlx::PgPool,
    herdr: crate::services::discord::inflight::herdr_turn_evidence_tests::HerdrCase,
) -> (
    Harness,
    InflightTurnState,
    Arc<crate::services::provider::CancelToken>,
) {
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::on(case, &seed, Some(storage)).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let mut row = h.row_at(f);
    row.turn_source = TurnSource::Managed;
    row.user_msg_id = 8_399_000_100 + case;
    row.input_origin_ids = vec![ORIGIN.to_string()];
    let source = std::path::PathBuf::from(format!("{}-herdr", h.path));
    let mark = crate::services::discord::inflight::herdr_turn_evidence_tests::mark_herdr;
    mark(pool, &h.shared.token_hash, &mut row, &source, herdr).await;
    h.save(&row);
    let token = Arc::new(crate::services::provider::CancelToken::new());
    token.bind_claude_tmux_session(&h.tmux);
    let (author, user_msg) = (
        serenity::UserId::new(7),
        serenity::MessageId::new(row.user_msg_id),
    );
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(&h.shared, h.channel, token.clone(), author, user_msg).await);
    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&row, &h.tmux, true);
    h.spawn_restoring(f, restored);
    h.append(format!("{}{}{}", user("T1"), said(BODY), stop()).as_bytes());
    h.drained("terminal frame").await;
    assert!(h.showing(BODY), "the watcher delivered the answer");
    (h, row, token)
}

/// A Herdr ending whose own end is unproven records no receipt and leaves no mark, so neither
/// boot nor the next admission removes its row, mailbox or token; its own end commits once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_herdr_ending_without_its_own_end_issues_no_receipt_pg() {
    use crate::services::discord::inflight::herdr_turn_evidence_tests::{HELD, OWN};
    let test = "a_herdr_ending_without_its_own_end_issues_no_receipt_pg";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let blocked = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(50))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/blocked")
        .unwrap();
    for (n, (storage, herdr)) in [(&pool, HELD[0]), (&blocked, HELD[0]), (&pool, OWN)]
        .into_iter()
        .enumerate()
    {
        let (mut h, row, token) = herdr_ending(90 + n as u64, storage.clone(), &pool, herdr).await;
        let kept = h.row().filter(|kept| kept.user_msg_id == row.user_msg_id);
        let receipt = receipt(&pool, row.user_msg_id, h.channel).await;
        let fresh =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let settle = crate::services::discord::receipt_settle::settle_marked_row;
        if herdr == OWN {
            assert_eq!(receipt, Some((vec![ORIGIN.into()], "completed".into())));
            assert!(kept.is_none(), "its own end settles the row");
        } else {
            let kept = kept.expect("the held row stays");
            assert_eq!((receipt, kept.terminal_receipt), (None, None), "case {n}");
            let active = crate::services::discord::mailbox_snapshot(&h.shared, h.channel).await;
            assert_eq!(
                active.active_user_message_id,
                Some(serenity::MessageId::new(row.user_msg_id))
            );
            assert!(!settle(&fresh, &CLAUDE, h.channel.get()).await, "case {n}");
            crate::services::discord::receipt_settle::settle_boot(&fresh, vec![kept]).await;
            assert!(h.row().is_some() && !token.cancelled.load(Ordering::Relaxed));
        }
        h.cancel();
        h.exited("watcher exit").await;
    }
    pool.close().await;
    db.drop().await;
}

mod committed_placeholder_cleanup {
    use crate::services::discord::outbound::delivery_record as dr;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::Arc;

    /// After an edit failure on a committed range, the placeholder is deleted only when another
    /// message is proven to hold the delivered body; otherwise it and its tracking stay.
    #[test]
    fn committed_edit_failure_deletes_placeholder_only_with_delivered_elsewhere_proof() {
        use crate::services::discord::tmux::tmux_watcher::terminal_long_chunks::{
            GuardedWatcherDeliveryResult, record_watcher_terminal_delivery, watcher_delivery_identity,
        };
        use crate::services::discord::tmux::tmux_watcher::terminal_send::committed_placeholder_cleanup::{
            CommittedEditFailureReconcileCtx, reconcile_already_committed_after_edit_failure,
        };
        use crate::services::discord::health::test_support::MockDiscord;
        use adk_test_support::test_support::wait::{WAIT_CAP, await_within};
        let root = tempfile::tempdir().expect("runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(root.path());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        for (index, delivered_elsewhere) in [(0u64, true), (1, false)] {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(4_508_100 + index);
            let (placeholder, delivered) = (MessageId::new(4_508_200), 4_508_300);
            let tmux = format!("committed-edit-failure-{index}");
            let generation = crate::services::tmux_common::session_temp_path(&tmux, "generation");
            std::fs::create_dir_all(std::path::Path::new(&generation).parent().unwrap()).unwrap();
            std::fs::write(&generation, b"1").unwrap();
            let transcript = root.path().join(format!("{tmux}.jsonl"));
            std::fs::File::create(&transcript)
                .and_then(|file| file.set_len(1_000))
                .expect("transcript");
            shared.tmux_watchers.insert(
                channel,
                crate::services::discord::TmuxWatcherHandle {
                    tmux_session_name: tmux.clone(),
                    output_path: transcript.to_string_lossy().into_owned(),
                    paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    resume_offset: Arc::new(std::sync::Mutex::new(None)),
                    cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                        crate::services::discord::tmux_watcher_now_ms(),
                    )),
                },
            );
            if delivered_elsewhere {
                let identity = watcher_delivery_identity(
                    dr::current_generation_mtime_ns(&tmux),
                    shared.relay_frontier_token(channel).reset_incarnation,
                    None,
                );
                let recorded = record_watcher_terminal_delivery(
                    crate::services::discord::tmux::WatcherDeliveryTarget {
                        shared: &shared,
                        provider: &ProviderKind::Claude,
                        channel_id: channel,
                        tmux_session_name: &tmux,
                    },
                    identity,
                    (100, 300),
                    Some(delivered),
                    "delivered body",
                );
                assert_eq!(recorded, GuardedWatcherDeliveryResult::Persisted);
            }
            let mut placeholder_msg_id = Some(placeholder);
            let (mut direct_send_delivered, mut anchor_visible, mut restored) = (false, true, true);
            let mut last_edit_text = String::new();
            let calls = rt.block_on(async {
                let discord = MockDiscord::start().await;
                await_within(
                    reconcile_already_committed_after_edit_failure(
                        CommittedEditFailureReconcileCtx {
                            http: &discord.http,
                            shared: &shared,
                            provider: &ProviderKind::Claude,
                            channel_id: channel,
                            tmux_session_name: &tmux,
                            msg_id: placeholder,
                            inflight_before_relay: None,
                            range: (150, 300),
                            response_sent_offset: 0,
                            edit_error: "500 edit failed".to_string(),
                            direct_send_delivered: &mut direct_send_delivered,
                            tui_direct_anchor_terminal_body_visible: &mut anchor_visible,
                            placeholder_msg_id: &mut placeholder_msg_id,
                            placeholder_from_restored_inflight: &mut restored,
                            last_edit_text: &mut last_edit_text,
                            cleanup_source: "committed_edit_failure_test",
                            record_source: "committed_edit_failure_test",
                        },
                    ),
                    WAIT_CAP,
                )
                .await;
                discord.calls_for(channel.get())
            });

            let deletes: Vec<&String> = calls.iter().filter(|c| c.starts_with("DELETE")).collect();
            let expected_delete =
                format!("DELETE /api/v10/channels/{channel}/messages/{placeholder}");
            assert_eq!(
                deletes,
                if delivered_elsewhere {
                    vec![&expected_delete]
                } else {
                    Vec::new()
                },
                "delivered_elsewhere={delivered_elsewhere}"
            );
            assert_eq!(
                (placeholder_msg_id, restored),
                if delivered_elsewhere {
                    (None, false)
                } else {
                    (Some(placeholder), true)
                },
                "tracking clears only after the delete commits"
            );
        }
    }
}
