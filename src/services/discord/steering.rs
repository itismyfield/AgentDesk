//! `/steer` (P0): inject a steering instruction into a channel's already-live
//! turn by enqueuing a standalone, never-merged `Soft` intervention.
//!
//! P0 SCOPE: current-channel only. No HTTP API, no DB helpers, no
//! `session_forwarding`, and no new delivery core. This is a thin wrapper over
//! the existing private `mailbox_enqueue_intervention` / `mailbox_has_active_turn`
//! in the parent `discord` module, reachable here via `super::` because
//! `steering` is a child module.

use std::{sync::Arc, time::Instant};

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId, UserId};

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{EnqueueRefusalReason, Intervention, InterventionMode};

use super::{
    Data, Error, SharedData, check_auth, mailbox_cancel_soft_intervention,
    mailbox_enqueue_intervention, mailbox_has_active_turn,
};

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

/// Custom-id prefix for the `/steer` cancel button. The message-component router
/// in `intake_gate.rs::handle_event` matches on this prefix, mirroring the
/// idle-recap clear button (`idle_recap_interaction.rs`).
pub(in crate::services::discord) const STEER_CANCEL_CUSTOM_ID_PREFIX: &str = "steer:cancel:";

/// Build the cancel-button `custom_id` for a steer source id. The source id is
/// the steer intervention's `message_id` (== the Discord interaction id), so the
/// cancel handler can target the queued intervention directly.
pub(in crate::services::discord) fn steer_cancel_custom_id(source_id: MessageId) -> String {
    format!("{STEER_CANCEL_CUSTOM_ID_PREFIX}{}", source_id.get())
}

/// True if a message-component `custom_id` belongs to a `/steer` cancel button.
pub(in crate::services::discord) fn is_steer_cancel_custom_id(custom_id: &str) -> bool {
    custom_id.starts_with(STEER_CANCEL_CUSTOM_ID_PREFIX)
}

/// Parse the steer source id out of a cancel `custom_id`. Rejects a missing
/// prefix, a non-numeric tail, and the zero sentinel so the cancel handler can
/// never target `MessageId(0)`.
pub(in crate::services::discord) fn parse_steer_cancel_source_id(
    custom_id: &str,
) -> Option<MessageId> {
    custom_id
        .strip_prefix(STEER_CANCEL_CUSTOM_ID_PREFIX)
        .and_then(|tail| tail.parse::<u64>().ok())
        .filter(|id| *id != 0)
        .map(MessageId::new)
}

/// Operator-visible queue lifecycle phase for a `/steer` invocation. The label
/// is a fixed contract (REQ-013): `<상태> : <instruction>` is rendered
/// identically in the Discord card and the AgentDesk console (tmux) log so an
/// operator watching either surface sees the same three states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum SteerLifecycle {
    /// Input is being queued (initial slash reply).
    Queuing,
    /// Input has been queued onto the live turn's mailbox.
    Queued,
    /// Queued input was cancelled before delivery.
    Cancelled,
}

impl SteerLifecycle {
    fn state_label(self) -> &'static str {
        match self {
            SteerLifecycle::Queuing => "큐잉중인 입력",
            SteerLifecycle::Queued => "큐잉됨",
            SteerLifecycle::Cancelled => "큐잉이 취소됨",
        }
    }
}

/// Max instruction characters rendered into a lifecycle label. Keeps the Discord
/// card and the log line bounded; the full instruction is still delivered to the
/// agent via the existing turn-boundary path (REQ-016).
const STEER_LABEL_MAX_INSTRUCTION_CHARS: usize = 1500;

/// Render the fixed `<상태> : <instruction>` lifecycle label (REQ-013/REQ-015).
/// The instruction is truncated for display only.
pub(in crate::services::discord) fn steer_lifecycle_label(
    phase: SteerLifecycle,
    instruction: &str,
) -> String {
    format!(
        "{} : {}",
        phase.state_label(),
        truncate_instruction_for_label(instruction)
    )
}

fn truncate_instruction_for_label(instruction: &str) -> String {
    let mut chars = instruction.chars();
    let head: String = chars
        .by_ref()
        .take(STEER_LABEL_MAX_INSTRUCTION_CHARS)
        .collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Build the standalone `Soft` intervention for a steer request. Pure; pinned by
/// unit tests so the never-merged / single-source-id contract can't drift.
fn build_steer_intervention(request: &SteeringRequest) -> Intervention {
    let queued_generation = crate::services::discord::runtime_store::load_generation();
    Intervention {
        author_id: request.author_id,
        author_is_bot: false,
        message_id: request.source_id,
        queued_generation,
        source_message_ids: vec![request.source_id],
        source_message_queued_generations: vec![
            crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                request.source_id,
                queued_generation,
            ),
        ],
        source_text_segments: Vec::new(),
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
///
/// `request.provider` is the EFFECTIVE per-channel provider and is used only for
/// the claude/codex support gate. `persist_provider` is the bot instance's own
/// provider (`Data.provider`) and is what keys the durable queue file. The two
/// can differ under a cross-provider role override; persistence MUST use
/// `persist_provider` so it matches the durable path that every other enqueue
/// path (normal soft-intervention enqueue) and the cancel path
/// (`mailbox_cancel_soft_intervention`) use. Otherwise a cancel — which always
/// runs with `Data.provider` — would clear a different file than the enqueue
/// wrote, resurrecting a cancelled steer on restart.
pub(in crate::services::discord) async fn enqueue_steering(
    shared: &Arc<SharedData>,
    persist_provider: &ProviderKind,
    request: SteeringRequest,
) -> SteeringOutcome {
    if !provider_supports_steering(&request.provider) {
        return SteeringOutcome::Unsupported;
    }
    // Gate BEFORE enqueue so a steer targets an observed live session. This is a
    // best-effort read (one actor hop before the enqueue): if the live turn
    // finishes in that window, the generic enqueue helper performs the #4048
    // enqueue-then-snapshot kick so the steer does not strand behind an already
    // fired completion event.
    if !mailbox_has_active_turn(shared, request.channel_id).await {
        return SteeringOutcome::NoLiveSession;
    }

    let intervention = build_steer_intervention(&request);
    let outcome =
        mailbox_enqueue_intervention(shared, persist_provider, request.channel_id, intervention)
            .await;

    if let Some(error) = outcome.persistence_error.as_ref() {
        // In-memory enqueue still succeeded; durable persistence is best-effort.
        tracing::error!(
            provider = persist_provider.as_str(),
            channel_id = request.channel_id.get(),
            error = %error,
            "/steer enqueue durable persistence failed (in-memory enqueue unaffected)"
        );
    }

    classify_enqueue_result(outcome.enqueued, outcome.refusal_reason)
}

/// Interaction handler for the `/steer` cancel button (P1.5). The button's
/// `custom_id` is `steer:cancel:<source_id>`, where `source_id` is the steer
/// intervention's `message_id` (== the Discord interaction id). On click we
/// authorise the user, then call the existing `mailbox_cancel_soft_intervention`
/// to remove the still-queued intervention before delivery, and edit the card
/// into the `큐잉이 취소됨 : <instruction>` state. No new cancel core is added —
/// this mirrors `idle_recap_interaction.rs` and reuses the reaction-cancel path.
pub(in crate::services::discord) async fn handle_steer_cancel_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    // Authorise with the same gate `/steer` uses. Without this, anyone able to
    // see the card could cancel another operator's steer.
    let user_id = component.user.id;
    let user_name = &component.user.name;
    if !check_auth(user_id, user_name, &data.shared, &data.token).await {
        let _ = component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("Not authorized for this bot.")
                        .ephemeral(true),
                ),
            )
            .await;
        return Ok(());
    }

    let Some(source_id) = parse_steer_cancel_source_id(&component.data.custom_id) else {
        // Malformed / sentinel id — acknowledge so the client doesn't time out.
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };

    let channel_id = component.channel_id;
    // Reuse the existing soft-intervention cancel path (same as reaction-remove).
    // It removes the queued intervention and fires `apply_queue_exit_feedback`.
    // Persist under `data.provider` to match the path `/steer` enqueued under.
    let removed =
        mailbox_cancel_soft_intervention(&data.shared, &data.provider, channel_id, source_id).await;

    match removed {
        Some(intervention) => {
            // State 3 (REQ-013): `큐잉이 취소됨 : <instruction>`. The instruction is
            // recovered from the removed intervention so the label is exact even
            // though the custom_id carries only the source id.
            let cancelled_label =
                steer_lifecycle_label(SteerLifecycle::Cancelled, &intervention.text);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📭 STEER-CANCEL: {cancelled_label} (channel {})",
                channel_id.get()
            );
            let _ = component
                .create_response(
                    ctx,
                    serenity::CreateInteractionResponse::UpdateMessage(
                        serenity::CreateInteractionResponseMessage::new()
                            .content(cancelled_label)
                            .components(Vec::new()),
                    ),
                )
                .await;
        }
        None => {
            // Already delivered or already cancelled — idempotent. Strip the
            // button so the stale card cannot be clicked again, but do not claim
            // a cancellation for an instruction that already reached the agent.
            let _ = component
                .create_response(
                    ctx,
                    serenity::CreateInteractionResponse::UpdateMessage(
                        serenity::CreateInteractionResponseMessage::new().components(Vec::new()),
                    ),
                )
                .await;
        }
    }

    Ok(())
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
            EnqueueRefusalReason::AlreadyActiveTurn,
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

    #[test]
    fn steer_cancel_custom_id_round_trips() {
        let id = MessageId::new(123_456_789);
        let cid = steer_cancel_custom_id(id);
        assert_eq!(cid, "steer:cancel:123456789");
        assert!(is_steer_cancel_custom_id(&cid));
        assert_eq!(parse_steer_cancel_source_id(&cid), Some(id));
    }

    #[test]
    fn steer_cancel_custom_id_rejects_foreign_and_zero_and_garbage() {
        assert!(!is_steer_cancel_custom_id("idle_recap:clear:1"));
        assert!(!is_steer_cancel_custom_id("agentdesk:model-cancel:1"));
        assert_eq!(parse_steer_cancel_source_id("steer:cancel:0"), None);
        assert_eq!(parse_steer_cancel_source_id("steer:cancel:abc"), None);
        assert_eq!(parse_steer_cancel_source_id("steer:cancel:"), None);
        assert_eq!(parse_steer_cancel_source_id("nope"), None);
    }

    #[test]
    fn steer_lifecycle_label_uses_fixed_three_state_format() {
        assert_eq!(
            steer_lifecycle_label(SteerLifecycle::Queuing, "계속 진행해줘"),
            "큐잉중인 입력 : 계속 진행해줘"
        );
        assert_eq!(
            steer_lifecycle_label(SteerLifecycle::Queued, "계속 진행해줘"),
            "큐잉됨 : 계속 진행해줘"
        );
        assert_eq!(
            steer_lifecycle_label(SteerLifecycle::Cancelled, "계속 진행해줘"),
            "큐잉이 취소됨 : 계속 진행해줘"
        );
    }

    #[test]
    fn steer_lifecycle_label_truncates_long_instruction_for_display() {
        let long = "가".repeat(2_000);
        let label = steer_lifecycle_label(SteerLifecycle::Queued, &long);
        assert!(label.starts_with("큐잉됨 : "));
        assert!(label.ends_with('…'));
        // Exactly STEER_LABEL_MAX_INSTRUCTION_CHARS instruction chars are kept.
        assert_eq!(label.chars().filter(|c| *c == '가').count(), 1_500);
    }
}
