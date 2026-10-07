//! A text command live intake already consumed must not come back as a catch-up
//! prompt, while every input it did not consume stays recoverable.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{AbortOnDrop, RelayE2eHarness};
use crate::services::discord::Error;
use crate::services::discord::catch_up::retry_state::arm_catch_up_retry_for_tests;
use crate::services::provider::CancelToken;

/// A snowflake base minted 30s ago, inside both catch-up age windows.
fn recent_snowflake_base() -> u64 {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let discord_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    u64::try_from(discord_ms).expect("after Discord epoch") << 22
}

/// Every source message the queue carries; consecutive inputs merge into one entry.
async fn queued_ids(harness: &RelayE2eHarness) -> Vec<u64> {
    let mailbox = harness.mailbox().await;
    let mut ids: Vec<u64> = mailbox
        .intervention_queue
        .iter()
        .flat_map(|intervention| {
            std::iter::once(intervention.message_id)
                .chain(intervention.source_message_ids.iter().copied())
        })
        .map(|id| id.get())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Holds a live turn on `active` at its placeholder POST, leaving the mailbox busy.
async fn hold_a_turn(
    harness: &RelayE2eHarness,
    active: u64,
) -> (AbortOnDrop<Result<(), Error>>, Arc<CancelToken>) {
    let held = harness
        .spawn_turn_held_at_placeholder(active, "long running work", Duration::from_secs(2))
        .await;
    let token = harness
        .mailbox()
        .await
        .cancel_token
        .expect("an active turn");
    (held, token)
}

/// Interrupts the held turn with a live `!stop`, which posts no reply.
async fn stop_the_turn(harness: &RelayE2eHarness, token: &CancelToken, stop: u64) {
    harness
        .deliver_user_message(stop, "!stop")
        .await
        .expect("live intake handles the command");
    assert!(
        token.cancelled.load(Ordering::SeqCst),
        "the command must interrupt the active turn"
    );
    assert!(
        !harness
            .messages()
            .iter()
            .any(|(reply_to, _)| *reply_to == Some(stop)),
        "an interrupting stop answers nothing: {:?}",
        harness.messages()
    );
}

fn assert_no_unhandled(harness: &RelayE2eHarness) {
    let unhandled = harness.unhandled_requests();
    assert!(unhandled.is_empty(), "{unhandled:?}");
}

/// Ids `answered < active < stop < missed`, history without any reply to the stop.
async fn stop_before_a_dropped_question() -> (
    RelayE2eHarness,
    AbortOnDrop<Result<(), Error>>,
    Arc<CancelToken>,
    [u64; 3],
) {
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();
    let base = recent_snowflake_base();
    let (answered, active, stop, missed) = (base | 1, base | 2, base | 3, base | 4);
    harness.seed_channel_history(&[
        (answered, "bot answer", true),
        (stop, "!stop", false),
        (missed, "question the gateway dropped", false),
    ]);
    let (held, token) = hold_a_turn(&harness, active).await;
    (harness, held, token, [active, stop, missed])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_that_interrupts_a_turn_is_not_replayed_by_catch_up() {
    let (harness, _held, token, [_, stop, missed]) = stop_before_a_dropped_question().await;
    stop_the_turn(&harness, &token, stop).await;

    harness.run_catch_up().await;

    assert_eq!(
        queued_ids(&harness).await,
        vec![missed],
        "catch-up must recover the dropped question and never the consumed stop"
    );
    assert_no_unhandled(&harness);
}

/// A retry cursor armed before the stop re-reads it; the consumed stop still
/// must not enqueue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_is_not_replayed_through_a_retry_cursor_armed_before_it() {
    let (harness, _held, token, [active, stop, missed]) = stop_before_a_dropped_question().await;
    arm_catch_up_retry_for_tests(&harness.shared, harness.channel_id, active);
    stop_the_turn(&harness, &token, stop).await;

    harness.run_catch_up().await;

    assert_eq!(queued_ids(&harness).await, vec![missed]);
    assert_no_unhandled(&harness);
}

/// A sweep that fetched its page before the stop was handled still must not
/// enqueue it once the page arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_handled_while_a_sweep_waits_on_its_page_is_not_replayed() {
    let (harness, _held, token, [_, stop, missed]) = stop_before_a_dropped_question().await;
    harness.hold_next_history();
    let sweep = tokio::spawn({
        let (http, shared) = (harness.ctx.http.clone(), harness.shared.clone());
        let provider = harness.data.provider.clone();
        async move {
            crate::services::discord::catch_up::catch_up_missed_messages(&http, &shared, &provider)
                .await
        }
    });
    assert!(
        harness.wait_for_held_history(Duration::from_secs(5)).await,
        "the sweep must reach its history fetch"
    );
    stop_the_turn(&harness, &token, stop).await;
    harness.release_held_history();
    sweep.await.expect("the sweep finishes");

    assert_eq!(queued_ids(&harness).await, vec![missed]);
    assert_no_unhandled(&harness);
}

/// Consuming a stop says nothing about older input the gateway dropped, a
/// question or an unanswered command alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_keeps_earlier_dropped_input_recoverable() {
    let mut lost = Vec::new();
    for dropped in ["question the gateway dropped", "!skill review"] {
        let harness = RelayE2eHarness::start().await;
        harness.register_channel_in_role_map();
        let base = recent_snowflake_base();
        let (answered, active, missed, stop) = (base | 1, base | 2, base | 3, base | 4);
        harness.seed_channel_history(&[
            (answered, "bot answer", true),
            (missed, dropped, false),
            (stop, "!stop", false),
        ]);
        let (_held, token) = hold_a_turn(&harness, active).await;
        stop_the_turn(&harness, &token, stop).await;

        harness.run_catch_up().await;

        let queued = queued_ids(&harness).await;
        if !queued.contains(&missed) {
            lost.push((dropped, queued));
        }
        assert_no_unhandled(&harness);
    }
    assert!(
        lost.is_empty(),
        "input before the stop must stay recoverable: {lost:?}"
    );
}

/// A command that fails after live intake took it is still consumed: its text
/// never becomes a prompt, and the question after it still recovers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clear_that_fails_is_not_replayed_by_catch_up() {
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();
    let base = recent_snowflake_base();
    let (answered, clear, missed) = (base | 1, base | 2, base | 3);
    harness.seed_channel_history(&[
        (answered, "bot answer", true),
        (clear, "!clear", false),
        (missed, "question the gateway dropped", false),
    ]);

    let failure = harness
        .deliver_user_message(clear, "!clear")
        .await
        .expect_err("without PostgreSQL the clear handler fails");
    assert!(
        failure
            .to_string()
            .contains("postgres pool is required to persist a channel clear boundary"),
        "{failure}"
    );

    harness.run_catch_up().await;

    assert_eq!(queued_ids(&harness).await, vec![missed]);
    assert_no_unhandled(&harness);
}
