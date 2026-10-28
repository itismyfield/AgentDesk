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

/// Force-kills the harness turn on its own task; the pane dies once the stop is recorded, as the
/// provider exits on its interrupt, so the watcher judges the death within the stop's wait.
async fn stop_then_die(h: &Harness, start: u64) -> tokio::task::JoinHandle<()> {
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
    let recorded = crate::services::discord::tmux::recent_turn_stop_for_watcher_range;
    h.until("the stop record", |h| {
        recorded(h.channel, &h.tmux, start).is_some()
    })
    .await;
    h.pane("dead");
    stop
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
        let start = row.turn_start_offset.unwrap_or(0);
        let stop = stop_then_die(&h, start).await;
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
