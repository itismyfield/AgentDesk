//! Explicit operator release of one mailbox episode. The mailbox lease is the
//! authority and the durable inflight row is only its projection (#5951 §2), so
//! a missing row neither hides the episode from inspect nor blocks release.
//!
//! Authority pins have TWO sources and both are refused. `restart_mode` lives
//! on the live `CancelToken` and is only PROJECTED onto the row (by
//! `sync_inflight_restart_mode_from_cancel`), so the token is checked first and
//! a planned-restart episode stays protected even with no row at all.
//! `rebind_origin` exists on the row alone: a release that finds no row cannot
//! observe it, and reports `rebind_pin_verified: false` rather than implying a
//! complete pin check.
use std::{
    sync::{Arc, OnceLock},
    time::Instant,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serenity::model::id::{ChannelId, MessageId};

use super::{SharedData, inflight, turn_finalizer::*};
use crate::services::{
    provider::ProviderKind,
    turn_orchestrator::{ChannelMailboxHandle, FinishTurnResult},
};

mod registry;
pub(crate) use registry::{inspect, release};

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
    pub(in crate::services::discord) user_message_id: u64,
    pub(in crate::services::discord) turn_nonce: String,
    pub(in crate::services::discord) started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseRequest {
    pub(crate) expected: LeaseIdentity,
    pub(crate) reason: String,
}

#[derive(Clone)]
pub(in crate::services::discord) struct OperatorRelease {
    request: ReleaseRequest,
    mailbox: Option<ChannelMailboxHandle>,
    observed_before: Instant,
    clear_outcome: Arc<OnceLock<inflight::GuardedClearOutcome>>,
}

impl std::fmt::Debug for OperatorRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorRelease")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

async fn identity(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Option<LeaseIdentity>, String> {
    identity_for_actor(shared, provider, channel, None).await
}

fn is_current_actor(
    shared: &SharedData,
    channel: ChannelId,
    mailbox: &ChannelMailboxHandle,
) -> bool {
    shared
        .mailbox_peek(channel)
        .is_some_and(|current| mailbox.same_actor(&current))
}

async fn identity_for_actor(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    mailbox: Option<&ChannelMailboxHandle>,
) -> Result<Option<LeaseIdentity>, String> {
    let snapshot = if let Some(mailbox) = mailbox {
        if !is_current_actor(shared, channel, mailbox) {
            return Err("mailbox incarnation changed".into());
        }
        mailbox
            .try_snapshot()
            .await
            .map_err(|_| "mailbox observation unavailable")?
    } else {
        let Some(mailbox) = shared.mailbox_peek(channel) else {
            return Ok(None);
        };
        mailbox.snapshot().await
    };
    if mailbox.is_some_and(|mailbox| !is_current_actor(shared, channel, mailbox)) {
        return Err("mailbox incarnation changed".into());
    }
    let Some(token) = snapshot.cancel_token.clone() else {
        return Ok(None);
    };
    // #5951 P1-1 — the lease is the authority, so its PIN is the token's too.
    // `cancel_active_token` stamps `restart_mode` on the live `CancelToken` and
    // the row only receives a copy, so a row-only check would hand a
    // planned-restart episode to an operator the moment its projection is gone
    // — precisely the rowless case this lane opens. Refuse from the authority.
    if token.restart_mode().is_some() {
        return Err(
            "lease cannot be released: the active episode is pinned by a planned restart".into(),
        );
    }
    Ok(Some(LeaseIdentity {
        provider: provider.as_str().into(),
        channel_id: channel.get(),
        generation: shared.restart.current_generation,
        runtime: shared.token_hash.clone(),
        // #5951 RG1 — a recovery / TUI-direct episode binds no user message
        // (`active_user_message_id` is `None`, because `MessageId::new(0)`
        // panics). Such a lease reports the canonical id 0 instead of refusing:
        // an operator must be able to SEE a turn before deciding about it.
        user_message_id: snapshot.active_user_message_id.map_or(0, |id| id.get()),
        turn_nonce: snapshot
            .active_turn_nonce
            .filter(|nonce| !nonce.is_empty())
            .ok_or("active lease has no episode nonce")?,
        started_at: snapshot
            .turn_started_at
            .ok_or("active lease has no start version")?,
    }))
}

fn read_inflight(
    provider: &ProviderKind,
    channel: u64,
    strict: bool,
) -> Result<Option<inflight::InflightTurnState>, String> {
    if strict {
        return inflight::load_inflight_state_read_only_result(provider, channel)
            .map_err(|error| format!("inflight observation unavailable: {error}"));
    }
    Ok(inflight::load_inflight_state_read_only(provider, channel))
}

fn matching_inflight(
    provider: &ProviderKind,
    expected: &LeaseIdentity,
) -> Result<Option<inflight::InflightTurnState>, String> {
    matching_inflight_for_actor(provider, expected, false)
}

#[cfg(unix)]
pub(in crate::services::discord) fn matching_inflight_for_hold_bound(
    provider: &ProviderKind,
    expected: &LeaseIdentity,
) -> Result<Option<inflight::InflightTurnState>, String> {
    matching_inflight_for_actor(provider, expected, true)
}

/// #5951 RG1 — the row is a projection, not the authority, so its ABSENCE is
/// reported as `Ok(None)` rather than refused: that is the rowless lane an
/// operator needs when no automatic path can see the turn any more. A row that
/// exists still has to name this exact episode, and `rebind_origin` (plus a
/// `restart_mode` copy) keeps pinning authority — those stay refusals, reported
/// apart from a plain identity mismatch so an operator can tell the two states
/// apart.
fn matching_inflight_for_actor(
    provider: &ProviderKind,
    expected: &LeaseIdentity,
    strict: bool,
) -> Result<Option<inflight::InflightTurnState>, String> {
    // #5951 P2-1 — read-only: the plain loader rewrites the sidecar under a
    // lock whenever it backfills `finalizer_turn_id`, and inspect is advertised
    // as a non-mutating probe.
    let Some(row) = read_inflight(provider, expected.channel_id, strict)? else {
        return Ok(None);
    };
    // #5951 P1-2 — an episode that bound no user message (id 0) has NO id axis
    // to compare. Its row legitimately carries `user_msg_id == 0`, and
    // `effective_finalizer_turn_id()` then synthesises a non-zero id that can
    // never equal 0, so an unconditional `!=` would reject the episode's own
    // row. Fall back to the nonce axis alone — the same exemption
    // `InflightTurnState::matches_finalizer_turn_id` makes for `expected == 0`.
    let identity_differs = (expected.user_message_id != 0
        && row.effective_finalizer_turn_id() != expected.user_message_id)
        || row.turn_nonce.as_deref() != Some(expected.turn_nonce.as_str());
    if identity_differs {
        return Err("lease cannot be released: inflight identity differs from this episode".into());
    }
    if row.restart_mode.is_some() || row.rebind_origin {
        return Err("lease cannot be released: inflight identity is protected by a planned restart or a rebind origin".into());
    }
    Ok(Some(row))
}

async fn release_on(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    request: ReleaseRequest,
) -> Result<serde_json::Value, String> {
    release_on_actor(shared, provider, channel, request, None).await
}

#[cfg(unix)]
pub(in crate::services::discord) async fn identity_for_hold_bound(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    mailbox: &ChannelMailboxHandle,
) -> Result<Option<LeaseIdentity>, String> {
    let lease = identity_for_actor(shared, provider, channel, Some(mailbox)).await?;
    if let Some(expected) = lease.as_ref() {
        let _ = matching_inflight_for_hold_bound(provider, expected)?;
    }
    Ok(lease)
}

#[cfg(unix)]
pub(in crate::services::discord) async fn release_for_hold_bound(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    mailbox: &ChannelMailboxHandle,
    expected: LeaseIdentity,
) -> Result<bool, String> {
    let request = ReleaseRequest {
        expected,
        reason: "input_hold_bound".into(),
    };
    let response = release_on_actor(shared, provider, channel, request, Some(mailbox)).await?;
    Ok(response["released"].as_bool() == Some(true))
}

async fn release_on_actor(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    request: ReleaseRequest,
    mailbox: Option<&ChannelMailboxHandle>,
) -> Result<serde_json::Value, String> {
    if request.reason.trim().is_empty() {
        return Err("operator reason is required".into());
    }
    if request.expected.generation != shared.restart.current_generation {
        return Err("runtime generation changed; inspect again".into());
    }
    let Some(current) = identity_for_actor(shared, provider, channel, mailbox).await? else {
        return Ok(serde_json::json!({"released": false, "status": "already_released"}));
    };
    if current != request.expected {
        return Err("lease changed; inspect again".into());
    }
    let _ = matching_inflight_for_actor(provider, &current, mailbox.is_some())?;
    let held = match mailbox {
        Some(mailbox) => mailbox.snapshot().await,
        None => shared.mailbox(channel).snapshot().await,
    }
    .cancel_token;
    let provider_preserved = held
        .as_ref()
        .filter(|token| token.turn_nonce() == Some(current.turn_nonce.as_str()))
        .is_some_and(|token| {
            token.herdr_interrupt_state().is_some()
                || token.tmux_session_name().is_some_and(|name| {
                    #[cfg(unix)]
                    {
                        super::turn_bridge::herdr_marked(&name)
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = name;
                        false
                    }
                })
        });
    // The mailbox CAS (`release_turn_lease_if_matches`) binds an exact
    // `MessageId`, and an episode that never bound one has no such key —
    // `MessageId::new(0)` panics. Refuse explicitly instead of releasing an
    // unidentified lease through an unguarded finish.
    if current.user_message_id == 0 {
        return Err(
            "lease cannot be released: episode has no bound message identity for the release CAS"
                .into(),
        );
    }
    let key = TurnKey::new(channel, current.user_message_id, current.generation)
        .with_episode_nonce(Some(&current.turn_nonce));
    let clear_outcome = Arc::new(OnceLock::new());
    let event = TerminalEvent::OperatorRelease(Box::new(OperatorRelease {
        request,
        mailbox: mailbox.cloned(),
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
            // `rebind_origin` has no in-memory counterpart, so a release that
            // cleared no row could not observe it. Report that instead of
            // implying every pin was checked.
            Ok(serde_json::json!({
                "released": true,
                "status": "operator_released",
                "settlement": "operator_released",
                "provider_preserved": provider_preserved,
                "provider_terminal_confirmed": false,
                "rebind_pin_verified": matches!(
                    clear_outcome.get(),
                    Some(inflight::GuardedClearOutcome::Cleared)
                ),
            }))
        }
        _ if identity_for_actor(shared, provider, channel, mailbox)
            .await?
            .is_none() =>
        {
            Ok(serde_json::json!({"released": false, "status": "already_released"}))
        }
        _ => Err("lease changed before release committed; inspect again".into()),
    }
}

impl OperatorRelease {
    /// Clear a projection that appeared after the rowless check ONLY while its
    /// nonce still names this episode; anything else belongs to a successor.
    fn clear_late_projection(
        provider: &ProviderKind,
        channel_id: ChannelId,
        turn_nonce: &str,
        strict: bool,
    ) -> inflight::GuardedClearOutcome {
        match read_inflight(provider, channel_id.get(), strict) {
            Ok(Some(row)) if row.turn_nonce.as_deref() == Some(turn_nonce) => {
                inflight::clear_inflight_state_for_captured_episode(
                    provider,
                    channel_id.get(),
                    &inflight::InflightTurnIdentity::from_state(&row),
                    Some(turn_nonce),
                )
            }
            Ok(_) => inflight::GuardedClearOutcome::Missing,
            Err(_) => inflight::GuardedClearOutcome::IoError,
        }
    }

    /// Exact mailbox CAS precedes inflight cleanup, notification and audit.
    pub(in crate::services::discord) async fn claim(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        key: TurnKey,
    ) -> Option<FinishTurnResult> {
        if identity_for_actor(shared, provider, key.channel_id, self.mailbox.as_ref())
            .await
            .ok()
            .flatten()
            .as_ref()
            != Some(&self.request.expected)
        {
            return None;
        }
        let strict = self.mailbox.is_some();
        let row = matching_inflight_for_actor(provider, &self.request.expected, strict).ok()?;
        // `MessageId::new(0)` panics and an unbound episode carries no CAS key.
        // `release_on` already refuses id 0 before building the `TurnKey`, so
        // this is unreachable from the HTTP lane; it is the guard for a direct
        // `claim` caller, and `claim_refuses_unbound_message_id_without_panic`
        // exercises exactly that entry.
        if key.user_msg_id == 0 {
            return None;
        }
        // A replacement mailbox must never receive a cleanup authorized on the old actor.
        let handle = match self.mailbox.as_ref() {
            Some(mailbox) if is_current_actor(shared, key.channel_id, mailbox) => mailbox.clone(),
            Some(_) => return None,
            None => shared.mailbox_peek(key.channel_id)?,
        };
        let result = handle
            .release_turn_lease_if_matches(
                MessageId::new(key.user_msg_id),
                self.request.expected.turn_nonce.clone(),
                self.observed_before,
                super::queue_persistence_context(shared, provider, key.channel_id),
            )
            .await;
        result.removed_token.as_ref()?;
        handle.recovery_done().mark_done();
        let cleared = match row.as_ref() {
            Some(row) => inflight::clear_inflight_state_for_captured_episode(
                provider,
                key.channel_id.get(),
                &inflight::InflightTurnIdentity::from_state(row),
                Some(&self.request.expected.turn_nonce),
            ),
            // #5951 P2-3 — a row appearing between the rowless check and the
            // CAS is THIS episode's late projection (a stream tick persist),
            // not a successor's: no successor can start until the CAS above
            // released the lease. Sweep it, but only while it still names this
            // episode, so a successor that did start keeps its own row.
            None => Self::clear_late_projection(
                provider,
                key.channel_id,
                &self.request.expected.turn_nonce,
                strict,
            ),
        };
        let _ = self.clear_outcome.set(cleared);
        #[cfg(test)]
        tests::pause_operator_claim().await;
        tracing::warn!(channel_id = key.channel_id.get(), turn_id = key.user_msg_id,
            turn_nonce = %self.request.expected.turn_nonce, generation = key.generation,
            reason = %self.request.reason, inflight_clear = ?cleared,
            settlement = "operator_released", provider_terminal_confirmed = false,
            "operator_turn_lease_released");
        Some(result)
    }
}

#[cfg(test)]
#[path = "turn_lease_tests.rs"]
mod tests;
