//! Explicit operator release of one mailbox episode. An inflight identity is
//! required; unsupported or protected states are reported before submitting.
use std::{
    sync::{Arc, OnceLock},
    time::Instant,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serenity::model::id::{ChannelId, MessageId};

use super::{SharedData, inflight, turn_finalizer::*};
use crate::services::{provider::ProviderKind, turn_orchestrator::FinishTurnResult};

impl SharedData {
    /// Non-creating lookup for probes and operator recovery.
    pub(in crate::services::discord) fn mailbox_peek(
        &self,
        channel_id: ChannelId,
    ) -> Option<crate::services::turn_orchestrator::ChannelMailboxHandle> {
        self.mailboxes.peek(channel_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeaseIdentity {
    pub(crate) provider: String,
    pub(crate) channel_id: u64,
    generation: u64,
    runtime: String,
    user_message_id: u64,
    turn_nonce: String,
    started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseRequest {
    pub(crate) expected: LeaseIdentity,
    pub(crate) reason: String,
}

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct OperatorRelease {
    request: ReleaseRequest,
    observed_before: Instant,
    clear_outcome: Arc<OnceLock<inflight::GuardedClearOutcome>>,
}

async fn identity(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Option<LeaseIdentity>, String> {
    let Some(mailbox) = shared.mailbox_peek(channel) else {
        return Ok(None);
    };
    let snapshot = mailbox.snapshot().await;
    if snapshot.cancel_token.is_none() {
        return Ok(None);
    }
    Ok(Some(LeaseIdentity {
        provider: provider.as_str().into(),
        channel_id: channel.get(),
        generation: shared.restart.current_generation,
        runtime: shared.token_hash.clone(),
        user_message_id: snapshot
            .active_user_message_id
            .map(|id| id.get())
            .filter(|id| *id != 0)
            .ok_or("active lease has no message identity")?,
        turn_nonce: snapshot
            .active_turn_nonce
            .filter(|nonce| !nonce.is_empty())
            .ok_or("active lease has no episode nonce")?,
        started_at: snapshot
            .turn_started_at
            .ok_or("active lease has no start version")?,
    }))
}

fn matching_inflight(
    provider: &ProviderKind,
    expected: &LeaseIdentity,
) -> Result<inflight::InflightTurnState, String> {
    let row = inflight::load_inflight_state(provider, expected.channel_id)
        .ok_or("lease cannot be released: matching inflight identity is missing")?;
    if row.effective_finalizer_turn_id() != expected.user_message_id
        || row.turn_nonce.as_deref() != Some(expected.turn_nonce.as_str())
        || row.restart_mode.is_some()
        || row.rebind_origin
    {
        return Err("lease cannot be released: inflight identity differs or is protected".into());
    }
    Ok(row)
}

async fn release_on(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    request: ReleaseRequest,
) -> Result<serde_json::Value, String> {
    if request.reason.trim().is_empty() {
        return Err("operator reason is required".into());
    }
    if request.expected.generation != shared.restart.current_generation {
        return Err("runtime generation changed; inspect again".into());
    }
    let Some(current) = identity(shared, provider, channel).await? else {
        return Ok(serde_json::json!({"released": false, "status": "already_released"}));
    };
    if current != request.expected {
        return Err("lease changed; inspect again".into());
    }
    matching_inflight(provider, &current)?;
    let key = TurnKey::new(channel, current.user_message_id, current.generation)
        .with_episode_nonce(Some(&current.turn_nonce));
    let clear_outcome = Arc::new(OnceLock::new());
    let event = TerminalEvent::OperatorRelease(Box::new(OperatorRelease {
        request,
        observed_before: Instant::now(),
        clear_outcome: clear_outcome.clone(),
    }));
    match shared
        .turn_finalizer
        .submit_terminal(
            key,
            provider.clone(),
            event,
            FinalizeContext::bridge(),
            shared.clone(),
        )
        .await
    {
        FinalizeOutcome::Finalized {
            removed_token: Some(_),
            ..
        } => {
            if !matches!(
                clear_outcome.get(),
                Some(
                    inflight::GuardedClearOutcome::Cleared | inflight::GuardedClearOutcome::Missing
                )
            ) {
                return Err(format!(
                    "lease released but inflight cleanup is incomplete ({:?}); provider preserved",
                    clear_outcome.get()
                ));
            }
            Ok(serde_json::json!({"released": true, "status": "operator_released"}))
        }
        _ if identity(shared, provider, channel).await?.is_none() => {
            Ok(serde_json::json!({"released": false, "status": "already_released"}))
        }
        _ => Err("lease changed before release committed; inspect again".into()),
    }
}

impl OperatorRelease {
    /// Exact mailbox CAS precedes inflight cleanup, notification and audit.
    pub(in crate::services::discord) async fn claim(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        key: TurnKey,
    ) -> Option<FinishTurnResult> {
        if identity(shared, provider, key.channel_id)
            .await
            .ok()
            .flatten()
            .as_ref()
            != Some(&self.request.expected)
        {
            return None;
        }
        let row = matching_inflight(provider, &self.request.expected).ok()?;
        let result = shared
            .mailbox_peek(key.channel_id)?
            .release_turn_lease_if_matches(
                MessageId::new(key.user_msg_id),
                self.request.expected.turn_nonce.clone(),
                self.observed_before,
                super::queue_persistence_context(shared, provider, key.channel_id),
            )
            .await;
        result.removed_token.as_ref()?;
        shared.mailboxes.recovery_done(key.channel_id).mark_done();
        let cleared = inflight::clear_inflight_state_for_captured_episode(
            provider,
            key.channel_id.get(),
            &inflight::InflightTurnIdentity::from_state(&row),
            Some(&self.request.expected.turn_nonce),
        );
        let _ = self.clear_outcome.set(cleared);
        tracing::warn!(channel_id = key.channel_id.get(), turn_id = key.user_msg_id,
            turn_nonce = %self.request.expected.turn_nonce, generation = key.generation,
            reason = %self.request.reason, inflight_clear = ?cleared,
            "operator_turn_lease_released");
        Some(result)
    }
}

#[cfg(test)]
#[path = "turn_lease_tests.rs"]
mod tests;
