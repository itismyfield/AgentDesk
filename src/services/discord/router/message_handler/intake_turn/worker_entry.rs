use super::*;

pub(crate) use crate::services::cluster::intake_turn::IntakeRequest;

/// Claimed workers check canonical admission without routing another delivery row.
/// Only their execution outcome may settle the original delivery.
pub(crate) async fn execute_intake_turn_core(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    request: IntakeRequest,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
) -> Result<IntakeExecution, Box<dyn std::error::Error + Send + Sync>> {
    if !matches!(
        crate::services::imsg_reply::agy_input::bound_row(
            shared.pg_pool.as_ref(),
            &shared.provider,
            request.channel_id.get(),
            request.user_msg_id.get(),
            true,
        )
        .await,
        Ok(None)
    ) {
        tracing::warn!(
            channel_id = request.channel_id.get(),
            "agy queue worker input held for local custody"
        );
        return Ok(IntakeExecution::Deferred);
    }
    #[cfg(unix)]
    {
        let proof = crate::services::cluster::intake_worker::take_before_effects(
            &request,
            crate::services::discord::checkpoint_waits_for_admission,
        );
        let deps = IntakeDeps {
            http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared,
            token,
        };
        if crate::services::discord::router::herdr_consent::admission::worker(
            &deps, &request, proof,
        )
        .await
        {
            return Ok(IntakeExecution::Deferred);
        }
    }
    if let Some(stopped) = worker_replay_admission(shared, &request).await {
        return Ok(stopped);
    }
    if crate::services::discord::queue_io::input_ingress::adopt(
        shared,
        &shared.provider,
        request.channel_id,
    )
    .await
        != Ok(false)
    {
        if let Some(intake) = crate::services::discord::queue_io::input_ingress::intake(
            shared,
            &shared.provider,
            &request,
            uploads.clone(),
            true,
            false,
        )
        .await
        {
            return intake.worker();
        }
    }
    super::handle_text_message(
        &IntakeDeps {
            http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared,
            token,
        },
        request.preserve_on_cancel,
        request,
        (false, None, None),
        uploads,
        (true, None),
    )
    .await
}

/// A claimed delivery keeps its existing durable payload when canonical admission cannot start it.
async fn worker_replay_admission(
    shared: &Arc<SharedData>,
    request: &IntakeRequest,
) -> Option<IntakeExecution> {
    use crate::services::discord::router::intake_dispatch::replay_admission::{
        self as admission, ReplayAdmissionContext, ReplayPreflight,
    };
    let channel = request.channel_id.get().to_string();
    if request.intake_outbox_id.is_none() {
        return None;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Some(IntakeExecution::Deferred);
    };
    let input = admission::discord_input(
        shared.provider.as_str(),
        request.channel_id.get(),
        request.user_msg_id.get(),
        request.source_message_ids.iter().map(|source| source.get()),
        &request.user_text,
    );
    // Physical adapters must consume the owner permit before this producer can be enabled.
    if crate::services::discord::router::intake_gate::input_start::has_custody(pool, &input)
        .await
        .unwrap_or(true)
    {
        return Some(IntakeExecution::Deferred);
    }
    let instance =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let ctx = ReplayAdmissionContext::production(pool, &instance, admission::incarnation())
        .for_channel(&channel);
    match admission::preflight_source(&ctx, &input).await {
        ReplayPreflight::Admit(_) | ReplayPreflight::LegacyUnprotected => None,
        ReplayPreflight::ConsumeProtected(_) => Some(IntakeExecution::ConsumedProtected),
        ReplayPreflight::Defer(_) => Some(IntakeExecution::Deferred),
    }
}

/// How long an exact retry waits for the stale turn to release its channel before it gives up.
const EXACT_RETRY_CLAIM_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Starts a protected request's exact retry on the intake start path once the stale turn released
/// the channel. Only that start can take the permit; anything short of its begin leaves it held.
pub(crate) async fn start_exact_retry(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    retry: crate::services::discord::replay_policy::ExactRetry,
) -> crate::services::discord::replay_policy::ExactHandoff {
    use crate::services::discord::replay_policy::{ExactHandoff, hand_over};
    let deadline = tokio::time::Instant::now() + EXACT_RETRY_CLAIM_WAIT;
    while crate::services::discord::mailbox_snapshot(shared, channel_id)
        .await
        .cancel_token
        .is_some()
    {
        if tokio::time::Instant::now() >= deadline {
            return ExactHandoff::Refused;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let request = IntakeRequest {
        intake_outbox_id: None,
        channel_id,
        user_msg_id: retry.user_message_id,
        source_message_ids: retry.sources,
        busy_followup_retry_user_msg_id: retry.user_message_id,
        request_owner: retry.request_owner,
        request_owner_name: String::new(),
        user_text: retry.text,
        reply_to_user_message: true,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: retry.reply_context,
        has_reply_boundary: false,
        dm_hint: None,
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: false,
    };
    let token = shared
        .http
        .cached_bot_token
        .get()
        .cloned()
        .unwrap_or_default();
    let start = execute_intake_turn_core(http, shared, &token, request, Vec::new());
    let start = crate::services::discord::queue_io::input_ingress::existing_retry(start);
    let (result, handoff) = hand_over(retry.permit, start).await;
    if let Err(error) = result {
        tracing::warn!(channel_id = channel_id.get(), %error, "exact retry start failed");
    }
    handoff
}

#[cfg(test)]
mod custody_pg_tests;

#[cfg(all(test, unix))]
mod preserve_cancel_tests;

#[cfg(all(test, unix))]
mod goal_fresh_pg_tests;

#[cfg(all(test, unix))]
mod dispatch_guard_pg_tests;
