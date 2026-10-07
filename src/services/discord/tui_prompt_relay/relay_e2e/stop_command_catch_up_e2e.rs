//! A `!stop` that interrupts an active turn posts no reply, so only the live
//! checkpoint keeps the next catch-up sweep from replaying it as a prompt.

use std::time::Duration;

use super::RelayE2eHarness;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_that_interrupts_a_turn_is_not_replayed_by_catch_up() {
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();

    let base = recent_snowflake_base();
    let (answered, active, stop, missed) = (base | 1, base | 2, base | 3, base | 4);
    harness.seed_channel_history(&[
        (answered, "bot answer", true),
        (stop, "!stop", false),
        (missed, "question the gateway dropped", false),
    ]);
    let _active = harness
        .spawn_turn_held_at_placeholder(active, "long running work", Duration::from_secs(2))
        .await;
    let interrupted = harness
        .mailbox()
        .await
        .cancel_token
        .expect("an active turn");

    harness
        .deliver_user_message(stop, "!stop")
        .await
        .expect("live intake handles the command");
    assert!(
        interrupted
            .cancelled
            .load(std::sync::atomic::Ordering::SeqCst),
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

    harness.run_catch_up().await;

    assert_eq!(
        queued_ids(&harness).await,
        vec![missed],
        "catch-up must recover the dropped question and never the consumed stop"
    );
    assert!(
        harness.unhandled_requests().is_empty(),
        "{:?}",
        harness.unhandled_requests()
    );
}
