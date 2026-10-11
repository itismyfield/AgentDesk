//! The replay check of an intake turn, right before its inflight projection is saved.

use super::*;
use crate::db::replay_disposition::write::CanonicalInput;
use crate::services::discord::replay_policy;
use crate::services::discord::replay_policy::permit::EffectTarget;
use crate::services::discord::router::intake_dispatch::replay_admission::{
    self as admission, PreparedEffectProjection, ReplayAdmissionContext, ReplayStart, StartStop,
};

/// What the turn's start registers besides the inflight request itself.
pub(super) struct StartRequest<'a> {
    pub(super) provider: &'a ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) agent_id: Option<&'a str>,
    pub(super) reply_context: Option<&'a str>,
    pub(super) uploads: &'a crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    pub(super) session_key: Option<&'a str>,
    pub(super) prompt: &'a str,
}

/// A dormant build keeps the legacy start without IO. An active one waits while another turn's
/// owner holds the channel, then begins the request or its exact continuation; `true` means the
/// stale resume was dropped and must not be passed.
pub(super) async fn admit(
    shared: &Arc<SharedData>,
    request: StartRequest<'_>,
    inflight: &mut InflightTurnState,
) -> Result<(ReplayStart, bool), StartStop> {
    let channel = request.channel_id.get().to_string();
    let continuation = replay_policy::take_continuation();
    if continuation.is_none()
        && let Some(start) = admission::dormant_start(&channel)
    {
        return Ok((start, false));
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Err(StartStop::Deferred("no replay store".into()));
    };
    let inflight_store = super::super::super::super::inflight::load_inflight_state;
    if inflight_store(request.provider, request.channel_id.get()).is_some() {
        return Err(StartStop::Deferred(
            "another turn's owner holds the channel".into(),
        ));
    }
    let instance =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let ctx = ReplayAdmissionContext::production(pool, &instance, admission::incarnation())
        .for_channel(&channel);
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
    let exact = continuation.is_some();
    let start = admission::admit_start(&ctx, &input, projection, continuation).await?;
    inflight.replay_receipt_id = start.receipt_id();
    if !exact {
        return Ok((start, false));
    }
    replay_policy::continuation_started();
    let mut core = shared.core.lock().await;
    let reset = || {
        if let Some(session) = core.sessions.get_mut(&request.channel_id) {
            session.clear_provider_session();
        }
        inflight.session_id = None;
    };
    let start = start.exact_reset(reset).map_err(|_| StartStop::Consumed)?;
    Ok((start, true))
}

/// Settles a start that did not happen: a consumed request releases its claim, a deferred one
/// goes back to the front of its queue; `Err` when that custody was refused.
pub(super) async fn stop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    inflight: &InflightTurnState,
    cancel_token: &Arc<CancelToken>,
    stop: StartStop,
) -> Result<(), Error> {
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
    if defer(shared, provider, inflight, cancel_token, requeue, reason).await || !requeue {
        return Ok(());
    }
    Err(format!(
        "replay start deferred; channel {} refused the requeue",
        inflight.channel_id
    )
    .into())
}
