//! Interaction handlers for idle-recap card buttons.
//!
//! The button's `custom_id` looks like `idle_recap:clear:<message_id>`. We
//! resolve the message id back to a `session_key` via the
//! `sessions.idle_recap_message_id` index, route through the same clear path
//! as `/clear`, and delete the recap card.

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use super::{Data, Error, check_auth};
use crate::services::discord::idle_recap::{
    IDLE_RECAP_CLEAR_BUTTON_PREFIX, IDLE_RECAP_COMPACT_BUTTON_PREFIX,
    IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX, IDLE_RECAP_SUGGEST_BUTTON_PREFIX, clear_recap_pointer,
    delete_previous_card,
};
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecapClearTarget {
    session_key: String,
    provider: String,
    owner_instance_id: Option<String>,
    turn_generation: i64,
    channel_matches: bool,
    provider_matches: bool,
    recap_current: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NativeCompactRequest {
    tmux_session_name: String,
    prompt: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RecapCompactOutcome {
    Started(NativeCompactRequest),
    AlreadyClaimed,
    ClaimFailed(String),
    PostClaimOwnerChanged,
    PostClaimFenceFailed(String),
    InvalidTarget,
    RoutingUnavailable {
        owner_instance_id: Option<String>,
        reason: String,
    },
    TargetNotLive(NativeCompactRequest),
    InjectionFailed {
        request: NativeCompactRequest,
        error: String,
    },
}

impl RecapCompactOutcome {
    fn preserves_recap_card(&self) -> bool {
        matches!(
            self,
            Self::AlreadyClaimed
                | Self::ClaimFailed(_)
                | Self::InvalidTarget
                | Self::RoutingUnavailable { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecapPromptRoute {
    NativeSlashCompact,
    InternalFollowup,
}

fn recap_prompt_route(custom_id: &str) -> Option<RecapPromptRoute> {
    if custom_id.starts_with(IDLE_RECAP_COMPACT_BUTTON_PREFIX) {
        Some(RecapPromptRoute::NativeSlashCompact)
    } else if custom_id.starts_with(IDLE_RECAP_SUGGEST_BUTTON_PREFIX) {
        Some(RecapPromptRoute::InternalFollowup)
    } else {
        None
    }
}

/// True if `custom_id` belongs to the idle-recap clear button.
pub(super) fn is_idle_recap_clear_custom_id(custom_id: &str) -> bool {
    custom_id.starts_with(IDLE_RECAP_CLEAR_BUTTON_PREFIX)
}

pub(super) fn is_idle_recap_custom_id(custom_id: &str) -> bool {
    is_idle_recap_clear_custom_id(custom_id)
        || custom_id.starts_with(IDLE_RECAP_COMPACT_BUTTON_PREFIX)
        || custom_id.starts_with(IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX)
        || custom_id.starts_with(IDLE_RECAP_SUGGEST_BUTTON_PREFIX)
}

pub(super) async fn handle_idle_recap_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let custom_id = &component.data.custom_id;
    if custom_id.starts_with(IDLE_RECAP_CLEAR_BUTTON_PREFIX) {
        return handle_idle_recap_clear_interaction(ctx, component, data).await;
    }
    if custom_id.starts_with(IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX) {
        return handle_idle_recap_relay_diag_interaction(ctx, component, data).await;
    }
    match recap_prompt_route(custom_id) {
        Some(RecapPromptRoute::NativeSlashCompact) => {
            return handle_idle_recap_compact_interaction(ctx, component, data).await;
        }
        Some(RecapPromptRoute::InternalFollowup) => {
            return handle_idle_recap_suggest_interaction(ctx, component, data).await;
        }
        None => {}
    }
    let _ = component
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await;
    Ok(())
}

pub(super) async fn handle_idle_recap_clear_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    // Authorise the click. Without this, anyone who can see the recap
    // card (= anyone with read access to the bound Discord channel) could
    // drop the provider session id. Reuses the same auth gate that the
    // `/clear` slash command goes through (see commands::control::clear).
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

    let custom_id = &component.data.custom_id;
    let Some(message_id) = parse_message_id(custom_id, IDLE_RECAP_CLEAR_BUTTON_PREFIX) else {
        // Unknown / sentinel id ("0") — happens during the brief window
        // before post_recap_card rewrites the placeholder button to the
        // real id. Acknowledge so the client doesn't time out.
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };

    let Some(pool) = data.shared.pg_pool.as_ref().cloned() else {
        let _ = component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("세션 정리 실패: DB 연결 없음.")
                        .ephemeral(true),
                ),
            )
            .await;
        return Ok(());
    };

    let clear_target = match lookup_recap_clear_target(
        &pool,
        message_id,
        component.channel_id.get(),
        data.provider.as_str(),
    )
    .await
    {
        Ok(Some(target)) => target,
        Ok(None) => {
            // Card already cleared (compare-and-clear path won the race
            // with a fresh-cycle post) — silently acknowledge.
            let _ = component
                .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
                .await;
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                message_id = message_id,
                "idle_recap clear: target lookup failed"
            );
            let _ = component
                .create_response(
                    ctx,
                    serenity::CreateInteractionResponse::Message(
                        serenity::CreateInteractionResponseMessage::new()
                            .content("세션 정리 실패. 잠시 후 다시 시도하세요.")
                            .ephemeral(true),
                    ),
                )
                .await;
            return Ok(());
        }
    };

    let _ = component
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await;

    if !clear_target.channel_matches
        || !clear_target.provider_matches
        || !clear_target.recap_current
    {
        let _ = clear_recap_pointer(&pool, &clear_target.session_key, message_id).await;
        delete_previous_card(&ctx.http, component.channel_id.get(), message_id).await;
        return Ok(());
    }

    // Compare-and-clear the recap pointer for this session, then delete
    // the card. Order matters: clear the pointer first so the
    // user-message hook (intake_gate) doesn't try to delete the same
    // message at the same time.
    let pointer_cleared = clear_recap_pointer(&pool, &clear_target.session_key, message_id)
        .await
        .unwrap_or(false);
    let channel_id = component.channel_id.get();
    if !pointer_cleared {
        delete_previous_card(&ctx.http, channel_id, message_id).await;
        return Ok(());
    }

    // Reuse `/clear` semantics, not just the provider-session-id drop. TUI
    // providers keep live tmux/process state that must be reset too.
    crate::services::discord::commands::clear_channel_session_state_with_session_key(
        &ctx.http,
        &data.shared,
        &data.provider,
        component.channel_id,
        "idle_recap_clear",
        crate::services::discord::commands::SoftClearNotifyMode::Enqueue,
        Some(&clear_target.session_key),
    )
    .await?;
    delete_previous_card(&ctx.http, channel_id, message_id).await;

    Ok(())
}

async fn handle_idle_recap_relay_diag_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    if !authorize_component(ctx, component, data).await {
        return Ok(());
    }

    let Some(message_id) = parse_message_id(
        &component.data.custom_id,
        IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX,
    ) else {
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };
    let Some(pool) = data.shared.pg_pool.as_ref().cloned() else {
        send_ephemeral(ctx, component, "릴레이 진단 실패: DB 연결 없음.").await;
        return Ok(());
    };
    let Some(target) = lookup_current_recap_target(
        &pool,
        message_id,
        component.channel_id.get(),
        data.provider.as_str(),
    )
    .await?
    else {
        send_ephemeral(
            ctx,
            component,
            "릴레이 진단 대상이 더 이상 유효하지 않습니다.",
        )
        .await;
        return Ok(());
    };
    let Some(snapshot) =
        crate::services::discord::idle_recap::load_recap_snapshot(&pool, &target.session_key)
            .await?
    else {
        send_ephemeral(ctx, component, "릴레이 진단 실패: 세션을 찾을 수 없습니다.").await;
        return Ok(());
    };
    let Some(provider) = ProviderKind::from_str(&snapshot.provider) else {
        send_ephemeral(
            ctx,
            component,
            "릴레이 진단 실패: provider를 확인할 수 없습니다.",
        )
        .await;
        return Ok(());
    };
    let probe = crate::services::discord::idle_recap::probe_relay_integrity(
        &snapshot,
        &provider,
        component.channel_id.get(),
        Some(message_id),
    );
    let report = truncate_interaction_body(&probe.diagnostic_report());
    send_ephemeral(ctx, component, &format!("```text\n{report}\n```")).await;
    Ok(())
}

async fn handle_idle_recap_compact_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    if !authorize_component(ctx, component, data).await {
        return Ok(());
    }

    let Some(message_id) =
        parse_message_id(&component.data.custom_id, IDLE_RECAP_COMPACT_BUTTON_PREFIX)
    else {
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };
    let Some(pool) = data.shared.pg_pool.as_ref().cloned() else {
        send_ephemeral(ctx, component, "맥락 압축 요청 실패: DB 연결 없음.").await;
        return Ok(());
    };
    let target = match lookup_current_recap_target(
        &pool,
        message_id,
        component.channel_id.get(),
        data.provider.as_str(),
    )
    .await
    {
        Ok(Some(target)) => target,
        Ok(None) => {
            send_ephemeral(
                ctx,
                component,
                "맥락 압축 대상이 더 이상 유효하지 않거나 이미 처리되었습니다.",
            )
            .await;
            return Ok(());
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                message_id,
                "idle_recap compact: target lookup failed"
            );
            send_ephemeral(ctx, component, "맥락 압축 요청 실패: 세션 확인 오류.").await;
            return Ok(());
        }
    };

    if let Err(error) = component.defer_ephemeral(ctx).await {
        tracing::warn!(
            error = %error,
            message_id,
            "idle_recap compact: failed to defer interaction response"
        );
        return Ok(());
    }

    let target_for_claim = target.clone();
    let target_for_pre_inject_fence = target.clone();
    let target_session_key_for_log = target.session_key.clone();
    let channel_id = component.channel_id.get();
    let pool_for_claim = pool.clone();
    let pool_for_pre_inject_fence = pool.clone();
    let outcome = run_native_compact_once(
        target,
        &crate::services::platform::hostname_short(),
        &crate::services::cluster::node_registry::resolve_self_instance_id_without_config(),
        move || async move {
            claim_recap_compact_pointer(&pool_for_claim, &target_for_claim, message_id, channel_id)
                .await
                .map_err(|error| error.to_string())
        },
        move || async move {
            recap_compact_owner_unchanged(&pool_for_pre_inject_fence, &target_for_pre_inject_fence)
                .await
                .map_err(|error| error.to_string())
        },
        |tmux_session_name| {
            std::future::ready(
                crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name),
            )
        },
        |request| async move { inject_native_compact(request).await },
    )
    .await;

    let response = match &outcome {
        RecapCompactOutcome::Started(_) => "Claude 맥락 압축을 시작했습니다.",
        RecapCompactOutcome::AlreadyClaimed => {
            "맥락 압축 요청이 이미 처리되었거나 대상이 더 이상 유효하지 않습니다."
        }
        RecapCompactOutcome::ClaimFailed(error) => {
            tracing::warn!(
                error = %error,
                message_id,
                session_key = %target_session_key_for_log,
                "idle_recap compact: atomic claim failed"
            );
            "맥락 압축 요청 실패: 세션 선점 오류. /compact를 직접 실행하세요."
        }
        RecapCompactOutcome::PostClaimOwnerChanged => {
            tracing::warn!(
                message_id,
                session_key = %target_session_key_for_log,
                "idle_recap compact: owner changed after atomic claim"
            );
            "맥락 압축 요청 실패: 세션이 다른 노드로 이동했습니다. 원래 세션 노드에서 /compact를 실행하세요."
        }
        RecapCompactOutcome::PostClaimFenceFailed(error) => {
            tracing::warn!(
                error = %error,
                message_id,
                session_key = %target_session_key_for_log,
                "idle_recap compact: post-claim owner fence failed"
            );
            "맥락 압축 요청 실패: 세션 소유권을 재확인할 수 없습니다. /compact를 직접 실행하세요."
        }
        RecapCompactOutcome::InvalidTarget => {
            "맥락 압축 요청 실패: recap 세션 대상을 확인할 수 없습니다."
        }
        RecapCompactOutcome::RoutingUnavailable {
            owner_instance_id,
            reason,
        } => {
            tracing::warn!(
                message_id,
                owner_instance_id = owner_instance_id.as_deref().unwrap_or("unknown"),
                reason,
                "idle_recap compact: recap owner is not local"
            );
            "맥락 압축 요청을 현재 노드에서 처리할 수 없습니다. 원래 세션 노드에서 다시 시도하세요."
        }
        RecapCompactOutcome::TargetNotLive(request) => {
            tracing::warn!(
                message_id,
                tmux_session_name = %request.tmux_session_name,
                "idle_recap compact: claimed target has no live pane"
            );
            "맥락 압축 요청 실패: 원래 Claude 세션이 종료되었습니다."
        }
        RecapCompactOutcome::InjectionFailed { request, error } => {
            tracing::warn!(
                error = %error,
                message_id,
                tmux_session_name = %request.tmux_session_name,
                "idle_recap compact: terminal native /compact injection failure"
            );
            "맥락 압축 요청을 전송하지 못했습니다. 중복 방지를 위해 요청을 종료했습니다. /compact를 직접 실행하세요."
        }
    };
    edit_deferred_ephemeral(ctx, component, message_id, response).await;

    if !outcome.preserves_recap_card() {
        delete_previous_card(&ctx.http, channel_id, message_id).await;
    }
    Ok(())
}

async fn run_native_compact_once<
    Claim,
    ClaimFut,
    PreInjectFence,
    PreInjectFenceFut,
    IsLive,
    IsLiveFut,
    Inject,
    InjectFut,
>(
    target: RecapClearTarget,
    local_hostname: &str,
    local_instance_id: &str,
    claim: Claim,
    pre_inject_fence: PreInjectFence,
    is_live: IsLive,
    inject: Inject,
) -> RecapCompactOutcome
where
    Claim: FnOnce() -> ClaimFut,
    ClaimFut: std::future::Future<Output = Result<bool, String>>,
    PreInjectFence: FnOnce() -> PreInjectFenceFut,
    PreInjectFenceFut: std::future::Future<Output = Result<bool, String>>,
    IsLive: FnOnce(&str) -> IsLiveFut,
    IsLiveFut: std::future::Future<Output = bool>,
    Inject: FnOnce(NativeCompactRequest) -> InjectFut,
    InjectFut: std::future::Future<Output = Result<(), String>>,
{
    let Some(identity) =
        crate::services::discord::session_identity::SessionIdentity::parse(&target.session_key)
    else {
        return RecapCompactOutcome::InvalidTarget;
    };
    let routing = crate::services::cluster::session_routing::session_owner_routing_status(
        target.owner_instance_id.as_deref(),
        Some(local_instance_id),
        &[],
    );
    if identity.host != local_hostname || routing["is_local"].as_bool() != Some(true) {
        let reason = if identity.host != local_hostname {
            "session_key_host_mismatch"
        } else {
            routing["reason"]
                .as_str()
                .unwrap_or("session_owner_not_local")
        };
        return RecapCompactOutcome::RoutingUnavailable {
            owner_instance_id: target.owner_instance_id.clone(),
            reason: reason.to_string(),
        };
    }

    match claim().await {
        Ok(true) => {}
        Ok(false) => return RecapCompactOutcome::AlreadyClaimed,
        Err(error) => return RecapCompactOutcome::ClaimFailed(error),
    }

    let request = NativeCompactRequest {
        tmux_session_name: identity.tmux_name,
        prompt: "/compact",
    };
    if !is_live(&request.tmux_session_name).await {
        return RecapCompactOutcome::TargetNotLive(request);
    }
    match pre_inject_fence().await {
        Ok(true) => {}
        Ok(false) => return RecapCompactOutcome::PostClaimOwnerChanged,
        Err(error) => return RecapCompactOutcome::PostClaimFenceFailed(error),
    }

    match inject(request.clone()).await {
        Ok(()) => RecapCompactOutcome::Started(request),
        Err(error) => RecapCompactOutcome::InjectionFailed { request, error },
    }
}

async fn inject_native_compact(request: NativeCompactRequest) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            crate::services::claude_tui::composer_lock::with_session_turn_lock(
                &request.tmux_session_name,
                || {
                    crate::services::claude_tui::input::send_followup_prompt(
                        &request.tmux_session_name,
                        request.prompt,
                        None,
                    )
                },
            )
        }
        #[cfg(not(unix))]
        {
            crate::services::claude_tui::input::send_followup_prompt(
                &request.tmux_session_name,
                request.prompt,
                None,
            )
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn claim_recap_compact_pointer(
    pool: &PgPool,
    target: &RecapClearTarget,
    message_id: u64,
    channel_id: u64,
) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "UPDATE sessions
         SET idle_recap_message_id = NULL,
             idle_recap_channel_id = NULL
         WHERE session_key = $1
           AND provider = $2
           AND idle_recap_turn_generation = $3
           AND idle_recap_message_id = $4
           AND idle_recap_channel_id = $5
           AND instance_id IS NOT DISTINCT FROM $6
           AND COALESCE(idle_recap_posted_at >= COALESCE(last_heartbeat, created_at), false)",
    )
    .bind(&target.session_key)
    .bind(&target.provider)
    .bind(target.turn_generation)
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(target.owner_instance_id.as_deref())
    .execute(pool)
    .await
    .map(|result| result.rows_affected() == 1)
}

async fn recap_compact_owner_unchanged(
    pool: &PgPool,
    target: &RecapClearTarget,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1
             FROM sessions
             WHERE session_key = $1
               AND instance_id IS NOT DISTINCT FROM $2
         )",
    )
    .bind(&target.session_key)
    .bind(target.owner_instance_id.as_deref())
    .fetch_one(pool)
    .await
}

async fn handle_idle_recap_suggest_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    if !authorize_component(ctx, component, data).await {
        return Ok(());
    }

    let Some(message_id) =
        parse_message_id(&component.data.custom_id, IDLE_RECAP_SUGGEST_BUTTON_PREFIX)
    else {
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };
    let Some(suggested_reply) =
        crate::services::discord::idle_recap::suggested_reply_from_recap_content(
            &component.message.content,
        )
    else {
        send_ephemeral(ctx, component, "추천 답변을 찾을 수 없습니다.").await;
        return Ok(());
    };
    let prompt_text = suggested_reply.clone();
    let Some(pool) = data.shared.pg_pool.as_ref().cloned() else {
        send_ephemeral(ctx, component, "추천 답변 전송 실패: DB 연결 없음.").await;
        return Ok(());
    };
    let Some(target) = lookup_current_recap_target(
        &pool,
        message_id,
        component.channel_id.get(),
        data.provider.as_str(),
    )
    .await?
    else {
        send_ephemeral(
            ctx,
            component,
            "추천 답변 대상이 더 이상 유효하지 않습니다.",
        )
        .await;
        return Ok(());
    };

    let enqueued = crate::services::discord::enqueue_internal_followup(
        &data.shared,
        &data.provider,
        component.channel_id,
        serenity::MessageId::new(message_id),
        suggested_reply,
        "idle recap suggested reply",
    )
    .await;
    if !enqueued {
        send_ephemeral(ctx, component, "추천 답변을 큐에 넣지 못했습니다.").await;
        return Ok(());
    }

    send_ephemeral(ctx, component, &prompt_sent_ephemeral(&prompt_text)).await;
    let _ = clear_recap_pointer(&pool, &target.session_key, message_id).await;
    delete_previous_card(&ctx.http, component.channel_id.get(), message_id).await;
    Ok(())
}

async fn authorize_component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> bool {
    let user_id = component.user.id;
    let user_name = &component.user.name;
    if check_auth(user_id, user_name, &data.shared, &data.token).await {
        return true;
    }
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
    false
}

async fn send_ephemeral(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    content: &str,
) {
    let _ = component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content(content)
                    .ephemeral(true),
            ),
        )
        .await;
}

async fn edit_deferred_ephemeral(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    message_id: u64,
    content: &str,
) {
    if let Err(error) = component
        .edit_response(
            ctx,
            serenity::EditInteractionResponse::new().content(content),
        )
        .await
    {
        tracing::warn!(
            error = %error,
            message_id,
            "idle_recap compact: failed to edit deferred interaction response"
        );
    }
}

fn truncate_interaction_body(body: &str) -> String {
    const LIMIT: usize = 1800;
    let mut out = String::new();
    let mut chars = body.chars();
    for ch in chars.by_ref().take(LIMIT) {
        out.push(ch);
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn prompt_sent_ephemeral(prompt: &str) -> String {
    let prompt = truncate_interaction_body(prompt);
    format!("다음 프롬프트를 보냈습니다:\n> {prompt}")
}

fn parse_message_id(custom_id: &str, prefix: &str) -> Option<u64> {
    custom_id
        .strip_prefix(prefix)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|id| *id != 0)
}

async fn lookup_recap_clear_target(
    pool: &PgPool,
    message_id: u64,
    channel_id: u64,
    provider: &str,
) -> Result<Option<RecapClearTarget>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, String, Option<String>, i64, bool, bool, bool)>(
        "SELECT session_key,
                provider,
                instance_id,
                idle_recap_turn_generation,
                idle_recap_channel_id = $2 AS channel_matches,
                provider = $3 AS provider_matches,
                COALESCE(idle_recap_posted_at >= COALESCE(last_heartbeat, created_at), false) AS recap_current
         FROM sessions
         WHERE idle_recap_message_id = $1
         LIMIT 1",
    )
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(provider)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(
            session_key,
            provider,
            owner_instance_id,
            turn_generation,
            channel_matches,
            provider_matches,
            recap_current,
        )| RecapClearTarget {
            session_key,
            provider,
            owner_instance_id,
            turn_generation,
            channel_matches,
            provider_matches,
            recap_current,
        },
    ))
}

async fn lookup_current_recap_target(
    pool: &PgPool,
    message_id: u64,
    channel_id: u64,
    provider: &str,
) -> Result<Option<RecapClearTarget>, sqlx::Error> {
    let Some(target) = lookup_recap_clear_target(pool, message_id, channel_id, provider).await?
    else {
        return Ok(None);
    };
    if target.channel_matches && target.provider_matches && target.recap_current {
        Ok(Some(target))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_recap_component_router_accepts_all_recap_actions() {
        assert!(is_idle_recap_custom_id("idle_recap:clear:1"));
        assert!(is_idle_recap_custom_id("idle_recap:relay_diag:1"));
        assert!(is_idle_recap_custom_id("idle_recap:compact:1"));
        assert!(is_idle_recap_custom_id("idle_recap:suggest:1"));
        assert!(!is_idle_recap_custom_id("foreign:action:1"));
    }

    fn recap_target(session_key: &str) -> RecapClearTarget {
        RecapClearTarget {
            session_key: session_key.to_string(),
            provider: "claude".to_string(),
            owner_instance_id: Some("mac-mini-release".to_string()),
            turn_generation: 7,
            channel_matches: true,
            provider_matches: true,
            recap_current: true,
        }
    }

    #[tokio::test]
    async fn compact_uses_claimed_recap_target_and_native_prompt() {
        let injected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = injected.clone();
        let outcome = run_native_compact_once(
            recap_target("claude/token/mac-mini:old-bound-session"),
            "mac-mini",
            "mac-mini-release",
            || async { Ok(true) },
            || async { Ok(true) },
            |target| std::future::ready(target == "old-bound-session"),
            move |request| {
                observed.lock().unwrap().push(request.clone());
                async { Ok(()) }
            },
        )
        .await;

        let expected = NativeCompactRequest {
            tmux_session_name: "old-bound-session".to_string(),
            prompt: "/compact",
        };
        assert_eq!(outcome, RecapCompactOutcome::Started(expected.clone()));
        assert_eq!(*injected.lock().unwrap(), vec![expected]);
    }

    #[tokio::test]
    async fn rebinding_cannot_redirect_compact_to_current_channel_session() {
        let injection_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = injection_calls.clone();
        let outcome = run_native_compact_once(
            recap_target("claude/token/mac-mini:old-bound-session"),
            "mac-mini",
            "mac-mini-release",
            || async { Ok(true) },
            || async { Ok(true) },
            |target| std::future::ready(target == "new-current-session"),
            move |_| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
        )
        .await;

        assert_eq!(
            outcome,
            RecapCompactOutcome::TargetNotLive(NativeCompactRequest {
                tmux_session_name: "old-bound-session".to_string(),
                prompt: "/compact",
            })
        );
        assert_eq!(injection_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_compact_claims_allow_exactly_one_injection() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let claim_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let claimed = std::sync::Arc::new(AtomicBool::new(false));
        let claims_in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let max_claims_in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let injections = std::sync::Arc::new(AtomicUsize::new(0));
        let run = || {
            let claim_barrier = claim_barrier.clone();
            let claimed = claimed.clone();
            let claims_in_flight = claims_in_flight.clone();
            let max_claims_in_flight = max_claims_in_flight.clone();
            let injections = injections.clone();
            run_native_compact_once(
                recap_target("mac-mini:claimed-session"),
                "mac-mini",
                "mac-mini-release",
                move || async move {
                    let in_flight = claims_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_claims_in_flight.fetch_max(in_flight, Ordering::SeqCst);
                    claim_barrier.wait().await;
                    let won = claimed
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok();
                    claims_in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(won)
                },
                || async { Ok(true) },
                |_| async { true },
                move |_| async move {
                    injections.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
        };

        let (first, second) = tokio::join!(run(), run());
        let outcomes = [first, second];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RecapCompactOutcome::Started(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RecapCompactOutcome::AlreadyClaimed))
                .count(),
            1
        );
        assert_eq!(max_claims_in_flight.load(Ordering::SeqCst), 2);
        assert_eq!(injections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn foreign_session_identity_preserves_pointer_without_claim_or_injection() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let claim_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let pointer_consumed = std::sync::Arc::new(AtomicBool::new(false));
        let live_checks = std::sync::Arc::new(AtomicUsize::new(0));
        let injections = std::sync::Arc::new(AtomicUsize::new(0));
        let outcome = run_native_compact_once(
            recap_target("claude/token/mac-book:foo"),
            "mac-mini",
            "mac-mini-release",
            {
                let claim_calls = claim_calls.clone();
                let pointer_consumed = pointer_consumed.clone();
                move || async move {
                    claim_calls.fetch_add(1, Ordering::SeqCst);
                    pointer_consumed.store(true, Ordering::SeqCst);
                    Ok(true)
                }
            },
            || async { Ok(true) },
            {
                let live_checks = live_checks.clone();
                move |_| {
                    live_checks.fetch_add(1, Ordering::SeqCst);
                    async { true }
                }
            },
            {
                let injections = injections.clone();
                move |_| async move {
                    injections.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;

        assert_eq!(
            outcome,
            RecapCompactOutcome::RoutingUnavailable {
                owner_instance_id: Some("mac-mini-release".to_string()),
                reason: "session_key_host_mismatch".to_string(),
            }
        );
        assert!(outcome.preserves_recap_card());
        assert_eq!(claim_calls.load(Ordering::SeqCst), 0);
        assert!(!pointer_consumed.load(Ordering::SeqCst));
        assert_eq!(live_checks.load(Ordering::SeqCst), 0);
        assert_eq!(injections.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn foreign_or_missing_owner_never_claims_local_host_target() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for owner_instance_id in [Some("mac-book-release".to_string()), None] {
            let mut target = recap_target("claude/token/mac-mini:foo");
            target.owner_instance_id = owner_instance_id.clone();
            let claim_calls = std::sync::Arc::new(AtomicUsize::new(0));
            let observed_claims = claim_calls.clone();
            let outcome = run_native_compact_once(
                target,
                "mac-mini",
                "mac-mini-release",
                move || {
                    observed_claims.fetch_add(1, Ordering::SeqCst);
                    async { Ok(true) }
                },
                || async { Ok(true) },
                |_| async { true },
                |_| async { Ok(()) },
            )
            .await;

            assert!(matches!(
                outcome,
                RecapCompactOutcome::RoutingUnavailable {
                    owner_instance_id: ref observed_owner,
                    ..
                } if observed_owner == &owner_instance_id
            ));
            assert_eq!(claim_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn injection_failure_is_terminal_after_claim() {
        let claim_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = claim_calls.clone();
        let first = run_native_compact_once(
            recap_target("mac-mini:claimed-session"),
            "mac-mini",
            "mac-mini-release",
            move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(true) }
            },
            || async { Ok(true) },
            |_| async { true },
            |_| async { Err("submit failed".to_string()) },
        )
        .await;
        let retried_injections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_retries = retried_injections.clone();
        let retry = run_native_compact_once(
            recap_target("mac-mini:claimed-session"),
            "mac-mini",
            "mac-mini-release",
            || async { Ok(false) },
            || async { Ok(true) },
            |_| async { true },
            move |_| {
                observed_retries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
        )
        .await;

        assert!(matches!(
            first,
            RecapCompactOutcome::InjectionFailed { ref request, ref error }
                if request.prompt == "/compact" && error == "submit failed"
        ));
        assert_eq!(retry, RecapCompactOutcome::AlreadyClaimed);
        assert_eq!(claim_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            retried_injections.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn claim_database_error_fails_closed_without_injection() {
        let injection_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = injection_calls.clone();
        let outcome = run_native_compact_once(
            recap_target("mac-mini:claimed-session"),
            "mac-mini",
            "mac-mini-release",
            || async { Err("database unavailable".to_string()) },
            || async { Ok(true) },
            |_| async { true },
            move |_| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
        )
        .await;

        assert_eq!(
            outcome,
            RecapCompactOutcome::ClaimFailed("database unavailable".to_string())
        );
        assert_eq!(injection_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn owner_handoff_after_claim_is_fenced_immediately_before_injection() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let live_checks = std::sync::Arc::new(AtomicUsize::new(0));
        let injections = std::sync::Arc::new(AtomicUsize::new(0));
        let outcome = run_native_compact_once(
            recap_target("mac-mini:claimed-session"),
            "mac-mini",
            "mac-mini-release",
            || async { Ok(true) },
            || async { Ok(false) },
            {
                let live_checks = live_checks.clone();
                move |_| async move {
                    live_checks.fetch_add(1, Ordering::SeqCst);
                    true
                }
            },
            {
                let injections = injections.clone();
                move |_| async move {
                    injections.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;

        assert_eq!(outcome, RecapCompactOutcome::PostClaimOwnerChanged);
        assert!(!outcome.preserves_recap_card());
        assert_eq!(live_checks.load(Ordering::SeqCst), 1);
        assert_eq!(injections.load(Ordering::SeqCst), 0);
    }

    async fn seed_recap_compact_session_pg(
        pool: &PgPool,
        target: &RecapClearTarget,
        message_id: u64,
        channel_id: u64,
    ) {
        sqlx::query(
            "INSERT INTO sessions (
                session_key,
                provider,
                instance_id,
                idle_recap_turn_generation,
                idle_recap_message_id,
                idle_recap_channel_id,
                idle_recap_posted_at,
                last_heartbeat
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind(&target.session_key)
        .bind(&target.provider)
        .bind(target.owner_instance_id.as_deref())
        .bind(target.turn_generation)
        .bind(message_id as i64)
        .bind(channel_id as i64)
        .execute(pool)
        .await
        .expect("seed recap compact session");
    }

    #[tokio::test]
    async fn concurrent_recap_compact_claim_pg_consumes_exactly_once() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "adk_recap_claim",
            "idle recap compact concurrent claim test",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate_with_max_connections(2).await;
        let target = recap_target("claude/token/mac-mini:pg-claimed-session");
        let message_id = 42;
        let channel_id = 84;
        seed_recap_compact_session_pg(&pool, &target, message_id, channel_id).await;

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let claim = || {
            let pool = pool.clone();
            let target = target.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                claim_recap_compact_pointer(&pool, &target, message_id, channel_id)
                    .await
                    .expect("claim recap compact pointer")
            }
        };
        let (first, second) = tokio::join!(claim(), claim());

        assert_eq!(
            [first, second]
                .into_iter()
                .filter(|claimed| *claimed)
                .count(),
            1
        );
        let pointer = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
            "SELECT idle_recap_message_id, idle_recap_channel_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(&target.session_key)
        .fetch_one(&pool)
        .await
        .expect("load claimed recap compact pointer");
        assert_eq!(pointer, (None, None));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn recap_compact_claim_pg_rejects_owner_handoff_without_consuming_pointer() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "adk_recap_owner",
            "idle recap compact owner handoff fence test",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let target = recap_target("claude/token/mac-mini:pg-owner-session");
        let message_id = 43;
        let channel_id = 85;
        seed_recap_compact_session_pg(&pool, &target, message_id, channel_id).await;
        sqlx::query("UPDATE sessions SET instance_id = 'mac-book-release' WHERE session_key = $1")
            .bind(&target.session_key)
            .execute(&pool)
            .await
            .expect("handoff recap compact owner");

        let injections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let outcome = run_native_compact_once(
            target.clone(),
            "mac-mini",
            "mac-mini-release",
            {
                let pool = pool.clone();
                let target = target.clone();
                move || async move {
                    claim_recap_compact_pointer(&pool, &target, message_id, channel_id)
                        .await
                        .map_err(|error| error.to_string())
                }
            },
            || async { Ok(true) },
            |_| async { true },
            {
                let injections = injections.clone();
                move |_| async move {
                    injections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(outcome, RecapCompactOutcome::AlreadyClaimed);
        assert!(outcome.preserves_recap_card());
        assert_eq!(injections.load(std::sync::atomic::Ordering::SeqCst), 0);
        let pointer = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<String>)>(
            "SELECT idle_recap_message_id, idle_recap_channel_id, instance_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(&target.session_key)
        .fetch_one(&pool)
        .await
        .expect("load owner-fenced recap compact pointer");
        assert_eq!(
            pointer,
            (
                Some(message_id as i64),
                Some(channel_id as i64),
                Some("mac-book-release".to_string())
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn recap_prompt_route_sends_suggest_to_internal_followup_handler() {
        assert_eq!(
            recap_prompt_route("idle_recap:suggest:42"),
            Some(RecapPromptRoute::InternalFollowup)
        );
    }

    #[test]
    fn recap_prompt_route_rejects_unrelated_custom_ids() {
        assert_eq!(recap_prompt_route("idle_recap:clear:42"), None);
        assert_eq!(recap_prompt_route("foreign:action:42"), None);
    }

    #[test]
    fn recap_component_message_id_parser_rejects_zero_and_foreign_prefixes() {
        assert_eq!(
            parse_message_id(
                "idle_recap:relay_diag:42",
                IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX
            ),
            Some(42)
        );
        assert_eq!(
            parse_message_id("idle_recap:compact:42", IDLE_RECAP_COMPACT_BUTTON_PREFIX),
            Some(42)
        );
        assert_eq!(
            parse_message_id(
                "idle_recap:relay_diag:0",
                IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX
            ),
            None
        );
        assert_eq!(
            parse_message_id("idle_recap:suggest:42", IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX),
            None
        );
    }

    #[test]
    fn recap_prompt_sent_ephemeral_includes_actual_prompt_text() {
        assert_eq!(
            prompt_sent_ephemeral("테스트 계속 진행해줘"),
            "다음 프롬프트를 보냈습니다:\n> 테스트 계속 진행해줘"
        );
        assert_eq!(
            prompt_sent_ephemeral("/compact"),
            "다음 프롬프트를 보냈습니다:\n> /compact"
        );
    }
}
