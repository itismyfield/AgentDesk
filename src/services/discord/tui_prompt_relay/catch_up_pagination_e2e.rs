//! Phase-1 `catch_up` over the relay e2e mock: one unanswered user message in a
//! channel of bot posts is only recovered if the mock pages from the cursor.

use super::relay_e2e::{HistoryQuery, RelayE2eHarness};

/// A snowflake base minted 30s ago, inside `catch_up`'s five-minute age window;
/// the low 22 bits are free for a per-message sequence.
fn recent_snowflake_base() -> u64 {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let discord_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    u64::try_from(discord_ms).expect("after Discord epoch") << 22
}

/// Starts a role-map-bound channel holding 120 posts, oldest first in the
/// returned ids, all from the bot except the one at `user_index`.
async fn harness_with_history(user_index: usize) -> (RelayE2eHarness, Vec<u64>) {
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();
    let base = recent_snowflake_base();
    let ids: Vec<u64> = (1..=120).map(|sequence| base | sequence).collect();
    let history: Vec<(u64, &str, bool)> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            if index == user_index {
                (*id, "unanswered question", false)
            } else {
                (*id, "bot noise", true)
            }
        })
        .collect();
    harness.seed_channel_history(&history);
    (harness, ids)
}

fn query(limit: usize, before: Option<u64>, after: Option<u64>) -> HistoryQuery {
    HistoryQuery {
        limit: Some(limit),
        before,
        after,
    }
}

async fn queued_message_ids(harness: &RelayE2eHarness) -> Vec<u64> {
    let mailbox = harness.mailbox().await;
    mailbox
        .intervention_queue
        .iter()
        .map(|intervention| intervention.message_id.get())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recent_catch_up_pages_back_to_a_buried_user_message_and_stops() {
    let (harness, ids) = harness_with_history(0).await;

    harness.run_catch_up().await;

    assert_eq!(
        harness.history_queries(),
        vec![
            query(50, None, None),
            query(50, Some(ids[70]), None),
            query(50, Some(ids[20]), None),
            query(50, Some(ids[0]), None),
            query(20, None, None),
        ],
        "phase 1 must page back from each page's oldest id until an empty page, then phase 2 reads once"
    );
    assert_eq!(queued_message_ids(&harness).await, vec![ids[0]]);
    assert_eq!(harness.checkpoint(), Some(ids[119]));
    assert!(
        harness.unhandled_requests().is_empty(),
        "{:?}",
        harness.unhandled_requests()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checkpointed_catch_up_reads_the_page_just_past_the_cursor() {
    let (harness, ids) = harness_with_history(30).await;
    harness
        .shared
        .last_message_ids
        .insert(harness.channel_id, ids[10]);

    harness.run_catch_up().await;

    assert_eq!(
        harness.history_queries(),
        vec![query(50, None, Some(ids[10])), query(20, None, None)]
    );
    assert_eq!(queued_message_ids(&harness).await, vec![ids[30]]);
    assert_eq!(
        harness.checkpoint(),
        Some(ids[60]),
        "the checkpoint advances only through the page actually read"
    );
    assert!(
        harness.unhandled_requests().is_empty(),
        "{:?}",
        harness.unhandled_requests()
    );
}
