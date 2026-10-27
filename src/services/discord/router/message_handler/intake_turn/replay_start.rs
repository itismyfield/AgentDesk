//! The replay check of an intake turn, right before its inflight projection is saved.

use super::*;
use crate::db::replay_disposition::write::CanonicalInput;
use crate::services::discord::replay_policy;
use crate::services::discord::replay_policy::permit::EffectTarget;
use crate::services::discord::router::intake_dispatch::replay_admission::{
    self as admission, PreparedEffectProjection, ReplayStart, StartStop,
};

pub(in crate::services::discord::router::message_handler) mod common;

/// What the turn's start registers besides the inflight request itself.
pub(super) struct StartRequest<'a> {
    pub(super) provider: &'a ProviderKind,
    pub(super) remote: bool,
    pub(super) channel_id: ChannelId,
    pub(super) agent_id: Option<&'a str>,
    pub(super) reply_context: Option<&'a str>,
    pub(super) uploads: &'a crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    pub(super) session_key: Option<&'a str>,
    pub(super) prompt: &'a str,
}

/// A start and whether it is fresh, or the turn's result once a refused start is settled.
type Checked = Result<(ReplayStart, bool), Result<IntakeExecution, Error>>;

/// Runs `admit`, settling a refused start with `stop`; boxed inside this plain function so the
/// check's store futures never sit in the intake turn's poll frame.
pub(super) fn check<'a>(
    shared: &'a Arc<SharedData>,
    request: StartRequest<'a>,
    inflight: &'a mut InflightTurnState,
    cancel_token: &'a Arc<CancelToken>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Checked> + Send + 'a>> {
    Box::pin(async move {
        let provider = request.provider;
        match admit(shared, request, inflight).await {
            Ok(started) => Ok(started),
            Err(refused) => Err(stop(shared, provider, inflight, cancel_token, refused).await),
        }
    })
}

/// A dormant build keeps the legacy start without IO; an active one begins the request or its exact
/// continuation once no other turn owns the channel. `true` means the stale resume was dropped.
async fn admit(
    shared: &Arc<SharedData>,
    request: StartRequest<'_>,
    inflight: &mut InflightTurnState,
) -> Result<(ReplayStart, bool), StartStop> {
    let channel = request.channel_id.get().to_string();
    let continuation = replay_policy::take_continuation();
    let uploads: Vec<String> = request
        .uploads
        .iter()
        .map(|upload| upload.history_record())
        .collect();
    let sources = inflight.source_message_ids.iter().copied();
    let input = CanonicalInput {
        owner_id: inflight.request_owner_user_id.to_string(),
        agent_id: request.agent_id.unwrap_or_default().to_string(),
        attachments: serde_json::json!(uploads),
        reply_context: request.reply_context.map(str::to_string),
        provenance: if continuation.is_some() {
            "reentry"
        } else {
            "new_input"
        },
        ..admission::discord_input(
            request.provider.as_str(),
            request.channel_id.get(),
            inflight.user_msg_id,
            sources,
            &inflight.user_text,
        )
    };
    let projection = PreparedEffectProjection {
        target: EffectTarget::ProviderStart {
            provider: request.provider.as_str().to_string(),
            channel,
        },
        input_hash: admission::input_hash(request.prompt),
        binding: serde_json::json!({
            "turn_nonce": inflight.turn_nonce,
            "user_msg_id": inflight.user_msg_id,
        }),
        session_key: request.session_key.map(str::to_string),
    };
    let scope = super::super::provider_dispatch::replay_scope::selected(
        request.provider,
        request.channel_id.get(),
        request.remote,
        inflight.tmux_session_name.as_deref(),
    );
    common::begin(
        shared,
        request.provider,
        inflight,
        input,
        projection,
        continuation,
        scope,
    )
    .await
}

/// Settles a start that did not happen: a consumed request releases its claim, a deferred one
/// goes back to the front of its queue; `Err` when that custody was refused.
async fn stop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    inflight: &InflightTurnState,
    cancel_token: &Arc<CancelToken>,
    stop: StartStop,
) -> Result<IntakeExecution, Error> {
    let defer = crate::services::discord::live_bridge::defer_unstarted_turn;
    let (requeue, reason) = match &stop {
        StartStop::Consumed => (false, "replay_start_consumed"),
        StartStop::Deferred(why) => {
            tracing::warn!(
                channel_id = inflight.channel_id,
                why,
                "intake start deferred by its replay check"
            );
            (true, "replay_start_deferred")
        }
    };
    let outcome = if requeue {
        IntakeExecution::Deferred
    } else {
        IntakeExecution::ConsumedProtected
    };
    if inflight.intake_outbox_id().is_some() {
        defer(shared, provider, inflight, cancel_token, false, reason).await;
        return Ok(outcome);
    }
    if defer(shared, provider, inflight, cancel_token, requeue, reason).await || !requeue {
        return Ok(outcome);
    }
    Err(format!(
        "replay start deferred; channel {} refused the requeue",
        inflight.channel_id
    )
    .into())
}
