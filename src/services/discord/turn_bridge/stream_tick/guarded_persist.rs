//! Identity-guarded persistence helpers for the periodic stream tick (#4259 R1).

use super::super::*;

pub(super) type GuardedSaveOutcome = crate::services::discord::inflight::GuardedSaveOutcome;

pub(in crate::services::discord::turn_bridge) struct StreamTickCandidateSaveContext<
    'a,
    G: TurnGateway + ?Sized,
> {
    pub(in crate::services::discord::turn_bridge) gateway: &'a G,
    pub(in crate::services::discord::turn_bridge) provider: &'a ProviderKind,
    pub(in crate::services::discord::turn_bridge) token_hash: &'a str,
    pub(in crate::services::discord::turn_bridge) channel_id: ChannelId,
    pub(in crate::services::discord::turn_bridge) persisted_baseline: &'a mut InflightTurnState,
    pub(in crate::services::discord::turn_bridge) inflight_state: &'a mut InflightTurnState,
    pub(in crate::services::discord::turn_bridge) expected_identity:
        &'a crate::services::discord::inflight::InflightTurnIdentity,
    pub(in crate::services::discord::turn_bridge) expected_current_message: &'a mut (u64, usize),
    pub(in crate::services::discord::turn_bridge) current_msg_id: &'a mut MessageId,
    pub(in crate::services::discord::turn_bridge) pending_current_message_candidate:
        &'a mut Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) bridge_created_response_placeholder_msg_id:
        &'a mut Option<MessageId>,
}

/// Durable precondition for a Discord-visible stream mutation.
///
/// A successful identity guard is not sufficient by itself: the same turn may
/// have handed live delivery to a watcher/standby relay.  Only the historical
/// `None` owner is bridge authority.  Store failures fail closed for this tick
/// but remain retryable; a missing/reowned row or a durable non-bridge relay
/// owner permanently ends bridge authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge) enum VisibleMutationAuthority {
    Authorized,
    Suppressed,
    Retry,
    AuthorityLost,
}

impl VisibleMutationAuthority {
    /// `None` is the only disposition that terminates stream lifecycle.
    /// Delegated owners suppress the individual Discord mutation while the
    /// tick continues through reclaim, post-loop finalization, and analytics.
    pub(in crate::services::discord::turn_bridge) fn mutation_permission(self) -> Option<bool> {
        match self {
            Self::Authorized => Some(true),
            Self::Suppressed | Self::Retry => Some(false),
            Self::AuthorityLost => None,
        }
    }
}

pub(in crate::services::discord::turn_bridge) fn visible_mutation_authority_after_guarded_save(
    outcome: GuardedSaveOutcome,
    inflight_state: &InflightTurnState,
    intended_authority: crate::services::discord::inflight::StreamRelayAuthority,
) -> VisibleMutationAuthority {
    use crate::services::discord::inflight::StreamRelayAuthority;

    let authority_unchanged =
        StreamRelayAuthority::from_state(inflight_state) == intended_authority;
    let authority = match outcome {
        GuardedSaveOutcome::Saved
            if authority_unchanged && intended_authority.bridge_owns_relay() =>
        {
            VisibleMutationAuthority::Authorized
        }
        GuardedSaveOutcome::Saved if authority_unchanged => VisibleMutationAuthority::Suppressed,
        GuardedSaveOutcome::Saved
        | GuardedSaveOutcome::Missing
        | GuardedSaveOutcome::IdentityMismatch => VisibleMutationAuthority::AuthorityLost,
        GuardedSaveOutcome::IoError => VisibleMutationAuthority::Retry,
    };
    // #5464 T5 S2: the one observation point that covers all sixteen
    // `authorize_visible_mutation!` sites. It tallies in memory for the cohort
    // and returns `()`, so `authority` reaches the caller unchanged.
    crate::services::discord::relay_recovery::authority_observation::record_stream_loop_gate(
        inflight_state,
        outcome,
        authority_unchanged,
        intended_authority.bridge_owns_relay(),
    );
    authority
}

pub(super) fn sync_stream_tick_tool_fields(
    inflight_state: &mut InflightTurnState,
    current_tool_line: &Option<String>,
    prev_tool_status: &Option<String>,
    last_tool_name: &Option<String>,
    last_tool_summary: &Option<String>,
) {
    inflight_state
        .current_tool_line
        .clone_from(current_tool_line);
    inflight_state.prev_tool_status.clone_from(prev_tool_status);
    inflight_state.last_tool_name.clone_from(last_tool_name);
    inflight_state
        .last_tool_summary
        .clone_from(last_tool_summary);
}

pub(in crate::services::discord::turn_bridge) fn persist_stream_tick_state(
    persisted_baseline: &mut InflightTurnState,
    inflight_state: &mut InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    expected_current_message: &mut (u64, usize),
    detached_current_msg_id: &mut MessageId,
    channel_id: ChannelId,
    caller: &'static str,
) -> GuardedSaveOutcome {
    persist_stream_tick_state_with_authority_mode(
        persisted_baseline,
        inflight_state,
        expected,
        expected_current_message,
        detached_current_msg_id,
        StreamTickSaveOperation {
            channel_id,
            caller,
            mode: StreamTickSaveMode::MergeConcurrentOwner,
        },
    )
}

fn persist_stream_tick_visible_mutation_fence(
    persisted_baseline: &mut InflightTurnState,
    inflight_state: &mut InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    expected_current_message: &mut (u64, usize),
    detached_current_msg_id: &mut MessageId,
    channel_id: ChannelId,
    caller: &'static str,
) -> GuardedSaveOutcome {
    persist_stream_tick_state_with_authority_mode(
        persisted_baseline,
        inflight_state,
        expected,
        expected_current_message,
        detached_current_msg_id,
        StreamTickSaveOperation {
            channel_id,
            caller,
            mode: StreamTickSaveMode::StrictVisibleMutationFence,
        },
    )
}

fn persist_stream_tick_state_with_authority_mode(
    persisted_baseline: &mut InflightTurnState,
    inflight_state: &mut InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    expected_current_message: &mut (u64, usize),
    detached_current_msg_id: &mut MessageId,
    operation: StreamTickSaveOperation,
) -> GuardedSaveOutcome {
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, save_stream_tick_state_if_bridge_authority,
        save_stream_tick_state_preserving_current_message_races,
    };
    let expected_current_before_save = *expected_current_message;
    let outcome = if operation.mode == StreamTickSaveMode::StrictVisibleMutationFence {
        save_stream_tick_state_if_bridge_authority(
            persisted_baseline,
            inflight_state,
            expected,
            expected_current_message.0,
            expected_current_message.1,
            operation.caller,
        )
    } else {
        save_stream_tick_state_preserving_current_message_races(
            persisted_baseline,
            inflight_state,
            expected,
            expected_current_message.0,
            expected_current_message.1,
            operation.caller,
        )
    };
    if outcome == GuardedSaveOutcome::Saved {
        *expected_current_message = (
            inflight_state.current_msg_id,
            inflight_state.current_msg_len,
        );
        *detached_current_msg_id =
            detached_current_msg_id_from_durable(inflight_state.current_msg_id);
    } else if operation.mode == StreamTickSaveMode::StrictVisibleMutationFence
        && outcome == GuardedSaveOutcome::IdentityMismatch
        && expected.matches_state(inflight_state)
        && ((
            inflight_state.current_msg_id,
            inflight_state.current_msg_len,
        ) != expected_current_before_save
            || inflight_state.effective_relay_owner_kind()
                != crate::services::discord::inflight::RelayOwnerKind::None)
    {
        // The strict lock-held fence adopted the exact same-turn durable row.
        // Make candidate cleanup fall back to that authoritative message, not
        // the stale pre-fence epoch.
        *expected_current_message = (
            inflight_state.current_msg_id,
            inflight_state.current_msg_len,
        );
        *detached_current_msg_id =
            detached_current_msg_id_from_durable(inflight_state.current_msg_id);
    }
    if matches!(
        outcome,
        GuardedSaveOutcome::Missing | GuardedSaveOutcome::IdentityMismatch
    ) {
        tracing::warn!(
            channel_id = operation.channel_id.get(),
            caller = operation.caller,
            ?outcome,
            "stream tick guarded save skipped because durable row is no longer owned by this turn"
        );
    }
    outcome
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamTickSaveMode {
    MergeConcurrentOwner,
    StrictVisibleMutationFence,
}

#[derive(Clone, Copy, Debug)]
struct StreamTickSaveOperation {
    channel_id: ChannelId,
    caller: &'static str,
    mode: StreamTickSaveMode,
}

pub(in crate::services::discord::turn_bridge) async fn persist_stream_tick_state_with_candidate_cleanup<
    G: TurnGateway + ?Sized,
>(
    mut context: StreamTickCandidateSaveContext<'_, G>,
    caller: &'static str,
) -> GuardedSaveOutcome {
    persist_stream_tick_state_with_candidate_cleanup_mode(
        &mut context,
        caller,
        StreamTickSaveMode::MergeConcurrentOwner,
    )
    .await
}

pub(super) async fn fence_stream_tick_visible_mutation_with_candidate_cleanup<
    G: TurnGateway + ?Sized,
>(
    mut context: StreamTickCandidateSaveContext<'_, G>,
    caller: &'static str,
) -> GuardedSaveOutcome {
    persist_stream_tick_state_with_candidate_cleanup_mode(
        &mut context,
        caller,
        StreamTickSaveMode::StrictVisibleMutationFence,
    )
    .await
}

async fn persist_stream_tick_state_with_candidate_cleanup_mode<G: TurnGateway + ?Sized>(
    context: &mut StreamTickCandidateSaveContext<'_, G>,
    caller: &'static str,
    mode: StreamTickSaveMode,
) -> GuardedSaveOutcome {
    let outcome = if mode == StreamTickSaveMode::StrictVisibleMutationFence {
        persist_stream_tick_visible_mutation_fence(
            context.persisted_baseline,
            context.inflight_state,
            context.expected_identity,
            context.expected_current_message,
            context.current_msg_id,
            context.channel_id,
            caller,
        )
    } else {
        persist_stream_tick_state(
            context.persisted_baseline,
            context.inflight_state,
            context.expected_identity,
            context.expected_current_message,
            context.current_msg_id,
            context.channel_id,
            caller,
        )
    };
    if outcome == GuardedSaveOutcome::IoError {
        return outcome;
    }
    // Orphan-delete seam for an ABANDONED LOCAL ROLLOVER. The candidate is a
    // Discord message the bridge already CREATED but has not yet bound to the
    // durable row. A `Saved` whose durable epoch is this candidate is the bind
    // completing, and only that clears the candidate without a delete. Every
    // other disposition — including a `Saved` whose merge kept a DIFFERENT
    // epoch, which is what a same-authority durable epoch advance produces —
    // leaves the created message bound to nothing, so it is deleted here.
    // Nothing downstream carries the id, so skipping the delete strands it.
    // Asserted by
    // `abandoned_local_rollover_is_deleted_when_the_fence_adopts_a_durable_epoch`.
    let Some(candidate) = *context.pending_current_message_candidate else {
        return outcome;
    };
    if outcome == GuardedSaveOutcome::Saved
        && context.inflight_state.current_msg_id == candidate.get()
    {
        context.pending_current_message_candidate.take();
        return outcome;
    }
    discard_pending_current_message_candidate(context).await;
    outcome
}

async fn discard_pending_current_message_candidate<G: TurnGateway + ?Sized>(
    context: &mut StreamTickCandidateSaveContext<'_, G>,
) {
    let Some(candidate) = context.pending_current_message_candidate.take() else {
        return;
    };
    if *context.bridge_created_response_placeholder_msg_id == Some(candidate) {
        *context.bridge_created_response_placeholder_msg_id = None;
    }
    context.inflight_state.current_msg_id = context.expected_current_message.0;
    context.inflight_state.current_msg_len = context.expected_current_message.1;
    *context.current_msg_id =
        detached_current_msg_id_from_durable(context.expected_current_message.0);
    cleanup_unbound_bridge_anchor(
        context.gateway,
        context.provider,
        context.token_hash,
        context.channel_id,
        candidate,
    )
    .await;
}

/// A stream-loop break may happen before the next periodic tick. Give a pending
/// response candidate one final guarded bind; if the store is unavailable,
/// discard the unbound Discord message instead of returning an orphan.
pub(in crate::services::discord::turn_bridge) async fn settle_pending_current_message_candidate_on_loop_exit<
    G: TurnGateway + ?Sized,
>(
    mut context: StreamTickCandidateSaveContext<'_, G>,
) -> bool {
    if context.pending_current_message_candidate.is_none() {
        return false;
    }
    let outcome = persist_stream_tick_state_with_candidate_cleanup_mode(
        &mut context,
        "turn_bridge::stream_loop::exit_candidate_flush",
        StreamTickSaveMode::MergeConcurrentOwner,
    )
    .await;
    if outcome == GuardedSaveOutcome::IoError {
        tracing::warn!(
            channel_id = context.channel_id.get(),
            "stream-loop exit could not bind response candidate; discarding unbound message"
        );
        discard_pending_current_message_candidate(&mut context).await;
    }
    debug_assert!(context.pending_current_message_candidate.is_none());
    outcome == GuardedSaveOutcome::Saved
}

pub(super) fn persist_stream_tick_heartbeat(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
) -> GuardedSaveOutcome {
    crate::services::discord::inflight::touch_inflight_state_if_matches_identity(
        provider,
        channel_id.get(),
        expected,
        "turn_bridge::stream_tick::long_running_heartbeat",
    )
}

pub(super) fn dirty_after_guarded_save(outcome: GuardedSaveOutcome) -> bool {
    matches!(outcome, GuardedSaveOutcome::IoError)
}

// #4267: the tests live in a sibling file. Inline, this module's ~1.2k lines
// over ~380 production lines is the test-residue ratio the readability gate
// flags — the module path and every `super::*` reference are unchanged.
#[cfg(test)]
#[path = "guarded_persist_tests.rs"]
mod tests;
