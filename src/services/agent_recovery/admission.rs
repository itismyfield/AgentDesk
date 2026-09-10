//! Shared mailbox admission. No caller-controlled metadata can grant a pending lease.

use super::checkpoint::{load_channel_state_in_tx, lock_admission};
use super::durable::{Coordinator, coordinator, lock};
use super::{ChannelRecoveryStatus, ChannelState, ProviderKind, RecoveryLease};
use sqlx::{Postgres, Transaction};
use std::future::Future;

tokio::task_local! {
    static RECOVERY_START: RecoveryLease;
    static TURN_IDENTITY: (ProviderKind, Option<String>);
}

pub(crate) async fn with_recovery_start<F: Future>(lease: RecoveryLease, future: F) -> F::Output {
    RECOVERY_START.scope(lease, future).await
}

pub(crate) async fn with_turn_identity<F: Future>(
    provider: ProviderKind,
    agent: Option<String>,
    future: F,
) -> F::Output {
    TURN_IDENTITY.scope((provider, agent), future).await
}

/// Held through actual mailbox/token reservation, not just the routing check.
pub(crate) struct AdmissionGuard {
    _transaction: Option<Transaction<'static, Postgres>>,
}

fn allows(
    state: &ChannelState,
    provider: &ProviderKind,
    agent: Option<&str>,
    permit: Option<&RecoveryLease>,
) -> bool {
    if !state.lock_held() {
        return true;
    }
    if state
        .context
        .as_ref()
        .and_then(|context| context.provider(state, &state.active_writer_agent_id))
        .as_ref()
        != Some(provider)
        || agent.is_some_and(|agent| agent != state.active_writer_agent_id)
        // Provider identity alone cannot distinguish two accounts.
        || (agent.is_none() && state.context.as_ref().is_some_and(|context| context.owner_provider == context.fallback_provider))
    {
        return false;
    }
    if state.status == ChannelRecoveryStatus::FallbackRunning {
        return true;
    }
    matches!(
        state.status,
        ChannelRecoveryStatus::TakeoverPending | ChannelRecoveryStatus::RestorePending
    ) && permit.is_some_and(|permit| permit == &RecoveryLease::from_state(state))
}

pub(crate) async fn admit(
    channel: &str,
    default_provider: &ProviderKind,
) -> Result<AdmissionGuard, String> {
    let (provider, agent) = TURN_IDENTITY
        .try_with(Clone::clone)
        .unwrap_or((default_provider.clone(), None));
    let permit = RECOVERY_START.try_with(Clone::clone).ok();
    admit_on(
        coordinator(),
        channel,
        &provider,
        agent.as_deref(),
        permit.as_ref(),
    )
    .await
}

pub(super) async fn admit_on(
    coordinator: &Coordinator,
    channel: &str,
    provider: &ProviderKind,
    agent: Option<&str>,
    permit: Option<&RecoveryLease>,
) -> Result<AdmissionGuard, String> {
    let pool = lock(&coordinator.pool).clone();
    let Some(pool) = pool else {
        let runtime = lock(&coordinator.runtime);
        if runtime.catalog.policy_for_channel(channel).is_some()
            || runtime
                .states
                .get(channel)
                .is_some_and(ChannelState::lock_held)
        {
            return Err("recovery ownership store unavailable".into());
        }
        return Ok(AdmissionGuard { _transaction: None });
    };
    let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
    lock_admission(&mut tx, channel)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(state) = load_channel_state_in_tx(&mut tx, channel)
        .await
        .map_err(|error| error.to_string())?
    {
        if !allows(&state, provider, agent, permit) {
            return Err("channel is fenced by another recovery lease or a pending handoff".into());
        }
        // The finalizer captures the same generation admitted at mailbox start.
        lock(&coordinator.runtime)
            .states
            .insert(channel.to_string(), state);
    }
    Ok(AdmissionGuard {
        _transaction: Some(tx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_recovery::{
        DetectorSignal, ObserveInput,
        tests::{CHANNEL, enabled_runtime},
    };

    #[test]
    fn pending_intent_only_accepts_its_exact_internal_lease() {
        let mut runtime = enabled_runtime();
        runtime.observe(ObserveInput {
            channel_id: CHANNEL.into(),
            primary_turn_id: "turn".into(),
            signal: DetectorSignal::StreamIdleTimeout,
        });
        let state = &runtime.states[CHANNEL];
        let lease = RecoveryLease::from_state(state);
        assert!(!allows(state, &ProviderKind::Grok, Some("claude"), None));
        assert!(!allows(
            state,
            &ProviderKind::Codex,
            Some("monitoring"),
            None
        ));
        assert!(allows(
            state,
            &ProviderKind::Codex,
            Some("monitoring"),
            Some(&lease)
        ));
        assert!(!allows(
            state,
            &ProviderKind::Codex,
            Some("different-agent"),
            Some(&lease)
        ));
        let mut stale = lease.clone();
        stale.generation -= 1;
        assert!(!allows(state, &ProviderKind::Codex, None, Some(&stale)));
        runtime.acknowledge_start(&lease).unwrap();
        assert!(allows(
            &runtime.states[CHANNEL],
            &ProviderKind::Codex,
            Some("monitoring"),
            None
        ));
        assert!(!allows(
            &runtime.states[CHANNEL],
            &ProviderKind::Grok,
            None,
            None
        ));
    }

    #[test]
    fn same_provider_fallback_never_admits_owner_or_unscoped_turns() {
        let mut runtime = enabled_runtime();
        runtime.observe(ObserveInput {
            channel_id: CHANNEL.into(),
            primary_turn_id: "turn".into(),
            signal: DetectorSignal::TurnRateLimit,
        });
        let state = runtime.states.get_mut(CHANNEL).unwrap();
        state.context.as_mut().unwrap().owner_provider = "codex".into();
        let lease = RecoveryLease::from_state(state);
        assert!(!allows(state, &ProviderKind::Codex, None, Some(&lease)));
        assert!(!allows(
            state,
            &ProviderKind::Codex,
            Some("claude"),
            Some(&lease)
        ));
        assert!(allows(
            state,
            &ProviderKind::Codex,
            Some("monitoring"),
            Some(&lease)
        ));
        runtime.acknowledge_start(&lease).unwrap();
        let state = &runtime.states[CHANNEL];
        assert!(!allows(state, &ProviderKind::Codex, None, None));
        assert!(!allows(state, &ProviderKind::Codex, Some("claude"), None));
        assert!(allows(
            state,
            &ProviderKind::Codex,
            Some("monitoring"),
            None
        ));
    }
}
