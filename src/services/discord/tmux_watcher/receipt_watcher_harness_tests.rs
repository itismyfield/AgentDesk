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
