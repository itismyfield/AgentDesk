//! A mailbox actor that never answers measured no queue; the gate keeps the card instead.

use super::*;
use crate::services::discord::{
    gateway, make_shared_data_for_tests, queue_exit_drain_queued_placeholders, runtime_bootstrap,
};
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, QueueExitEvent, QueueExitKind};

struct RecordingDeleter(std::sync::Mutex<Vec<MessageId>>);

impl StalePlaceholderDeleter for RecordingDeleter {
    fn delete<'a>(
        &'a self,
        _channel_id: ChannelId,
        placeholder_msg_id: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        self.0.lock().unwrap().push(placeholder_msg_id);
        Box::pin(async { Ok(()) })
    }
}

fn leaving(head: MessageId, sources: &[MessageId]) -> Intervention {
    let mut item = ChannelMailboxRegistry::queued_for_test(head.get());
    item.source_message_ids = sources.to_vec();
    item
}

/// Every gated entry point, on a closed, a reply-dropping and an answered empty mailbox.
#[tokio::test]
async fn an_unanswered_mailbox_keeps_queued_cards_and_an_answered_empty_one_releases_them() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let deleter = RecordingDeleter(std::sync::Mutex::new(Vec::new()));
    let mut released = Vec::new();
    let cases = ["closed", "reply_dropping", "empty"];
    for (index, case) in cases.into_iter().enumerate() {
        let base = 604_620_000_000_000 + 1_000 * index as u64;
        let (channel, m) = (ChannelId::new(base), |n: u64| MessageId::new(base + n));
        match case {
            "closed" => shared.mailboxes.insert_unreachable_for_test(channel),
            "reply_dropping" => shared.mailboxes.insert_reply_dropping_for_test(channel),
            _ => drop(shared.mailbox(channel)),
        }
        // Boot restore: a disk row refused because another owner maps the same card.
        shared
            .queued
            .queued_placeholders
            .insert((channel, m(2)), m(10));
        let stale = [(channel, m(1), m(10))];
        runtime_bootstrap::delete_stale_queued_placeholder_cards_with(&deleter, &shared, &stale)
            .await;
        // Queue exit of an entry holding its own card.
        shared
            .queued
            .queued_placeholders
            .insert((channel, m(3)), m(11));
        let exit = QueueExitEvent {
            intervention: leaving(m(3), &[m(3)]),
            kind: QueueExitKind::Cancelled,
        };
        let exited = queue_exit_drain_queued_placeholders(&shared, channel, &[&exit]).await;
        // Merged dispatch draining a source's card.
        shared
            .queued
            .queued_placeholders
            .insert((channel, m(6)), m(13));
        let merged =
            gateway::drain_merged_queued_placeholders(&shared, channel, m(5), &[m(5), m(6)]).await;
        // A parked card whose deferred drain re-enters the gate.
        shared
            .add_pending_queue_exit_placeholder_clear_one(channel, m(4), m(12))
            .await;
        released.push((case, exited.len(), merged.len()));
    }
    crate::services::discord::drain_pending_queue_exit_placeholder_clears_with(&shared, &deleter)
        .await;

    let deleted = deleter.0.lock().unwrap().clone();
    let pending = shared.pending_queue_exit_placeholder_clears();
    let outcomes: Vec<_> = released
        .into_iter()
        .enumerate()
        .map(|(index, (case, exited, merged))| {
            let base = 604_620_000_000_000 + 1_000 * index as u64;
            let (channel, m) = (ChannelId::new(base), |n: u64| MessageId::new(base + n));
            let boot = deleted.contains(&m(10));
            let parked_deleted = deleted.contains(&m(12));
            let parked_kept = pending.contains(&(channel, m(4), m(12)));
            (case, boot, exited, merged, parked_deleted, parked_kept)
        })
        .collect();
    let kept = |case| (case, false, 0, 0, false, true);
    assert_eq!(
        outcomes,
        [
            kept("closed"),
            kept("reply_dropping"),
            ("empty", true, 1, 1, true, false)
        ],
        "(case, boot delete, exit releases, merge releases, parked delete, parked row kept)"
    );
}
