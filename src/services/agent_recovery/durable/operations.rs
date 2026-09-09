//! Reconstruct retryable runtime work from committed intent.
use super::*;
use sqlx::{Postgres, Transaction};

pub(crate) struct ExecutionGuard {
    _transaction: Transaction<'static, Postgres>,
}

/// Distinct from admission's lock so a reconciler can reserve the new mailbox.
pub(crate) async fn try_execution(
    channel: &str,
) -> Result<Option<ExecutionGuard>, RecoveryStoreError> {
    let mut tx = coordinator().pool()?.begin().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(1893, hashtext($1))")
        .bind(channel)
        .fetch_one(&mut *tx)
        .await?;
    Ok(acquired.then_some(ExecutionGuard { _transaction: tx }))
}
pub(crate) enum OperationPlan {
    Fallback(FallbackSpawnPlan),
    Restore(RestorePlan),
}
pub(crate) struct PendingOperation {
    pub lease: RecoveryLease,
    pub owner_provider: ProviderKind,
    pub fallback_provider: ProviderKind,
    pub plan: OperationPlan,
}
pub(crate) async fn recovery_state(
    channel: &str,
) -> Result<Option<ChannelState>, RecoveryStoreError> {
    coordinator().refresh(channel).await?;
    Ok(lock(&coordinator().runtime).states.get(channel).cloned())
}
pub(crate) fn active_channels(provider: &ProviderKind) -> Vec<String> {
    let runtime = lock(&coordinator().runtime);
    runtime
        .states
        .values()
        .filter(|state| {
            state.lock_held()
                && runtime
                    .writer_provider(state, &state.owner_agent_id)
                    .as_ref()
                    == Some(provider)
        })
        .map(|state| state.channel_id.clone())
        .collect()
}
pub(crate) fn owner_provider(channel: &str) -> Option<ProviderKind> {
    let runtime = lock(&coordinator().runtime);
    let binding = runtime.binding_for_channel(channel)?;
    binding.policy.as_ref().filter(|policy| policy.enabled)?;
    Some(binding.owner_provider)
}
pub(crate) async fn pending_operation(
    channel: &str,
) -> Result<Option<PendingOperation>, RecoveryStoreError> {
    coordinator().refresh(channel).await?;
    let runtime = lock(&coordinator().runtime);
    let Some(state) = runtime.states.get(channel) else {
        return Ok(None);
    };
    if !matches!(
        state.status,
        ChannelRecoveryStatus::TakeoverPending | ChannelRecoveryStatus::RestorePending
    ) {
        return Ok(None);
    }
    let binding = runtime
        .binding_for_channel(channel)
        .ok_or_else(|| conflict("pending launch has no durable binding"))?;
    let fallback_provider = runtime
        .writer_provider(state, &state.fallback_agent_id)
        .ok_or_else(|| conflict("fallback provider missing"))?;
    let events = runtime.last_n(channel);
    let plan = if state.status == ChannelRecoveryStatus::TakeoverPending {
        OperationPlan::Fallback(
            FallbackSpawnPlan::from_binding(
                &binding,
                fallback_provider.clone(),
                &events,
                state.generation,
            )
            .ok_or_else(|| conflict("fallback launch binding invalid"))?,
        )
    } else {
        OperationPlan::Restore(build_restore_plan(
            &binding,
            &events,
            true,
            "fallback finished; owner runtime restart",
        ))
    };
    Ok(Some(PendingOperation {
        lease: RecoveryLease::from_state(state),
        owner_provider: binding.owner_provider,
        fallback_provider,
        plan,
    }))
}
pub(crate) async fn acknowledge_start_durable(
    lease: &RecoveryLease,
) -> Result<(), RecoveryStoreError> {
    coordinator()
        .transition(&lease.channel_id, |runtime| {
            runtime.acknowledge_start(lease)
        })
        .await?;
    Ok(())
}
pub(crate) async fn retry_interrupted_durable(channel: &str) -> Result<(), RecoveryStoreError> {
    coordinator()
        .transition(channel, |runtime| runtime.retry_interrupted_launch(channel))
        .await?;
    Ok(())
}
