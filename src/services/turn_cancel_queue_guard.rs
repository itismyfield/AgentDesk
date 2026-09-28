//! Record captured queue removals and report a positive verdict only for proved non-removal.

use std::collections::HashSet;

use poise::serenity_prelude::MessageId;
use sqlx::PgPool;

use crate::db::relay_dead_letter::{RelayDeadLetterRecord, record_detached_reporting};
use crate::services::turn_lifecycle::TurnLifecycleTarget;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, Intervention};

/// `relay_dead_letter.kind` for a queued user message a cancel removed.
pub(crate) const KIND_CANCEL_QUEUE_DISCARD: &str = "cancel_queue_discard";

/// The channel queue as it stood immediately before a cancel ran, when it could
/// be read at all.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancelQueueCapture {
    items: Vec<Intervention>,
    /// False when the channel or its mailbox could not be read. Unobservable is
    /// the unmeasured case, never a measured-empty queue.
    observed: bool,
}

impl CancelQueueCapture {
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PurgeCounts {
    pub(crate) drained: usize,
    pub(crate) disk_files_removed: usize,
    pub(crate) own_files_removed: Option<usize>,
    pub(crate) queue_len_after: usize,
}

impl From<crate::services::turn_orchestrator::PurgeQueueResult> for PurgeCounts {
    fn from(result: crate::services::turn_orchestrator::PurgeQueueResult) -> Self {
        Self {
            drained: result.drained,
            disk_files_removed: result.disk_files_removed,
            own_files_removed: result.own_files_removed,
            queue_len_after: result.queue_len_after,
        }
    }
}

pub(crate) enum CancelRemovalWitness {
    Preserve { disk_lost: Option<bool> },
    Purge(Option<PurgeCounts>),
}

pub(crate) fn disk_queue_lost(before: Option<bool>, after: Option<bool>) -> Option<bool> {
    before.zip(after).map(|(before, after)| before && !after)
}

/// What the guard managed to record about the items the cancel removed.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancelQueueLoss {
    pub(crate) dead_lettered_message_ids: Vec<u64>,
    /// Removed with no durable record anywhere. Non-empty is the contract
    /// violation itself.
    pub(crate) unpreserved_message_ids: Vec<u64>,
    /// Depth of the channel queue after the cancel; `None` when the mailbox never answered.
    pub(crate) queue_depth_after: Option<usize>,
    /// True only when observed queues and the removal witness prove that nothing was removed.
    pub(crate) proved_nothing_removed: bool,
}

impl CancelQueueLoss {
    /// Unrecorded loss is false; only proved non-removal is true, and all other cases are unknown.
    pub(crate) fn loss_recorded(&self) -> Option<bool> {
        if !self.unpreserved_message_ids.is_empty() {
            return Some(false);
        }
        self.proved_nothing_removed.then_some(true)
    }
}

/// Read the channel's queued interventions before the cancel touches them.
///
/// An unresolvable channel or mailbox yields an *unobserved* capture, which the
/// record below refuses to call complete: absence of a reading is not a reading
/// of absence.
pub(crate) async fn capture_queue_before_cancel(
    target: &TurnLifecycleTarget,
) -> CancelQueueCapture {
    let Some(channel_id) = target.channel_id else {
        return CancelQueueCapture::default();
    };
    let Some(handle) = ChannelMailboxRegistry::global_handle(channel_id) else {
        return CancelQueueCapture::default();
    };
    match handle.try_snapshot().await {
        Ok(snapshot) => CancelQueueCapture {
            items: snapshot.intervention_queue,
            observed: true,
        },
        Err(_) => CancelQueueCapture::default(),
    }
}

/// Record captured losses after the last mutation; the witness bounds positive verdicts to no removal.
pub(crate) async fn record_queue_loss_after_cancel(
    target: &TurnLifecycleTarget,
    capture: &CancelQueueCapture,
    pool: Option<&PgPool>,
    witness: CancelRemovalWitness,
    reason: &'static str,
) -> CancelQueueLoss {
    let mut outcome = CancelQueueLoss {
        proved_nothing_removed: capture.observed
            && match witness {
                CancelRemovalWitness::Preserve {
                    disk_lost: Some(false),
                } => true,
                CancelRemovalWitness::Purge(Some(c)) => {
                    capture.is_empty()
                        && c.drained == 0
                        && c.disk_files_removed == 0
                        && c.own_files_removed == Some(0)
                }
                _ => false,
            },
        ..Default::default()
    };
    if capture.is_empty() {
        return outcome;
    }
    // Unreachable with a non-empty capture, which always resolved a channel.
    let Some(channel_id) = target.channel_id else {
        return outcome;
    };

    let snapshot = match ChannelMailboxRegistry::global_handle(channel_id) {
        Some(handle) => handle.try_snapshot().await.ok(),
        None => None,
    };
    // An unread post-cancel queue proves nothing kept, so the cancel gets no verdict.
    outcome.proved_nothing_removed &= snapshot.is_some();
    outcome.queue_depth_after = snapshot
        .as_ref()
        .map(|snapshot| snapshot.intervention_queue.len());
    let kept = match snapshot.as_ref() {
        Some(snapshot) => kept_message_ids(
            &snapshot.intervention_queue,
            &snapshot.pending_user_dispatch_source_ids,
            snapshot.active_user_message_id,
        ),
        None => HashSet::new(),
    };

    for item in capture
        .items
        .iter()
        .filter(|item| !kept.contains(&item.message_id.get()))
    {
        let message_id = item.message_id.get();
        if dead_letter(pool, channel_id.get(), item, reason).await {
            outcome.dead_lettered_message_ids.push(message_id);
        } else {
            outcome.unpreserved_message_ids.push(message_id);
        }
    }
    outcome.proved_nothing_removed &=
        outcome.dead_lettered_message_ids.is_empty() && outcome.unpreserved_message_ids.is_empty();
    report(target, &outcome, reason);
    outcome
}

/// Every place a captured message can legitimately be after the cancel. The
/// queue is only one of them: an item between dequeue and claim sits in
/// `pending_user_dispatch`, and the one the turn already took is
/// `active_user_message_id`. Both left the queue on purpose, so recording
/// either as lost would be a false loss report.
fn kept_message_ids(
    queued: &[Intervention],
    dispatching: &[MessageId],
    active: Option<MessageId>,
) -> HashSet<u64> {
    queued
        .iter()
        .map(|item| item.message_id.get())
        .chain(dispatching.iter().map(|id| id.get()))
        .chain(active.map(|id| id.get()))
        .collect()
}

/// Returns whether the row actually landed in the durable sink. Decided by the
/// write, not by the pool: a spawned INSERT that fails records nothing.
async fn dead_letter(
    pool: Option<&PgPool>,
    channel_id: u64,
    item: &Intervention,
    reason: &'static str,
) -> bool {
    let recorded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = std::sync::Arc::clone(&recorded);
    let handle = record_detached_reporting(
        pool,
        RelayDeadLetterRecord {
            kind: KIND_CANCEL_QUEUE_DISCARD.to_string(),
            channel_id: channel_id.to_string(),
            author_id: Some(item.author_id.get().to_string()),
            message_id: Some(item.message_id.get().to_string()),
            content: item.text.clone(),
            reason: format!("{reason}: cancel removed a queued user message"),
        },
        move |landed| observed.store(landed, std::sync::atomic::Ordering::SeqCst),
    );
    if let Some(handle) = handle {
        let _ = handle.await;
    }
    recorded.load(std::sync::atomic::Ordering::SeqCst)
}

/// A silent removal is the defect; both branches here are removals, so the only
/// thing not logged is a cancel that took nothing.
fn report(target: &TurnLifecycleTarget, outcome: &CancelQueueLoss, reason: &'static str) {
    let channel_id = target
        .channel_id
        .map(poise::serenity_prelude::ChannelId::get)
        .unwrap_or(0);
    if !outcome.unpreserved_message_ids.is_empty() {
        tracing::error!(
            channel_id,
            reason,
            message_ids = ?outcome.unpreserved_message_ids,
            "cancel destroyed queued user messages with no durable record (see #5176)"
        );
    } else if !outcome.dead_lettered_message_ids.is_empty() {
        tracing::warn!(
            channel_id,
            reason,
            message_ids = ?outcome.dead_lettered_message_ids,
            "cancel removed queued user messages; recorded them in relay_dead_letter (see #5176)"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use super::*;
    use crate::services::provider::ProviderKind;
    use crate::services::turn_orchestrator::{InterventionMode, QueuePersistenceContext};

    fn queued(message_id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    fn target(channel_id: ChannelId) -> TurnLifecycleTarget {
        TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel_id),
            tmux_name: String::new(),
        }
    }

    #[tokio::test]
    async fn queue_truth_purge_requires_a_complete_no_removal_witness() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let registry = ChannelMailboxRegistry::default();
        let provider = ProviderKind::Claude;
        let mut wrong = Vec::new();
        for (index, row) in ["empty", "memory", "token", "flat", "unknown"]
            .into_iter()
            .enumerate()
        {
            let channel = ChannelId::new(6038760 + index as u64);
            let handle = registry.handle(channel);
            let target = target(channel);
            let persistence = QueuePersistenceContext::new(&provider, "", None);
            let capture = capture_queue_before_cancel(&target).await;
            if row == "memory" {
                handle
                    .replace_queue(vec![queued(42, "late")], persistence.clone())
                    .await;
                crate::services::turn_orchestrator::save_channel_queue(
                    &provider,
                    "",
                    channel,
                    &[],
                    None,
                )
                .unwrap();
            }
            if row == "token" || row == "flat" {
                crate::services::turn_orchestrator::save_channel_queue(
                    &provider,
                    if row == "token" { "other" } else { "" },
                    channel,
                    &[queued(42, "disk")],
                    None,
                )
                .unwrap();
            }
            let mut purge = handle.try_purge_queue(persistence, true).await.unwrap();
            assert_eq!(purge.drained, usize::from(row == "memory"));
            assert_eq!(purge.disk_files_removed, usize::from(row == "token"));
            assert_eq!(purge.own_files_removed, Some(usize::from(row == "flat")));
            if row == "unknown" {
                purge.own_files_removed = None;
            }
            let witness = CancelRemovalWitness::Purge(Some(purge.into()));
            let outcome =
                record_queue_loss_after_cancel(&target, &capture, None, witness, "test").await;
            registry.remove_fixture_for_test(channel);
            let expected = (row == "empty").then_some(true);
            if outcome.loss_recorded() != expected {
                wrong.push(format!(
                    "{row}: {:?} expected {expected:?}",
                    outcome.loss_recorded()
                ));
            }
        }
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn queue_truth_disk_loss_needs_both_readings() {
        for (before, after, expected) in [
            (None, None, None),
            (None, Some(false), None),
            (None, Some(true), None),
            (Some(false), None, None),
            (Some(true), None, None),
            (Some(false), Some(false), Some(false)),
            (Some(false), Some(true), Some(false)),
            (Some(true), Some(false), Some(true)),
            (Some(true), Some(true), Some(false)),
        ] {
            assert_eq!(disk_queue_lost(before, after), expected);
        }
    }

    #[tokio::test]
    async fn queue_truth_preserved_capture_requires_measured_disk() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(6038768);
        let handle = registry.handle(channel);
        let ctx = QueuePersistenceContext::new(&ProviderKind::Claude, "", None);
        handle.replace_queue(vec![queued(42, "kept")], ctx).await;
        let target = target(channel);
        let capture = capture_queue_before_cancel(&target).await;
        for (disk_lost, expected) in [(None, None), (Some(true), None), (Some(false), Some(true))] {
            let witness = CancelRemovalWitness::Preserve { disk_lost };
            let result =
                record_queue_loss_after_cancel(&target, &capture, None, witness, "test").await;
            assert_eq!(result.loss_recorded(), expected);
            assert!(result.dead_lettered_message_ids.is_empty());
        }
        registry.remove_fixture_for_test(channel);
    }

    fn ids(capture: &CancelQueueCapture) -> Vec<u64> {
        capture.items.iter().map(|i| i.message_id.get()).collect()
    }

    /// Every test drives the same entry point with no pool, so a removal it
    /// cannot record lands in `unpreserved` where the assertions can see it.
    async fn record(
        target: &TurnLifecycleTarget,
        capture: &CancelQueueCapture,
        disk_lost: bool,
    ) -> CancelQueueLoss {
        record_queue_loss_after_cancel(
            target,
            capture,
            None,
            CancelRemovalWitness::Preserve {
                disk_lost: Some(disk_lost),
            },
            "test_cancel",
        )
        .await
    }

    /// Replaces the removed restore test. It protected the detection of what a
    /// cancel took; the same fixture now asserts that what it took is named
    /// rather than put back.
    #[tokio::test]
    async fn records_the_queued_message_a_cancel_removed() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_176_301);
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "", None);
        handle
            .replace_queue(
                vec![queued(9_001, "the lost instruction")],
                persistence.clone(),
            )
            .await;

        let capture = capture_queue_before_cancel(&target(channel_id)).await;
        assert_eq!(
            ids(&capture),
            vec![9_001],
            "fixture must actually hold one queued user message"
        );

        handle.purge_queue(persistence, false).await;
        let outcome = record(&target(channel_id), &capture, false).await;

        assert_eq!(outcome.unpreserved_message_ids, vec![9_001]);
        assert_eq!(
            outcome.loss_recorded(),
            Some(false),
            "without a pool the removal has no durable record, and it must say so"
        );
    }

    /// Replaces the live-anchor test. That one proved the guard declined to
    /// rewrite an anchored mailbox; this proves the guard never writes to a
    /// mailbox at all, which is the same protection promoted from a branch
    /// condition to a structural property.
    #[tokio::test]
    async fn the_guard_never_writes_to_the_mailbox() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_176_302);
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "", None);
        handle
            .replace_queue(
                vec![queued(9_002, "taken by the cancel")],
                persistence.clone(),
            )
            .await;
        let capture = capture_queue_before_cancel(&target(channel_id)).await;
        assert_eq!(ids(&capture), vec![9_002]);

        handle.purge_queue(persistence, false).await;
        let outcome = record(&target(channel_id), &capture, false).await;

        assert!(
            handle.snapshot().await.intervention_queue.is_empty(),
            "the guard must not put anything back into the channel mailbox"
        );
        assert_eq!(outcome.unpreserved_message_ids, vec![9_002]);
    }

    /// A message the turn actually took is not a loss, so naming it as one
    /// would be a false report.
    #[tokio::test]
    async fn the_promoted_message_is_not_recorded_as_lost() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_176_303);
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "", None);
        handle
            .replace_queue(
                vec![queued(70, "this one started running")],
                persistence.clone(),
            )
            .await;
        let capture = capture_queue_before_cancel(&target(channel_id)).await;

        handle.purge_queue(persistence, false).await;
        let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            handle
                .try_start_turn(token.clone(), UserId::new(7), MessageId::new(70))
                .await
        );

        let outcome = record(&target(channel_id), &capture, false).await;

        assert!(outcome.unpreserved_message_ids.is_empty());
        assert!(outcome.dead_lettered_message_ids.is_empty());
        assert_eq!(
            outcome.loss_recorded(),
            Some(true),
            "a message the turn actually took is not a loss"
        );
        drop(token);
    }

    /// Nothing removed means nothing to record.
    #[tokio::test]
    async fn an_intact_queue_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_176_304);
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        handle
            .replace_queue(
                vec![queued(9_003, "still queued")],
                QueuePersistenceContext::new(&provider, "", None),
            )
            .await;
        let capture = capture_queue_before_cancel(&target(channel_id)).await;
        handle
            .replace_queue(
                vec![
                    queued(9_003, "still queued"),
                    queued(9_004, "arrived after capture"),
                ],
                QueuePersistenceContext::new(&provider, "", None),
            )
            .await;

        let outcome = record(&target(channel_id), &capture, false).await;

        assert!(outcome.unpreserved_message_ids.is_empty());
        assert_eq!(outcome.loss_recorded(), Some(true));
        assert_eq!(
            handle
                .snapshot()
                .await
                .intervention_queue
                .iter()
                .map(|i| i.message_id.get())
                .collect::<Vec<_>>(),
            [9_003, 9_004]
        );
    }

    /// The union is the whole guard against a false loss report, so it is
    /// tested as what it is: three inputs, one set. No actor loop required.
    #[test]
    fn kept_ids_cover_the_queue_the_dispatch_window_and_the_running_turn() {
        assert_eq!(
            kept_message_ids(
                &[queued(1, "still queued")],
                &[MessageId::new(2)],
                Some(MessageId::new(3)),
            ),
            HashSet::from([1, 2, 3]),
            "a message in any of the three places has not been lost"
        );
    }

    /// A queue the cancel could never read is the unmeasured case, so the
    /// response must not answer the question at all.
    #[tokio::test]
    async fn an_unreadable_queue_reports_no_verdict() {
        let unknown = target(ChannelId::new(5_176_305));
        let capture = capture_queue_before_cancel(&unknown).await;
        assert_eq!(
            record(&unknown, &capture, false).await.loss_recorded(),
            None,
            "an unobservable queue must not be reported as a kept promise"
        );
    }

    /// The guard reads only the in-memory queue. A disk-backed queue file that
    /// disappeared across the cancel is a removal it cannot enumerate, so an
    /// empty in-memory capture is not evidence that nothing was taken.
    #[tokio::test]
    async fn a_disk_only_removal_is_not_reported_as_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let channel_id = ChannelId::new(5_176_306);
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let _handle = registry.handle(channel_id);
        let capture = capture_queue_before_cancel(&target(channel_id)).await;
        assert!(
            capture.is_empty() && capture.observed,
            "fixture must be an observed but empty in-memory queue"
        );

        assert_eq!(
            record(&target(channel_id), &capture, true)
                .await
                .loss_recorded(),
            None,
            "a disk queue that vanished unseen must not be reported as fully recorded"
        );
    }

    type BreakMailbox = fn(&ChannelMailboxRegistry, ChannelId);
    const DEAD_MAILBOXES: [(&str, BreakMailbox); 2] = [
        (
            "closed",
            ChannelMailboxRegistry::insert_unreachable_for_test,
        ),
        (
            "reply-dropping",
            ChannelMailboxRegistry::insert_reply_dropping_for_test,
        ),
    ];

    /// A mailbox that exists but never answers is unread, not an empty queue.
    #[tokio::test]
    async fn a_dead_mailbox_capture_reports_no_verdict() {
        for (offset, (kind, kill)) in DEAD_MAILBOXES.into_iter().enumerate() {
            let channel_id = ChannelId::new(6_038_301 + offset as u64);
            kill(&ChannelMailboxRegistry::default(), channel_id);
            let capture = capture_queue_before_cancel(&target(channel_id)).await;
            assert!(
                !capture.observed,
                "{kind}: a dead mailbox was read as empty"
            );
            assert_eq!(
                record(&target(channel_id), &capture, false)
                    .await
                    .loss_recorded(),
                None,
                "{kind}: an unread queue must not be reported as a kept promise"
            );
        }
    }

    /// The mailbox dies between capture and record: nothing is provably kept, and an
    /// unrecorded removal still reports `false` rather than hiding behind the unread queue.
    #[tokio::test]
    async fn a_mailbox_that_dies_after_capture_keeps_nothing_and_still_reports_failure() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        for (offset, (kind, kill)) in DEAD_MAILBOXES.into_iter().enumerate() {
            let channel_id = ChannelId::new(6_038_311 + offset as u64);
            let registry = ChannelMailboxRegistry::default();
            registry
                .handle(channel_id)
                .replace_queue(
                    vec![queued(9_311, "captured, then unreadable")],
                    QueuePersistenceContext::new(&ProviderKind::Claude, "", None),
                )
                .await;
            let capture = capture_queue_before_cancel(&target(channel_id)).await;
            assert_eq!(
                ids(&capture),
                vec![9_311],
                "{kind}: fixture must capture one item"
            );
            kill(&registry, channel_id);

            let outcome = record(&target(channel_id), &capture, false).await;
            assert_eq!(
                outcome.queue_depth_after, None,
                "{kind}: no reading, no depth"
            );
            assert_eq!(outcome.unpreserved_message_ids, vec![9_311], "{kind}");
            assert_eq!(outcome.loss_recorded(), Some(false), "{kind}");
        }
    }
}
