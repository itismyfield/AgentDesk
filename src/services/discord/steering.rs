//! `/steer` (P0): inject a steering instruction into a channel's already-live
//! turn by enqueuing a standalone, never-merged `Soft` intervention.
//!
//! P0 SCOPE: current-channel only. No HTTP API, no DB helpers, no
//! `session_forwarding`, and no new delivery core. This is a thin wrapper over
//! the existing private `mailbox_enqueue_intervention` / `mailbox_has_active_turn`
//! in the parent `discord` module, reachable here via `super::` because
//! `steering` is a child module.

use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{EnqueueRefusalReason, Intervention, InterventionMode};

use super::{SharedData, mailbox_enqueue_intervention, mailbox_has_active_turn};

/// Inputs for one `/steer` invocation (current-channel scoped).
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct SteeringRequest {
    pub(in crate::services::discord) channel_id: ChannelId,
    /// Effective provider for this channel (from `ctx.data().provider`).
    pub(in crate::services::discord) provider: ProviderKind,
    /// Invoking Discord user.
    pub(in crate::services::discord) author_id: UserId,
    /// Single source id == the Discord interaction id (`ctx.id()`), wrapped into
    /// a `MessageId`. Unique per invocation, so it can never collide with an
    /// already-queued `source_message_ids` entry (avoids `SourceIdAlreadyQueued`).
    pub(in crate::services::discord) source_id: MessageId,
    /// Steering instruction text (already trimmed by the caller).
    pub(in crate::services::discord) instruction: String,
}

/// Terminal classification of a `/steer` attempt. The fixed outcome set the PRD
/// pins; `DiscordUnavailable` / `SessionNotFound` are reserved for the P1 HTTP
/// path and are never produced by the P0 current-channel path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum SteeringOutcome {
    /// Enqueued onto the live turn's mailbox as a standalone head.
    Queued,
    /// A mailbox guard refused the enqueue (dedup / actor reachability).
    Refused { reason: EnqueueRefusalReason },
    /// No active turn on the channel — P0 never creates a new turn.
    NoLiveSession,
    /// Provider is not claude/codex.
    Unsupported,
    /// Reserved for cross-runtime resolution failure (P1 reuse).
    #[allow(dead_code)]
    DiscordUnavailable,
    /// Reserved: channel resolved but no session record (P1 reuse).
    #[allow(dead_code)]
    SessionNotFound,
}

/// P0 capability gate: claude + codex only. Mirrors `provider_supports_resume`
/// (`commands/restart.rs`) in spirit but uses an explicit `matches!` because
/// there is no `supports_steering` capability and the requirement is exactly
/// these two providers, whose live-followup path is already validated.
pub(in crate::services::discord) fn provider_supports_steering(provider: &ProviderKind) -> bool {
    matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
}

/// Derive the single steer source id from the Discord interaction id. Pure, so
/// REQ-004 ("the interaction id is the single source") stays pinned by a test
/// independently of the un-mockable poise `Context`. `interaction_id` is always
/// non-zero (serenity `InteractionId` is `NonZeroU64`-backed), so `MessageId::new`
/// cannot hit its zero-panic path on the slash-command path.
pub(in crate::services::discord) fn steer_source_id(interaction_id: u64) -> MessageId {
    MessageId::new(interaction_id)
}

/// Build the standalone `Soft` intervention for a steer request. Pure; pinned by
/// unit tests so the never-merged / single-source-id contract can't drift.
fn build_steer_intervention(request: &SteeringRequest) -> Intervention {
    Intervention {
        author_id: request.author_id,
        author_is_bot: false,
        message_id: request.source_id,
        source_message_ids: vec![request.source_id],
        text: request.instruction.clone(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// Map a `mailbox_enqueue_intervention` result to a `SteeringOutcome`. Pure.
fn classify_enqueue_result(
    enqueued: bool,
    refusal_reason: Option<EnqueueRefusalReason>,
) -> SteeringOutcome {
    if enqueued {
        return SteeringOutcome::Queued;
    }
    // `enqueued == false` ⇒ a refusal reason is present (MailboxEnqueueOutcome
    // invariant); fall back to ActorUnreachable if one is somehow absent.
    SteeringOutcome::Refused {
        reason: refusal_reason.unwrap_or(EnqueueRefusalReason::ActorUnreachable),
    }
}

/// The single enqueue path for `/steer`. Gates BEFORE enqueue, then reuses the
/// existing per-channel mailbox. Never creates a new turn.
pub(in crate::services::discord) async fn enqueue_steering(
    shared: &SharedData,
    request: SteeringRequest,
) -> SteeringOutcome {
    if !provider_supports_steering(&request.provider) {
        return SteeringOutcome::Unsupported;
    }
    // Gate BEFORE enqueue so a steer never starts a fresh turn. This is a
    // best-effort read (one actor hop before the enqueue): if the live turn
    // happens to finish in that window, the steer is left on the queue and the
    // normal idle-kickoff path may later dispatch it as an ordinary queued
    // message — the same benign degradation any queued intervention has. The
    // enqueue itself never starts a turn.
    if !mailbox_has_active_turn(shared, request.channel_id).await {
        return SteeringOutcome::NoLiveSession;
    }

    let intervention = build_steer_intervention(&request);
    let outcome =
        mailbox_enqueue_intervention(shared, &request.provider, request.channel_id, intervention)
            .await;

    if let Some(error) = outcome.persistence_error.as_ref() {
        // In-memory enqueue still succeeded; durable persistence is best-effort.
        tracing::error!(
            provider = request.provider.as_str(),
            channel_id = request.channel_id.get(),
            error = %error,
            "/steer enqueue durable persistence failed (in-memory enqueue unaffected)"
        );
    }

    classify_enqueue_result(outcome.enqueued, outcome.refusal_reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(instruction: &str) -> SteeringRequest {
        SteeringRequest {
            channel_id: ChannelId::new(3_038_400),
            provider: ProviderKind::Claude,
            author_id: UserId::new(42),
            source_id: MessageId::new(7_777),
            instruction: instruction.to_string(),
        }
    }

    #[test]
    fn provider_gate_allows_only_claude_and_codex() {
        assert!(provider_supports_steering(&ProviderKind::Claude));
        assert!(provider_supports_steering(&ProviderKind::Codex));
        assert!(!provider_supports_steering(&ProviderKind::Gemini));
        assert!(!provider_supports_steering(&ProviderKind::OpenCode));
        assert!(!provider_supports_steering(&ProviderKind::Qwen));
        assert!(!provider_supports_steering(&ProviderKind::Unsupported(
            "x".to_string()
        )));
    }

    #[test]
    fn steer_source_id_wraps_interaction_id() {
        assert_eq!(steer_source_id(987_654), MessageId::new(987_654));
    }

    #[test]
    fn steer_intervention_is_standalone_with_single_source_id() {
        let req = request("run the tests first");
        let intervention = build_steer_intervention(&req);
        assert_eq!(intervention.message_id, req.source_id);
        assert_eq!(intervention.source_message_ids, vec![req.source_id]);
        assert!(!intervention.merge_consecutive);
        assert_eq!(intervention.mode, InterventionMode::Soft);
        assert_eq!(intervention.text, "run the tests first");
        assert!(intervention.reply_context.is_none());
        assert!(!intervention.has_reply_boundary);
        assert!(intervention.pending_uploads.is_empty());
        assert!(intervention.voice_announcement.is_none());
        assert!(!intervention.author_is_bot);
    }

    #[test]
    fn enqueue_result_classification_covers_queued_and_refusals() {
        assert_eq!(classify_enqueue_result(true, None), SteeringOutcome::Queued);
        for reason in [
            EnqueueRefusalReason::SourceIdAlreadyQueued,
            EnqueueRefusalReason::LastItemDedup,
            EnqueueRefusalReason::ActorUnreachable,
            EnqueueRefusalReason::MailboxClosed,
        ] {
            assert_eq!(
                classify_enqueue_result(false, Some(reason)),
                SteeringOutcome::Refused { reason }
            );
        }
        // enqueued=false with no reason falls back to ActorUnreachable.
        assert_eq!(
            classify_enqueue_result(false, None),
            SteeringOutcome::Refused {
                reason: EnqueueRefusalReason::ActorUnreachable
            }
        );
    }
}
