//! #5176 R3 — a cancel must not silently discard queued user messages.
//!
//! The lifecycle already *observes* the loss (`queue_preserved`,
//! `queue_dropped_message_ids`) but nothing puts the messages back. This module
//! is that missing half: capture the channel queue before the cancel, and once
//! the cancel has settled either restore what it removed or record the removal
//! durably. Either outcome is acceptable under the user-message-lossless
//! contract; a silent drop is not.
//!
//! Entry points are free functions on a `TurnLifecycleTarget` so any cancel
//! surface — the queue API, an operator escape hatch — can reuse the same path
//! instead of growing a competing one.

use std::collections::HashSet;

use sqlx::PgPool;

use crate::db::relay_dead_letter::{RelayDeadLetterRecord, record_detached};
use crate::services::discord::session_identity::SessionIdentity;
use crate::services::turn_lifecycle::TurnLifecycleTarget;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, QueuePersistenceContext,
};

/// `relay_dead_letter.kind` for a queued user message a cancel removed and this
/// guard could not put back.
pub(crate) const KIND_CANCEL_QUEUE_DISCARD: &str = "cancel_queue_discard";

/// What the caller wants done with the items a cancel removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CancelQueueDisposition {
    /// Put the messages back so the post-cancel kickoff can run them.
    Restore,
    /// The operator asked for the queue to go (`force=true`), so record the
    /// casualties durably rather than reviving them.
    DeadLetterOnly,
}

/// The channel queue as it stood immediately before a cancel ran.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancelQueueCapture {
    items: Vec<Intervention>,
}

impl CancelQueueCapture {
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn message_ids(&self) -> Vec<u64> {
        self.items.iter().map(|i| i.message_id.get()).collect()
    }
}

/// What the guard managed to do about the items the cancel removed.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancelQueuePreservation {
    pub(crate) restored_message_ids: Vec<u64>,
    pub(crate) dead_lettered_message_ids: Vec<u64>,
    /// Removed, not restored, and not durably recorded either. Non-empty means
    /// the lossless contract was actually broken.
    pub(crate) unpreserved_message_ids: Vec<u64>,
    pub(crate) queue_depth_after: Option<usize>,
    /// Why the guard declined to restore, when it declined.
    pub(crate) hold_reason: Option<&'static str>,
}

impl CancelQueuePreservation {
    pub(crate) fn is_lossless(&self) -> bool {
        self.unpreserved_message_ids.is_empty()
    }
}

/// Read the channel's queued interventions before the cancel touches them.
///
/// An unresolvable channel or mailbox yields an empty capture: an unobservable
/// queue is not evidence that anything was there to lose.
pub(crate) async fn capture_queue_before_cancel(
    target: &TurnLifecycleTarget,
) -> CancelQueueCapture {
    let Some(channel_id) = target.channel_id else {
        return CancelQueueCapture::default();
    };
    let Some(handle) = ChannelMailboxRegistry::global_handle(channel_id) else {
        return CancelQueueCapture::default();
    };
    CancelQueueCapture {
        items: handle.snapshot().await.intervention_queue,
    }
}

/// Restore, or durably record, every captured message the cancel removed.
///
/// Call after the cancel and after any existing post-cancel drain, so the guard
/// only acts on items that are still missing.
pub(crate) async fn preserve_queue_after_cancel(
    target: &TurnLifecycleTarget,
    capture: &CancelQueueCapture,
    session_key: Option<&str>,
    pool: Option<&PgPool>,
    disposition: CancelQueueDisposition,
    reason: &'static str,
) -> CancelQueuePreservation {
    let mut outcome = CancelQueuePreservation::default();
    if capture.is_empty() {
        return outcome;
    }
    let Some(channel_id) = target.channel_id else {
        outcome.unpreserved_message_ids = capture.message_ids();
        report(target, &outcome, reason);
        return outcome;
    };

    let handle = ChannelMailboxRegistry::global_handle(channel_id);
    let snapshot = match handle.as_ref() {
        Some(handle) => Some(handle.snapshot().await),
        None => None,
    };
    outcome.queue_depth_after = snapshot
        .as_ref()
        .map(|snapshot| snapshot.intervention_queue.len());

    let survivors: HashSet<u64> = snapshot
        .as_ref()
        .map(|s| {
            s.intervention_queue
                .iter()
                .map(|i| i.message_id.get())
                .collect()
        })
        .unwrap_or_default();
    // An item promoted into the turn that is running right now left the queue on
    // purpose. Putting it back would run the same instruction twice.
    let promoted = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.active_user_message_id)
        .map(|message_id| message_id.get());

    let missing: Vec<Intervention> = capture
        .items
        .iter()
        .filter(|item| !survivors.contains(&item.message_id.get()))
        .filter(|item| Some(item.message_id.get()) != promoted)
        .cloned()
        .collect();
    if missing.is_empty() {
        return outcome;
    }

    // Over-release is the danger this whole fix has to avoid: a mailbox that
    // still anchors a turn is not ours to rewrite, so record instead of restore.
    let anchored = snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.cancel_token.is_some());
    if disposition == CancelQueueDisposition::DeadLetterOnly {
        outcome.hold_reason = Some("force_cancel_purge");
    } else if anchored {
        outcome.hold_reason = Some("live_turn_owns_channel");
    } else if let (Some(handle), Some(provider)) = (handle.as_ref(), target.provider.as_ref()) {
        let token_hash = session_key
            .and_then(SessionIdentity::parse)
            .and_then(|identity| identity.token_hash)
            .unwrap_or_default();
        let merged = handle
            .merge_restored_queue_items(
                missing.clone(),
                QueuePersistenceContext::new(provider, &token_hash, None),
            )
            .await;
        if merged.persistence_error.is_none() {
            outcome.restored_message_ids =
                missing.iter().map(|item| item.message_id.get()).collect();
            outcome.queue_depth_after = Some(merged.queue_len_after);
            return outcome;
        }
        outcome.hold_reason = Some("restore_persist_failed");
    } else {
        outcome.hold_reason = Some("no_live_mailbox");
    }

    for item in &missing {
        let message_id = item.message_id.get();
        if dead_letter(pool, channel_id.get(), item, reason, outcome.hold_reason) {
            outcome.dead_lettered_message_ids.push(message_id);
        } else {
            outcome.unpreserved_message_ids.push(message_id);
        }
    }
    report(target, &outcome, reason);
    outcome
}

/// Returns whether the row was handed to the durable sink. `false` means no PG
/// pool, so the removal has no durable record anywhere.
fn dead_letter(
    pool: Option<&PgPool>,
    channel_id: u64,
    item: &Intervention,
    reason: &'static str,
    hold_reason: Option<&'static str>,
) -> bool {
    record_detached(
        pool,
        RelayDeadLetterRecord {
            kind: KIND_CANCEL_QUEUE_DISCARD.to_string(),
            channel_id: channel_id.to_string(),
            author_id: Some(item.author_id.get().to_string()),
            message_id: Some(item.message_id.get().to_string()),
            content: item.text.clone(),
            reason: format!(
                "{reason}: cancel removed a queued user message ({})",
                hold_reason.unwrap_or("unknown")
            ),
        },
    )
    .is_some()
}

/// Only the loss paths log: a successful restore is already carried by the
/// returned `restored_message_ids`.
fn report(target: &TurnLifecycleTarget, outcome: &CancelQueuePreservation, reason: &'static str) {
    let channel_id = target
        .channel_id
        .map(poise::serenity_prelude::ChannelId::get)
        .unwrap_or(0);
    let hold_reason = outcome.hold_reason.unwrap_or("unknown");
    if !outcome.unpreserved_message_ids.is_empty() {
        tracing::error!(
            channel_id,
            reason,
            hold_reason,
            message_ids = ?outcome.unpreserved_message_ids,
            "cancel destroyed queued user messages with no durable record (see #5176)"
        );
    } else if !outcome.dead_lettered_message_ids.is_empty() {
        tracing::warn!(
            channel_id,
            reason,
            hold_reason,
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

    /// The repair direction: a cancel emptied the queue, so the guard puts the
    /// user instruction back where the next kickoff will find it.
    #[tokio::test]
    async fn restores_the_queued_message_a_cancel_removed() {
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
            capture.message_ids(),
            vec![9_001],
            "fixture must actually hold one queued user message"
        );

        handle.purge_queue(persistence, false).await;
        assert!(handle.snapshot().await.intervention_queue.is_empty());

        let outcome = preserve_queue_after_cancel(
            &target(channel_id),
            &capture,
            None,
            None,
            CancelQueueDisposition::Restore,
            "test_cancel",
        )
        .await;

        assert_eq!(outcome.restored_message_ids, vec![9_001]);
        assert!(outcome.dead_lettered_message_ids.is_empty());
        assert!(outcome.is_lossless());
        let survivors = handle.snapshot().await.intervention_queue;
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].message_id.get(), 9_001);
        assert_eq!(survivors[0].text, "the lost instruction");
    }

    /// The dangerous direction. A mailbox that still anchors a turn must not be
    /// rewritten by the guard, or a cancel could resurrect an instruction the
    /// running turn already took.
    #[tokio::test]
    async fn a_live_turn_anchor_holds_the_restore_back() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_176_302);
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "", None);
        handle
            .replace_queue(vec![queued(9_002, "already promoted")], persistence.clone())
            .await;
        let capture = capture_queue_before_cancel(&target(channel_id)).await;
        assert_eq!(capture.message_ids(), vec![9_002]);

        handle.purge_queue(persistence, false).await;
        let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            handle
                .try_start_turn(token.clone(), UserId::new(7), MessageId::new(70))
                .await,
            "fixture must own the foreground slot"
        );

        let outcome = preserve_queue_after_cancel(
            &target(channel_id),
            &capture,
            None,
            None,
            CancelQueueDisposition::Restore,
            "test_cancel",
        )
        .await;

        assert!(
            outcome.restored_message_ids.is_empty(),
            "a live turn's channel must not be rewritten by the cancel guard"
        );
        assert_eq!(outcome.hold_reason, Some("live_turn_owns_channel"));
        assert!(
            handle.snapshot().await.intervention_queue.is_empty(),
            "the guard must leave the live turn's queue exactly as it found it"
        );
        assert_eq!(outcome.unpreserved_message_ids, vec![9_002]);
        assert!(
            !outcome.is_lossless(),
            "without a pool the removal has no durable record, and the guard must say so"
        );
        drop(token);
    }

    /// The message the running turn took must not be handed back to the queue.
    #[tokio::test]
    async fn the_promoted_message_is_not_resurrected() {
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

        let outcome = preserve_queue_after_cancel(
            &target(channel_id),
            &capture,
            None,
            None,
            CancelQueueDisposition::Restore,
            "test_cancel",
        )
        .await;

        assert!(outcome.restored_message_ids.is_empty());
        assert!(outcome.unpreserved_message_ids.is_empty());
        assert!(
            outcome.is_lossless(),
            "a message the turn actually took is not a loss"
        );
        drop(token);
    }

    /// Nothing removed means nothing to do — the guard must not duplicate a
    /// queue it found intact.
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

        let outcome = preserve_queue_after_cancel(
            &target(channel_id),
            &capture,
            None,
            None,
            CancelQueueDisposition::Restore,
            "test_cancel",
        )
        .await;

        assert!(outcome.restored_message_ids.is_empty());
        assert!(outcome.is_lossless());
        assert_eq!(handle.snapshot().await.intervention_queue.len(), 1);
    }
}
