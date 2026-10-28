//! A restored watcher completing a scheduled snapshot turn on the real loop.

use super::*;
use crate::services::discord::inflight::TurnSource;
use crate::services::discord::scheduled_turn::{SKIP_FALLBACK_SETTLE, ScheduledTurnMark};
use crate::services::scheduled_messages::turn_completion::tests::{
    delivery_state, launch_agent_delivery, poll_once,
};

const T0: &str = "I7420 T0 delivered before the restart";
const BODY: &str = "I7420 scheduled snapshot answer from the restored watcher";
const TURN: &str = "i7420-restored-watcher-turn";

/// The watcher records the outcome before its pre-panel release drops the Managed row;
/// with Path B's late record cut away (a crash after the release), the next poll still
/// settles the same delivery and a restart resends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restored_watcher_records_the_scheduled_outcome_before_its_release_pg() {
    let test = "a_restored_watcher_records_the_scheduled_outcome_before_its_release_pg";
    if !isolated_in("scheduled_watcher_harness_tests", test, &[]) {
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    launch_agent_delivery(&pool, "i7420-watcher", TURN, true, "fail").await;
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::on(74, &seed, Some(pool.clone())).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let placeholder = crate::services::discord::http::send_channel_message(&h.http, h.channel, "…")
        .await
        .unwrap()
        .id;
    let mut row = h.row_at(f);
    row.turn_source = TurnSource::Managed;
    row.user_msg_id = 7_420_000_001;
    row.current_msg_id = placeholder.get();
    row.scheduled = ScheduledTurnMark::Scheduled {
        turn_id: TURN.to_string(),
        isolated: true,
        outcome: None,
    };
    h.save(&row);
    let restored =
        crate::services::discord::tmux::restored_watcher_turn_from_inflight(&row, &h.tmux, true);
    assert!(
        restored.is_some(),
        "the scheduled row restores its watcher turn"
    );
    SKIP_FALLBACK_SETTLE.store(true, Ordering::SeqCst);
    h.spawn_restoring(f, restored);
    h.append(format!("{}{}{}", user("T1"), said(BODY), stop()).as_bytes());
    h.drained("terminal frame").await;

    let removed = h.events("inflight state row removal");
    eprintln!("i7420 removals: {removed:?}");
    assert!(h.showing(BODY), "the restored watcher delivered the answer");
    assert!(
        h.row().is_none(),
        "the completion dropped the scheduled row"
    );
    assert_eq!(
        delivery_state(&pool, TURN).await,
        ("running".to_string(), Some("delivered".to_string())),
        "the outcome was recorded before the row was dropped"
    );
    let posts_before_restart = h.discord.lock().unwrap().posts;
    h.take_tmux_calls();
    h.cancel();
    h.exited("watcher exit").await;

    // Restart: a new runtime finds no row to restore and the next poll settles the delivery.
    let fresh =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    crate::services::discord::recovery_engine::restore_inflight_turns(&h.http, &fresh, &CLAUDE)
        .await;
    assert!(
        poll_once(&pool).await,
        "the poll settles the recorded outcome"
    );
    assert_eq!(delivery_state(&pool, TURN).await.0, "sent");
    assert_eq!(
        h.discord.lock().unwrap().posts,
        posts_before_restart,
        "nothing resent"
    );
    let launches: Vec<_> = h
        .take_tmux_calls()
        .into_iter()
        .filter(|call| call.starts_with("new-session") || call.starts_with("send-keys"))
        .collect();
    assert_eq!(
        launches,
        Vec::<String>::new(),
        "no provider start after the restart"
    );
    pool.close().await;
    db.drop().await;
}
