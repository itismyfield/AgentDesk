//! The recap card's native `/compact` button. An unprotected channel claims the card and injects
//! as in main; a channel the input fence admits keeps the card until the send has returned.

use poise::serenity_prelude as serenity;
use sqlx::{PgConnection, PgPool};

use super::{
    Data, Error, RecapClearTarget, admit_recap_effect, authorize_component,
    edit_deferred_ephemeral, lookup_current_recap_target, parse_message_id, send_ephemeral,
    send_native_compact,
};
use crate::services::discord::idle_recap::{
    IDLE_RECAP_COMPACT_BUTTON_PREFIX, delete_previous_card,
};
use crate::services::discord::input_runtime::fence;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NativeCompactRequest {
    pub(super) tmux_session_name: String,
    pub(super) prompt: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RecapCompactOutcome {
    Started(NativeCompactRequest),
    /// The input fence refused the click before any claim, probe or send.
    Refused(String),
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
    /// The session's host is not legacy tmux: no claim, probe or injection ran.
    HostUnsupported(String),
    InjectionFailed {
        request: NativeCompactRequest,
        error: String,
    },
}

impl RecapCompactOutcome {
    pub(super) fn preserves_recap_card(&self) -> bool {
        matches!(
            self,
            Self::AlreadyClaimed
                | Self::Refused(_)
                | Self::ClaimFailed(_)
                | Self::InvalidTarget
                | Self::RoutingUnavailable { .. }
                | Self::HostUnsupported(_)
        )
    }
}

/// A compact click's outcome; `protected` runs consumed the card only on a returned send.
#[derive(Debug)]
pub(super) struct RecapCompactRun {
    pub(super) outcome: RecapCompactOutcome,
    pub(super) protected: bool,
}

impl RecapCompactRun {
    pub(super) fn keeps_card(&self) -> bool {
        if self.protected {
            !matches!(self.outcome, RecapCompactOutcome::Started(_))
        } else {
            self.outcome.preserves_recap_card()
        }
    }
}

pub(super) async fn handle_idle_recap_compact_interaction(
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
    let run = run_recap_compact(
        &data.provider,
        &pool,
        (target, message_id, channel_id),
        (
            &crate::services::platform::hostname_short(),
            &crate::services::cluster::node_registry::resolve_self_instance_id_without_config(),
        ),
        CompactSeams {
            host_refusal: |tmux_session_name: &str| {
                let refusal = crate::services::discord::admin_host_guard::session_key_refusal;
                let (pool, name) = (pool.clone(), tmux_session_name.to_string());
                let (provider, key) = (data.provider.clone(), target_session_key_for_log.clone());
                async move { refusal(&pool, Some(&provider), Some(channel_id), &key, &name).await }
            },
            legacy_claim: move || async move {
                let (pool, target) = (&pool_for_claim, &target_for_claim);
                claim_recap_compact_pointer(pool, target, message_id, channel_id)
                    .await
                    .map_err(|error| error.to_string())
            },
            pre_inject_fence: move || async move {
                let (pool, target) = (&pool_for_pre_inject_fence, &target_for_pre_inject_fence);
                recap_compact_owner_unchanged(pool, target)
                    .await
                    .map_err(|error| error.to_string())
            },
            is_live: |tmux_session_name: &str| {
                std::future::ready(
                    crate::services::tmux_diagnostics::tmux_session_has_live_pane(
                        tmux_session_name,
                    ),
                )
            },
            send: send_native_compact,
        },
    )
    .await;

    let response = match &run.outcome {
        RecapCompactOutcome::Started(_) => "Claude 맥락 압축을 시작했습니다.",
        RecapCompactOutcome::Refused(reason) => {
            tracing::warn!(message_id, %reason, "idle_recap compact: input fence refused");
            "맥락 압축 요청 거절: 이 채널의 입력이 전환 중이라 지금은 실행할 수 없습니다."
        }
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
        RecapCompactOutcome::HostUnsupported(reason) => {
            tracing::warn!(message_id, %reason, "idle_recap compact: host unsupported");
            "맥락 압축 요청 거절: 이 세션의 호스트에서는 맥락 압축을 지원하지 않습니다."
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
                protected = run.protected,
                "idle_recap compact: terminal native /compact injection failure"
            );
            if run.protected {
                "맥락 압축 요청의 전송 결과를 확인하지 못했습니다. 자동으로 다시 보내지 않습니다. 화면을 확인한 뒤 필요하면 /compact를 직접 실행하세요."
            } else {
                "맥락 압축 요청을 전송하지 못했습니다. 중복 방지를 위해 요청을 종료했습니다. /compact를 직접 실행하세요."
            }
        }
    };
    edit_deferred_ephemeral(ctx, component, message_id, response).await;

    if !run.keeps_card() {
        delete_previous_card(&ctx.http, channel_id, message_id).await;
    }
    Ok(())
}

/// The click's claim, probe and send boundaries; the handler passes main's, tests their own.
pub(super) struct CompactSeams<HostRefusal, Claim, PreInjectFence, IsLive, SendFn> {
    pub(super) host_refusal: HostRefusal,
    pub(super) legacy_claim: Claim,
    pub(super) pre_inject_fence: PreInjectFence,
    pub(super) is_live: IsLive,
    pub(super) send: SendFn,
}

/// Admits the click first. An unprotected channel runs main's destructive claim; an admitted
/// one reserves the card, sends under its permit and consumes the exact pointer on success.
pub(super) async fn run_recap_compact<
    HostRefusal,
    HostRefusalFut,
    Claim,
    ClaimFut,
    PreInjectFence,
    PreInjectFenceFut,
    IsLive,
    IsLiveFut,
    SendFn,
>(
    provider: &ProviderKind,
    pool: &PgPool,
    (target, message_id, channel_id): (RecapClearTarget, u64, u64),
    local: (&str, &str),
    seams: CompactSeams<HostRefusal, Claim, PreInjectFence, IsLive, SendFn>,
) -> RecapCompactRun
where
    HostRefusal: FnOnce(&str) -> HostRefusalFut,
    HostRefusalFut: std::future::Future<Output = Option<String>>,
    Claim: FnOnce() -> ClaimFut,
    ClaimFut: std::future::Future<Output = Result<bool, String>>,
    PreInjectFence: FnOnce() -> PreInjectFenceFut,
    PreInjectFenceFut: std::future::Future<Output = Result<bool, String>>,
    IsLive: FnOnce(&str) -> IsLiveFut,
    IsLiveFut: std::future::Future<Output = bool>,
    SendFn: FnOnce(&NativeCompactRequest) -> Result<(), String> + Send + 'static,
{
    let CompactSeams {
        host_refusal,
        legacy_claim,
        pre_inject_fence,
        is_live,
        send,
    } = seams;
    let permit = match admit_recap_effect(provider, channel_id) {
        Ok(permit) => permit,
        Err(reason) => {
            return RecapCompactRun {
                outcome: RecapCompactOutcome::Refused(reason),
                protected: true,
            };
        }
    };
    let Some(permit) = permit else {
        let outcome = run_native_compact_once(
            target,
            local,
            host_refusal,
            legacy_claim,
            pre_inject_fence,
            is_live,
            |request| inject_native_compact(request, None, send),
        )
        .await;
        return RecapCompactRun {
            outcome,
            protected: false,
        };
    };

    // The reservation's transaction holds the card's advisory lock until it commits or drops.
    let reservation = std::sync::Mutex::new(None);
    let outcome = run_native_compact_once(
        target.clone(),
        local,
        host_refusal,
        || async {
            let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
            let reserved = reserve_recap_card(&mut tx, &target, message_id, channel_id)
                .await
                .map_err(|error| error.to_string())?;
            if reserved {
                *reservation.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
            }
            Ok(reserved)
        },
        pre_inject_fence,
        is_live,
        |request| inject_native_compact(request, Some(permit), send),
    )
    .await;
    let tx = reservation.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let (RecapCompactOutcome::Started(_), Some(mut tx)) = (&outcome, tx) {
        let consumed = consume_recap_card(&mut tx, &target, message_id, channel_id).await;
        if let Err(error) = async {
            consumed?;
            tx.commit().await
        }
        .await
        {
            tracing::warn!(%error, message_id, "idle_recap compact: sent, pointer kept");
        }
    }
    RecapCompactRun {
        outcome,
        protected: true,
    }
}

pub(super) async fn run_native_compact_once<
    HostRefusal,
    HostRefusalFut,
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
    (local_hostname, local_instance_id): (&str, &str),
    host_refusal: HostRefusal,
    claim: Claim,
    pre_inject_fence: PreInjectFence,
    is_live: IsLive,
    inject: Inject,
) -> RecapCompactOutcome
where
    HostRefusal: FnOnce(&str) -> HostRefusalFut,
    HostRefusalFut: std::future::Future<Output = Option<String>>,
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

    // Compact is unsupported on any other host: refused before the claim consumes the card.
    if let Some(reason) = host_refusal(&identity.tmux_name).await {
        return RecapCompactOutcome::HostUnsupported(reason);
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

/// The blocking send owns the permit, so a close still waits after the caller is dropped.
pub(super) async fn inject_native_compact(
    request: NativeCompactRequest,
    permit: Option<fence::Permit>,
    send: impl FnOnce(&NativeCompactRequest) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || match permit {
        Some(permit) => fence::effect::synchronous(Some(permit), || send(&request)),
        None => send(&request),
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(super) async fn claim_recap_compact_pointer(
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

/// Serializes the session's recap actions on a transaction advisory lock and confirms the card
/// main's claim would take, without consuming it; a held lock reads as already claimed.
pub(super) async fn reserve_recap_card(
    conn: &mut PgConnection,
    target: &RecapClearTarget,
    message_id: u64,
    channel_id: u64,
) -> Result<bool, sqlx::Error> {
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("idle_recap_card:{}", target.session_key))
            .fetch_one(&mut *conn)
            .await?;
    if !locked {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
             FROM sessions
             WHERE session_key = $1
               AND provider = $2
               AND idle_recap_turn_generation = $3
               AND idle_recap_message_id = $4
               AND idle_recap_channel_id = $5
               AND instance_id IS NOT DISTINCT FROM $6
               AND COALESCE(idle_recap_posted_at >= COALESCE(last_heartbeat, created_at), false)
         )",
    )
    .bind(&target.session_key)
    .bind(&target.provider)
    .bind(target.turn_generation)
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(target.owner_instance_id.as_deref())
    .fetch_one(&mut *conn)
    .await
}

/// Clears the reserved card's own pointer; a successor card or a moved owner is left alone.
/// Freshness was settled under the lock, as the sent action itself advances the heartbeat.
pub(super) async fn consume_recap_card(
    conn: &mut PgConnection,
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
           AND instance_id IS NOT DISTINCT FROM $6",
    )
    .bind(&target.session_key)
    .bind(&target.provider)
    .bind(target.turn_generation)
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(target.owner_instance_id.as_deref())
    .execute(&mut *conn)
    .await
    .map(|result| result.rows_affected() == 1)
}

pub(super) async fn recap_compact_owner_unchanged(
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
