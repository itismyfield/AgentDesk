//! The same committed begin and post-ACK reset for intake and headless starts.
use super::*;
use crate::services::discord::replay_policy::RetryPermit;
use crate::services::discord::router::intake_dispatch::replay_admission::ReplayAdmissionContext;
use crate::services::provider_hosting::ProviderSessionDriver;

pub(in crate::services::discord::router::message_handler) async fn begin(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    inflight: &mut InflightTurnState,
    input: CanonicalInput,
    mut projection: PreparedEffectProjection,
    continuation: Option<RetryPermit>,
) -> Result<(ReplayStart, bool), StartStop> {
    let scope = super::super::super::provider_dispatch::replay_scope::selected(
        provider,
        channel.get(),
        false,
        inflight.tmux_session_name.as_deref(),
    );
    if scope.driver == ProviderSessionDriver::TuiHosting
        || projection.binding["driver_scope"] == "tui-hosting"
    {
        if continuation.is_some() {
            return Err(StartStop::Consumed);
        }
        return admission::dormant_start_for_driver(
            &input.channel,
            ProviderSessionDriver::TuiHosting,
        )
        .map(|start| (start, false))
        .ok_or(StartStop::Consumed);
    }
    let binding = projection
        .binding
        .as_object_mut()
        .ok_or_else(|| StartStop::Deferred("invalid start binding".into()))?;
    binding
        .entry("driver_scope")
        .or_insert_with(|| scope.driver.as_str().into());
    binding
        .entry("runtime_scope")
        .or_insert_with(|| scope.runtime.into());
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or_else(|| StartStop::Deferred("no replay store".into()))?;
    if crate::services::discord::inflight::load_inflight_state(provider, channel.get()).is_some() {
        return Err(StartStop::Deferred(
            "another turn's owner holds the channel".into(),
        ));
    }
    let instance =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let ctx = ReplayAdmissionContext::production(pool, &instance, admission::incarnation())
        .for_channel(&input.channel);
    let exact = continuation.is_some();
    let start = admission::admit_start(&ctx, &input, projection, continuation).await?;
    inflight.replay_receipt_id = start.receipt_id();
    if !exact {
        return Ok((start, false));
    }
    replay_policy::continuation_started();
    let mut core = shared.core.lock().await;
    let start = start
        .exact_reset(|| {
            if let Some(session) = core.sessions.get_mut(&channel) {
                session.clear_provider_session();
            }
            inflight.session_id = None;
        })
        .map_err(|_| StartStop::Consumed)?;
    Ok((start, true))
}
