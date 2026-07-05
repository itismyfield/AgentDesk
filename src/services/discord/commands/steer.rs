//! `/steer <instruction>` (P0) — enqueue a steering instruction onto the
//! channel's already-live turn. Never creates a new turn; non-claude/codex
//! providers and idle channels are refused with a structured reply.

use poise::CreateReply;
use poise::serenity_prelude as serenity;

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::EnqueueRefusalReason;

use super::super::steering::{
    SteerLifecycle, SteeringOutcome, SteeringRequest, enqueue_steering, steer_cancel_custom_id,
    steer_lifecycle_label, steer_source_id,
};
use super::super::{Context, Error, check_auth};
use super::config::{effective_provider_for_channel, fallback_channel_name_for_feature_toggle};

// Reuses the existing mailbox intervention path; never starts a new turn.
/// Inject a steering instruction into the channel's live turn (claude/codex).
#[poise::command(slash_command, rename = "steer")]
pub(in crate::services::discord) async fn cmd_steer(
    ctx: Context<'_>,
    #[description = "Steering instruction to inject into the live turn"] instruction: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    if !super::enforce_slash_command_policy(&ctx, "/steer").await? {
        return Ok(());
    }

    let instruction = instruction.trim().to_string();
    if instruction.is_empty() {
        ctx.say("Usage: `/steer <instruction>`").await?;
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /steer");

    let channel_id = ctx.channel_id();

    // Effective per-channel provider (honors role overrides), same resolution as
    // `/skill` and `/restart`.
    let channel_name_hint = fallback_channel_name_for_feature_toggle(ctx, channel_id).await;
    let provider: ProviderKind = effective_provider_for_channel(
        &ctx.data().shared,
        channel_id,
        &ctx.data().provider,
        channel_name_hint.as_deref(),
    )
    .await;

    // Single source id == the Discord interaction id, unique per invocation.
    let source_id = steer_source_id(ctx.id());

    let request = SteeringRequest {
        channel_id,
        provider,
        author_id: user_id,
        source_id,
        instruction: instruction.clone(),
    };

    // State 1 (REQ-013): show `큐잉중인 입력 : <instruction>` immediately and log
    // the same label to the AgentDesk console (visible in the tmux pane, REQ-015).
    let queuing_label = steer_lifecycle_label(SteerLifecycle::Queuing, &instruction);
    tracing::info!("  [{ts}] 🔀 STEER {queuing_label}");
    let handle = ctx
        .send(CreateReply::default().content(queuing_label))
        .await?;

    // Persist under THIS bot instance's provider (same as the normal
    // soft-intervention enqueue and the cancel path) so a later cancel clears
    // the same durable queue file. The claude/codex support gate inside
    // `enqueue_steering` still uses the effective per-channel provider.
    let outcome = enqueue_steering(&ctx.data().shared, &ctx.data().provider, request).await;

    match outcome {
        SteeringOutcome::Queued => {
            // State 2 (REQ-013/REQ-014): `큐잉됨 : <instruction>` + cancel button.
            // The actual instruction is delivered to the agent's tmux session by
            // the existing turn-boundary path; nothing is typed into the composer
            // here (REQ-016).
            let queued_label = steer_lifecycle_label(SteerLifecycle::Queued, &instruction);
            let queued_ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{queued_ts}] 📬 STEER {queued_label}");
            let cancel_row = serenity::CreateActionRow::Buttons(vec![
                serenity::CreateButton::new(steer_cancel_custom_id(source_id))
                    .style(serenity::ButtonStyle::Secondary)
                    .label("취소"),
            ]);
            handle
                .edit(
                    ctx,
                    CreateReply::default()
                        .content(queued_label)
                        .components(vec![cancel_row]),
                )
                .await?;
        }
        SteeringOutcome::NoLiveSession => {
            handle
                .edit(
                    ctx,
                    CreateReply::default().content(
                        "진행 중인 턴이 없습니다. `/steer`는 활성 턴에만 사용할 수 있습니다.",
                    ),
                )
                .await?;
        }
        SteeringOutcome::Unsupported => {
            handle
                .edit(
                    ctx,
                    CreateReply::default().content(
                        "이 provider는 `/steer`를 지원하지 않습니다 (claude / codex 전용).",
                    ),
                )
                .await?;
        }
        SteeringOutcome::Refused { reason } => {
            let message = match reason {
                EnqueueRefusalReason::AlreadyActiveTurn
                | EnqueueRefusalReason::SourceIdAlreadyQueued
                | EnqueueRefusalReason::LastItemDedup => "중복으로 판단되어 큐잉되지 않았습니다.",
                EnqueueRefusalReason::ActorUnreachable | EnqueueRefusalReason::MailboxClosed => {
                    "세션 액터에 접근할 수 없어 일시적으로 실패했습니다. 잠시 후 다시 시도하세요."
                }
            };
            handle
                .edit(ctx, CreateReply::default().content(message))
                .await?;
        }
        SteeringOutcome::DiscordUnavailable | SteeringOutcome::SessionNotFound => {
            handle
                .edit(
                    ctx,
                    CreateReply::default().content("세션을 확인할 수 없습니다."),
                )
                .await?;
        }
    }
    Ok(())
}
