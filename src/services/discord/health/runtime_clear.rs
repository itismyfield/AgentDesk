use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{HealthRegistry, RuntimeChannelClearResult};
use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::ProviderKind;

pub(crate) struct PreparedRuntimeChannelClear {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    session_key: Option<String>,
    transition_key: crate::services::turn_orchestrator::ResumeTransitionKey,
}

impl PreparedRuntimeChannelClear {
    #[cfg(test)]
    pub(crate) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }
}

pub(crate) enum PrepareRuntimeChannelClearResult {
    Prepared(PreparedRuntimeChannelClear),
    Unavailable,
    DeferredResumeTransition,
    PersistenceFailed,
}

async fn apply_committed_runtime_clear(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    session_key: Option<&str>,
    cleared: crate::services::turn_orchestrator::ClearChannelResult,
) -> RuntimeChannelClearResult {
    if cleared.refused_resume_transition {
        tracing::warn!(
            channel_id = channel_id.get(),
            "runtime clear deferred while resume transition is active"
        );
        return RuntimeChannelClearResult::DeferredResumeTransition;
    }
    if let Some(error) = cleared.persistence_error.as_deref() {
        tracing::warn!(
            channel_id = channel_id.get(),
            %error,
            "runtime clear stopped after mailbox persistence failed"
        );
        return RuntimeChannelClearResult::PersistenceFailed;
    }

    let tmux_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_ref())
            .map(|channel_name| provider.build_tmux_session_name(channel_name))
            .or_else(|| session_key.and_then(tmux_name_from_session_key))
    };

    if let Some(token) = cleared.removed_token {
        discord::turn_bridge::stop_active_turn(
            provider,
            &token,
            discord::TmuxCleanupPolicy::PreserveSession,
            "auto-queue slot clear",
        )
        .await;
        discord::saturating_decrement_global_active(shared);
    }

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            discord::settings::cleanup_channel_uploads(channel_id);
            session.clear_provider_session();
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }

    #[cfg(unix)]
    if let Some(name) = tmux_name {
        if provider.uses_managed_tmux_backend() {
            discord::commands::reset_managed_process_session(&name);
        }
    }

    RuntimeChannelClearResult::Cleared
}

pub(crate) async fn prepare_provider_channel_runtime_clear(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    session_key: Option<&str>,
    transition_id: uuid::Uuid,
) -> PrepareRuntimeChannelClearResult {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return PrepareRuntimeChannelClearResult::Unavailable;
    };
    let Some(shared) = super::shared_for_provider(registry, &provider, channel_id).await else {
        return PrepareRuntimeChannelClearResult::Unavailable;
    };
    let prepared =
        discord::mailbox_prepare_channel_clear(&shared, &provider, channel_id, transition_id).await;
    if prepared.refused_resume_transition {
        return PrepareRuntimeChannelClearResult::DeferredResumeTransition;
    }
    if prepared.persistence_error.is_some() {
        return PrepareRuntimeChannelClearResult::PersistenceFailed;
    }
    let Some(transition_key) = prepared.key else {
        return PrepareRuntimeChannelClearResult::DeferredResumeTransition;
    };
    PrepareRuntimeChannelClearResult::Prepared(PreparedRuntimeChannelClear {
        shared,
        provider,
        channel_id,
        session_key: session_key.map(str::to_owned),
        transition_key,
    })
}

pub(crate) async fn abort_prepared_provider_channel_runtime_clear(
    prepared: PreparedRuntimeChannelClear,
) -> Result<(), String> {
    let result = discord::mailbox_abort_prepared_channel_clear(
        &prepared.shared,
        prepared.channel_id,
        prepared.transition_key,
    )
    .await;
    if let Some(error) = result.persistence_error {
        return Err(error);
    }
    match result.transition_result {
        crate::services::turn_orchestrator::EndResumeTransitionResult::Applied(_)
        | crate::services::turn_orchestrator::EndResumeTransitionResult::AlreadyAppliedInactive(
            _,
        ) => Ok(()),
        crate::services::turn_orchestrator::EndResumeTransitionResult::Refused(refusal) => {
            Err(format!("prepared runtime clear abort refused: {refusal:?}"))
        }
    }
}

pub(crate) async fn commit_prepared_provider_channel_runtime_clear(
    prepared: PreparedRuntimeChannelClear,
) -> RuntimeChannelClearResult {
    let cleared = discord::mailbox_commit_prepared_channel_clear(
        &prepared.shared,
        prepared.channel_id,
        prepared.transition_key,
    )
    .await;
    apply_committed_runtime_clear(
        &prepared.shared,
        &prepared.provider,
        prepared.channel_id,
        prepared.session_key.as_deref(),
        cleared,
    )
    .await
}

pub async fn clear_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    session_key: Option<&str>,
) -> RuntimeChannelClearResult {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return RuntimeChannelClearResult::Unavailable;
    };
    let Some(shared) = super::shared_for_provider(registry, &provider, channel_id).await else {
        return RuntimeChannelClearResult::Unavailable;
    };
    let cleared = discord::mailbox_clear_channel(&shared, &provider, channel_id).await;
    apply_committed_runtime_clear(&shared, &provider, channel_id, session_key, cleared).await
}
