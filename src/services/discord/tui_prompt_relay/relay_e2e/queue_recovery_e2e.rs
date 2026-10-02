//! Phase-2 `catch_up` over the relay e2e mock: a message that is only queued is
//! not recovered yet, so the checkpoint stays before it until a turn takes it.

use std::time::Duration;

use super::RelayE2eHarness;

const QUEUED_TEXT: &str = "queued question [queued-input]";
const FRESH_TEXT: &str = "fresh question [fresh-input]";

/// A snowflake base minted 30s ago, inside both catch-up age windows.
fn recent_snowflake_base() -> u64 {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let discord_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    u64::try_from(discord_ms).expect("after Discord epoch") << 22
}

/// Every source message the queue still carries; consecutive inputs merge into one entry.
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
async fn a_queued_message_holds_the_checkpoint_until_a_turn_dispatches_it() {
    let name = concat!(
        module_path!(),
        "::a_queued_message_holds_the_checkpoint_until_a_turn_dispatches_it"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();
    harness.cache_relay_transport();

    // Bot posts, then two user messages after the last bot reply: `queued` reaches
    // the mailbox through intake, `fresh` is only in history.
    let base = recent_snowflake_base();
    let ids: Vec<u64> = (1..=120).map(|sequence| base | sequence).collect();
    let (queued, fresh) = (ids[118], ids[119]);
    let history: Vec<(u64, &str, bool)> = ids
        .iter()
        .map(|id| match *id {
            id if id == queued => (id, QUEUED_TEXT, false),
            id if id == fresh => (id, FRESH_TEXT, false),
            id => (id, "bot noise", true),
        })
        .collect();
    harness.seed_channel_history(&history);

    let _active = harness
        .spawn_turn_held_at_placeholder(base | 500, "holds the mailbox", Duration::from_secs(2))
        .await;
    harness
        .deliver_user_message(queued, QUEUED_TEXT)
        .await
        .expect("intake queues behind the active turn");
    assert_eq!(queued_ids(&harness).await, vec![queued]);
    // A live cursor lagging the durable queue: phase 1 resumes past the newest
    // intake and reads nothing, so phase 2 alone meets both messages.
    harness
        .shared
        .last_message_ids
        .insert(harness.channel_id, ids[10]);

    harness.run_catch_up().await;

    assert_eq!(
        queued_ids(&harness).await,
        vec![queued, fresh],
        "phase 2 must recover the fresh message and keep the queued one"
    );
    let checkpoint = harness.checkpoint().expect("the rewound cursor stays set");
    assert!(
        checkpoint < queued,
        "queue membership is not dispatch; checkpoint {checkpoint} passed queued {queued}"
    );

    // The active turn ends; the queue must drain through real dispatch.
    harness.release_held_placeholder();
    let drained = super::wait_until(Duration::from_secs(15), {
        let shared = harness.shared.clone();
        let channel_id = harness.channel_id;
        let messages = harness.mock.messages.clone();
        move || {
            let shared = shared.clone();
            let messages = messages.clone();
            Box::pin(async move {
                let snapshot = super::mailbox_snapshot(&shared, channel_id).await;
                let answered = messages
                    .lock()
                    .expect("mock messages")
                    .values()
                    .any(|(_, content)| content == "ok");
                answered
                    && snapshot.intervention_queue.is_empty()
                    && snapshot.cancel_token.is_none()
            })
        }
    })
    .await;
    assert!(
        drained,
        "queued input must be dispatched and answered: queue={:?} messages={:?}",
        queued_ids(&harness).await,
        harness.messages()
    );
    assert!(harness.durable_queue().is_empty());
    // Ids alone can drain while a body is lost: each input must reach the provider
    // exactly once, the queued one first.
    let delivered = harness.provider_inputs().concat();
    for text in [QUEUED_TEXT, FRESH_TEXT] {
        assert_eq!(
            delivered.matches(text).count(),
            1,
            "{text:?} must reach the provider exactly once: {delivered:?}"
        );
    }
    assert!(
        delivered.find(QUEUED_TEXT) < delivered.find(FRESH_TEXT),
        "the queued input must reach the provider before the fresh one: {delivered:?}"
    );
    let unhandled = harness.unhandled_requests();
    assert!(
        unhandled.is_empty(),
        "mock Discord swallowed production calls as 404: {unhandled:?}"
    );
}
