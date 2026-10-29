//! A watcher ending a managed execution on the real loop commits its receipt before the row
//! goes; a watcher cancelled for a replacement hands the turn over and records nothing.

use super::*;
use crate::services::discord::inflight::TurnSource;

const T0: &str = "I7340 T0 delivered before the watcher started";
const BODY: &str = "I7340 answer the restored watcher relays";
const ORIGIN: &str = "G-watcher-origin";

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
    if !isolated_in("receipt_watcher_harness_tests", test, &[]) {
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
    if !isolated_in("receipt_watcher_harness_tests", test, &[]) {
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
    let registry = crate::services::discord::health::HealthRegistry::new();
    registry
        .register("claude".to_string(), h.shared.clone())
        .await;
    let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
        provider: Some(CLAUDE),
        channel_id: Some(h.channel),
        tmux_name: h.tmux.clone(),
    };
    let stop = tokio::spawn(async move {
        let kill = crate::services::turn_lifecycle::force_kill_turn;
        kill(Some(&registry), &target, "i7340 stop", "i7340_test").await;
    });
    let recorded = crate::services::discord::tmux::recent_turn_stop_for_channel;
    h.until("the stop record", |h| recorded(h.channel).is_some())
        .await;
    let offset = recorded(h.channel).and_then(|stop| stop.stop_output_offset);
    h.pane("dead");
    (stop, offset)
}

/// A force stop's pane death is judged by the watcher, which consumes the stop record: its exit
/// records that judgment, cancelled, before the row goes. A rebind row keeps its row and no receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_pane_death_ends_cancelled_and_a_rebind_row_ends_nothing_pg() {
    let test = "a_stopped_pane_death_ends_cancelled_and_a_rebind_row_ends_nothing_pg";
    if !isolated_in("receipt_watcher_harness_tests", test, &[]) {
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
    if !isolated_in("receipt_watcher_harness_tests", test, &[]) {
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

/// A runtime stop of the execution the mailbox runs, before its first byte: the stop's offset is
/// that execution's start, inside no output range, and its pane death still ends it cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_before_the_first_byte_ends_the_execution_cancelled_pg() {
    let test = "a_stop_before_the_first_byte_ends_the_execution_cancelled_pg";
    if !isolated_in("receipt_watcher_harness_tests", test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::on(83, &seed, Some(pool.clone())).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    link_runtime_output(&h);
    let mut row = h.row_at(f);
    row.turn_source = TurnSource::Managed;
    row.user_msg_id = 7_340_000_183;
    row.input_origin_ids = vec![ORIGIN.to_string()];
    h.save(&row);
    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&row, &h.tmux, true);
    h.spawn_restoring(f, restored);
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("watching", |h| h.heartbeat() > seen).await;
    assert!(row.turn_nonce.is_some(), "the row names its execution");
    start_live_turn(&h, &row, row.turn_nonce.clone()).await;
    let (stop, stopped_at) = stop_then_die(&h).await;
    assert_eq!(stopped_at, row.turn_start_offset, "no byte since the start");
    h.exited("death after the stop").await;
    let cancelled = Some((vec![ORIGIN.to_string()], "cancelled".to_string()));
    assert_eq!(receipt(&pool, row.user_msg_id, h.channel).await, cancelled);
    assert!(h.row().is_none(), "the dead pane's turn ended");
    stop.await.unwrap();
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
    if !isolated_in("receipt_watcher_harness_tests", test, &[]) {
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
