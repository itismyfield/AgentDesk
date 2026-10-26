//! The same committed begin and post-ACK reset for intake and headless starts.
use super::*;
use crate::services::discord::replay_policy::RetryPermit;
use crate::services::discord::router::intake_dispatch::replay_admission::ReplayAdmissionContext;
use crate::services::provider_hosting::ProviderSessionDriver;

pub(in crate::services::discord::router::message_handler) async fn begin(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    inflight: &mut InflightTurnState,
    input: CanonicalInput,
    mut projection: PreparedEffectProjection,
    continuation: Option<RetryPermit>,
    scope: super::super::super::provider_dispatch::replay_scope::Scope,
) -> Result<(ReplayStart, bool), StartStop> {
    let channel = ChannelId::new(inflight.channel_id);
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
    if shared.pg_pool.is_none()
        && continuation.is_none()
        && let Some(legacy) = admission::dormant_start_for_scope(
            &input.channel,
            provider.as_str(),
            scope.driver.as_str(),
            scope.runtime,
        )
    {
        return Ok((legacy, false));
    }
    binding.entry("replay_version").or_insert(1.into());
    binding.entry("replay_mode").or_insert("protected".into());
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or_else(|| StartStop::Deferred("no replay store; retry this request".into()))?;
    let instance =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let ctx = ReplayAdmissionContext::production(pool, &instance, admission::incarnation())
        .for_scope(
            &input.channel,
            provider.as_str(),
            scope.driver.as_str(),
            scope.runtime,
            continuation.is_some(),
        );
    if ctx.producers == admission::ProducerMode::Dormant {
        if continuation.is_some() {
            return Err(StartStop::Consumed);
        }
        return admission::admit_start(&ctx, &input, projection, None)
            .await
            .map(|s| (s, false));
    }
    if crate::services::discord::inflight::load_inflight_state(provider, channel.get()).is_some() {
        return Err(StartStop::Deferred(
            "another turn's owner holds the channel".into(),
        ));
    }
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
