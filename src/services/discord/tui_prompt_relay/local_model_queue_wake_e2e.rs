//! Scenario: a local-only `/model` observation must wake an idle durable queue
//! through the production workers, without dispatching the queued turn twice.
//!
//! The mock Discord transport, the `serenity::Context` over it and the
//! production entry points live in [`super::relay_e2e`].

use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::relay_e2e::{self, CHANNEL_ID, RelayE2eHarness};

const A_MESSAGE_ID: u64 = 940_487_400_000_011;
const B_MESSAGE_ID: u64 = 940_487_400_000_012;

#[derive(Debug, PartialEq, Eq)]
enum PromotedBCompletionProgress {
    AwaitingMailboxRelease,
    AwaitingQueueEligible,
    Complete,
}

fn validate_promoted_b_completion_lifecycle(
    events: &[super::super::turn_completion_events::TurnCompletionEvent],
    channel_id: ChannelId,
) -> Result<PromotedBCompletionProgress, String> {
    let expected_mailbox_release =
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        );
    let expected_queue_eligible =
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        );
    match events {
        [] => Ok(PromotedBCompletionProgress::AwaitingMailboxRelease),
        [event] if *event == expected_mailbox_release => {
            Ok(PromotedBCompletionProgress::AwaitingQueueEligible)
        }
        [event] => Err(format!(
            "promoted B must publish MailboxReleased first; received phase={:?}, turn_id={:?}, channel_id={}",
            event.phase, event.turn_id, event.channel_id
        )),
        [mailbox_release, queue_eligible]
            if *mailbox_release == expected_mailbox_release
                && *queue_eligible == expected_queue_eligible =>
        {
            Ok(PromotedBCompletionProgress::Complete)
        }
        [_, event, ..] => Err(format!(
            "promoted B must publish exactly one MailboxReleased followed by exactly one QueueEligible; received phase={:?}, turn_id={:?}, channel_id={}",
            event.phase, event.turn_id, event.channel_id
        )),
    }
}

async fn drain_promoted_b_completion_lifecycle(
    rx: &mut tokio::sync::broadcast::Receiver<
        super::super::turn_completion_events::TurnCompletionEvent,
    >,
    channel_id: ChannelId,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut events = Vec::with_capacity(2);

    loop {
        let event = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => panic!(
                "promoted B completion lifecycle did not reach QueueEligible before the deadline; observed_events={events:?}"
            ),
            Ok(Ok(event)) => event,
            Ok(Err(error)) => panic!(
                "completion receiver must remain open while draining promoted B; recv error={error:?}"
            ),
        };
        events.push(event);
        match validate_promoted_b_completion_lifecycle(&events, channel_id) {
            Ok(PromotedBCompletionProgress::Complete) => return,
            Ok(PromotedBCompletionProgress::AwaitingMailboxRelease)
            | Ok(PromotedBCompletionProgress::AwaitingQueueEligible) => {}
            Err(error) => panic!("{error}"),
        }
    }
}

async fn assert_no_local_only_completion_event(
    rx: &mut tokio::sync::broadcast::Receiver<
        super::super::turn_completion_events::TurnCompletionEvent,
    >,
    window: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return,
            Ok(Ok(event)) => panic!(
                "local-only halves must not publish a completion event; received phase={:?}, turn_id={:?}, channel_id={}",
                event.phase, event.turn_id, event.channel_id
            ),
            Ok(Err(error)) => {
                panic!("local-only completion receiver must remain open; recv error={error:?}")
            }
        }
    }
}

async fn assert_local_only_completion_lifecycle(
    rx: &mut tokio::sync::broadcast::Receiver<
        super::super::turn_completion_events::TurnCompletionEvent,
    >,
    channel_id: ChannelId,
    lifecycle_timeout: std::time::Duration,
    strict_window: std::time::Duration,
) {
    drain_promoted_b_completion_lifecycle(rx, channel_id, lifecycle_timeout).await;
    assert_no_local_only_completion_event(rx, strict_window).await;
}

async fn completion_guard_must_panic(
    events: Vec<super::super::turn_completion_events::TurnCompletionEvent>,
) {
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    for event in events {
        tx.send(event).expect("completion receiver registered");
    }
    let task = tokio::spawn(async move {
        assert_local_only_completion_lifecycle(
            &mut rx,
            ChannelId::new(CHANNEL_ID),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        )
        .await;
    });
    let error = task
        .await
        .expect_err("the completion lifecycle guard must reject the mutation");
    assert!(
        error.is_panic(),
        "completion lifecycle rejection must terminate through its own assertion"
    );
}

#[test]
fn promoted_b_completion_lifecycle_validates_order_and_cardinality() {
    let channel_id = ChannelId::new(CHANNEL_ID);
    let mailbox_release =
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        );
    let queue_eligible = super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
        channel_id,
        Some(B_MESSAGE_ID),
    );

    assert_eq!(
        validate_promoted_b_completion_lifecycle(
            &[mailbox_release.clone(), queue_eligible.clone()],
            channel_id,
        ),
        Ok(PromotedBCompletionProgress::Complete)
    );
    assert!(
        validate_promoted_b_completion_lifecycle(
            &[queue_eligible.clone(), mailbox_release.clone()],
            channel_id,
        )
        .is_err(),
        "QueueEligible before MailboxReleased must be rejected"
    );
    assert!(
        validate_promoted_b_completion_lifecycle(
            &[mailbox_release.clone(), mailbox_release],
            channel_id,
        )
        .is_err(),
        "duplicate MailboxReleased must be rejected"
    );
    assert!(
        validate_promoted_b_completion_lifecycle(&[queue_eligible], channel_id).is_err(),
        "QueueEligible without MailboxReleased must be rejected"
    );
}

#[tokio::test]
async fn completion_lifecycle_rejects_duplicate_mailbox_release() {
    let channel_id = ChannelId::new(CHANNEL_ID);
    completion_guard_must_panic(vec![
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
    ])
    .await;
}

#[tokio::test]
async fn completion_lifecycle_rejects_event_after_queue_eligible() {
    let channel_id = ChannelId::new(CHANNEL_ID);
    completion_guard_must_panic(vec![
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
    ])
    .await;
}

#[tokio::test]
async fn completion_lifecycle_rejects_queue_eligible_before_mailbox_release() {
    let channel_id = ChannelId::new(CHANNEL_ID);
    completion_guard_must_panic(vec![
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
    ])
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_model_observation_wakes_idle_durable_queue_through_production_workers() {
    let harness = RelayE2eHarness::start().await;
    let shared = harness.shared.clone();
    let channel_id = harness.channel_id;

    let _a_guard = harness
        .spawn_turn_held_at_placeholder(
            A_MESSAGE_ID,
            "A holds the active mailbox",
            Duration::from_secs(2),
        )
        .await;

    let active_a = harness.mailbox().await;
    assert_eq!(
        active_a.active_user_message_id,
        Some(MessageId::new(A_MESSAGE_ID))
    );

    harness
        .deliver_user_message(B_MESSAGE_ID, "B must survive as durable pending intake")
        .await
        .expect("B traverses FullEvent intake");
    let queued_b = harness.mailbox().await;
    assert_eq!(
        queued_b.active_user_message_id,
        Some(MessageId::new(A_MESSAGE_ID))
    );
    assert_eq!(queued_b.intervention_queue.len(), 1);
    assert_eq!(
        queued_b.intervention_queue[0].message_id,
        MessageId::new(B_MESSAGE_ID)
    );
    let durable_b = harness.durable_queue();
    assert_eq!(
        durable_b.len(),
        1,
        "B must be durably persisted by BusyActiveTurn"
    );
    assert_eq!(durable_b[0].message_id, MessageId::new(B_MESSAGE_ID));

    // Subscribe before the release that makes A's placeholder-failure recovery publish its
    // completion event. `broadcast` only buffers sends that happen after a receiver exists, so
    // subscribing later left the negative assertion below racing A's normal `QueueEligible`
    // publish instead of observing the local-only halves.
    let mut a_release_rx = harness.subscribe_completions();

    harness.release_held_placeholder();
    assert!(
        relay_e2e::wait_until(Duration::from_secs(1), {
            let shared = shared.clone();
            move || {
                let shared = shared.clone();
                Box::pin(async move {
                    let snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
                    snapshot.active_user_message_id.is_none()
                        && snapshot.intervention_queue.len() == 1
                        && shared
                            .restart
                            .deferred_hook_channels
                            .contains_key(&channel_id)
                })
            }
        })
        .await,
        "production placeholder-failure recovery must leave idle durable B behind an armed normal worker"
    );

    // Drain A's release event explicitly so the local-only assertion below observes only the
    // `/model` halves rather than whatever A left buffered.
    let a_release = tokio::time::timeout(Duration::from_secs(2), a_release_rx.recv())
        .await
        .expect("A placeholder-failure release must publish one completion event")
        .expect("completion bus open");
    assert_eq!(a_release.channel_id, channel_id);

    let before_model = harness.mailbox().await;
    assert!(before_model.active_user_message_id.is_none());
    assert_eq!(
        before_model.intervention_queue[0].message_id,
        MessageId::new(B_MESSAGE_ID)
    );

    harness.cache_relay_transport();
    let tmux = "AgentDesk-claude-4874-local-model-wake";
    harness.attach_tmux_watcher(tmux, "claude-queue-wake.jsonl");
    harness.spawn_relay_worker();

    // Subscribe after draining A's release event so every edge after the local-only observations is
    // inspected, including promoted B's known two-phase completion lifecycle.
    let mut local_only_completion_rx = harness.subscribe_completions();
    let command_half = "<command-message>x</command-message>\n<command-name>/model</command-name>";
    let stdout_half = "<local-command-stdout>Set model to Fable 5</local-command-stdout>";
    assert_eq!(
        harness.observe_tui_prompt(tmux, command_half),
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        harness.observe_tui_prompt(tmux, stdout_half),
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    );

    assert!(
        harness
            .wait_for_next_placeholder(Duration::from_millis(1500))
            .await,
        "local /model must wake the occupied two-second deferred worker"
    );

    let promoted_b = harness.mailbox().await;
    assert_eq!(
        promoted_b.active_user_message_id,
        Some(MessageId::new(B_MESSAGE_ID))
    );
    assert!(promoted_b.intervention_queue.is_empty());
    assert!(
        harness.durable_queue().is_empty(),
        "production kickoff must durably dequeue B"
    );
    assert_eq!(harness.placeholder_posts(), 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        harness.placeholder_posts(),
        2,
        "coalesced two-half wake must not dispatch B twice"
    );
    assert_eq!(harness.local_note_posts(), 2);

    assert!(!harness.relay_lease_present(tmux));
    assert!(!harness.ssh_direct_observation_pending(tmux));
    assert_eq!(harness.prompt_anchor(tmux), None);
    assert!(
        !harness.synthetic_inflight_matches(tmux, 1),
        "local-only halves must not create synthetic inflight ownership"
    );

    // Source tracing reproduced MailboxReleased(B) 3/3 as promoted B's normal Discord bridge
    // lifecycle: TerminalEvent::Complete with FinalizeContext::bridge(),
    // request_owner_name="queue-user", is_external_input_tui_direct=false, and no TUI runtime.
    // CompletionAdmission::claim_queue_eligible then emits QueueEligible(B) exactly once after its
    // mailbox-released and terminal barriers settle. Inspect that exact ordered pair before opening
    // the strict window. The idle-queue consumer continues without dispatch on MailboxReleased; the
    // other production consumer can only stop B's typing indicator. #5018 tracks causal origin to
    // close the remaining value-only gap where a faulty local-only publisher replaces, rather than
    // duplicates, one of B's own lifecycle edges.
    assert_local_only_completion_lifecycle(
        &mut local_only_completion_rx,
        channel_id,
        std::time::Duration::from_secs(10),
        std::time::Duration::from_millis(100),
    )
    .await;
}
