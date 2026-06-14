//! `/steer <instruction>` (P0) — enqueue a steering instruction onto the
//! channel's already-live turn. Never creates a new turn; non-claude/codex
//! providers and idle channels are refused with a structured reply.

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::EnqueueRefusalReason;

use super::super::steering::{SteeringOutcome, SteeringRequest, enqueue_steering, steer_source_id};
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
        instruction,
    };

    let outcome = enqueue_steering(&ctx.data().shared, request).await;

    let reply = match outcome {
        SteeringOutcome::Queued => "조종 지시를 현재 진행 중인 턴에 큐잉했습니다.",
        SteeringOutcome::NoLiveSession => {
            "진행 중인 턴이 없습니다. `/steer`는 활성 턴에만 사용할 수 있습니다."
        }
        SteeringOutcome::Unsupported => {
            "이 provider는 `/steer`를 지원하지 않습니다 (claude / codex 전용)."
        }
        SteeringOutcome::Refused { reason } => match reason {
            EnqueueRefusalReason::SourceIdAlreadyQueued | EnqueueRefusalReason::LastItemDedup => {
                "중복으로 판단되어 큐잉되지 않았습니다."
            }
            EnqueueRefusalReason::ActorUnreachable | EnqueueRefusalReason::MailboxClosed => {
                "세션 액터에 접근할 수 없어 일시적으로 실패했습니다. 잠시 후 다시 시도하세요."
            }
        },
        SteeringOutcome::DiscordUnavailable | SteeringOutcome::SessionNotFound => {
            "세션을 확인할 수 없습니다."
        }
    };
    ctx.say(reply).await?;
    Ok(())
}
