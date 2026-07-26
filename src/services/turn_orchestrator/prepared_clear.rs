use poise::serenity_prelude::ChannelId;

use super::{
    ActiveTurnKind, BeginResumeTransitionResult, ChannelMailboxState, EndResumeTransitionResult,
    Intervention, QueueExitEvent, QueueExitKind, QueuePersistenceContext, ResumeTransitionKey,
    ResumeTransitionMutationRefusal,
};
use crate::services::provider::CancelToken;
use std::sync::Arc;

pub(crate) struct ClearChannelResult {
    pub(crate) removed_token: Option<Arc<CancelToken>>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    // Durable clear commit result. Callers must not perform runtime teardown or
    // complete recovery authority when this is `Some`.
    pub(crate) persistence_error: Option<String>,
    pub(crate) refused_resume_transition: bool,
}

pub(crate) struct PrepareChannelClearResult {
    pub(crate) key: Option<ResumeTransitionKey>,
    pub(crate) persistence_error: Option<String>,
    pub(crate) refused_resume_transition: bool,
}

pub(crate) struct AbortPreparedClearResult {
    pub(crate) transition_result: EndResumeTransitionResult,
    pub(crate) persistence_error: Option<String>,
}

pub(super) struct PreparedChannelClear {
    pub(super) key: super::ResumeTransitionKey,
    pub(super) persistence: QueuePersistenceContext,
    pub(super) previous_queue: Vec<Intervention>,
    pub(super) queue_exit_events: Vec<QueueExitEvent>,
}

pub(super) fn apply_prepared_channel_clear(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Option<Arc<CancelToken>> {
    let removed_token = state.cancel_token.take();
    state.active_request_owner = None;
    state.active_user_message_id = None;
    state.active_turn_nonce = None;
    state.active_turn_kind = ActiveTurnKind::default();
    state.recovery_started_at = None;
    state.turn_started_at = None;
    state.turn_started_instant = None;
    super::reset_watchdog_extension_state(state);
    super::clear_pending_user_dispatch(state);
    state.recently_valve_cleared_dispatch = None;
    super::delete_pending_dispatch_marker_with_persistence(persistence, channel_id, "clear");
    super::mark_turn_finished_signal_done(channel_id);
    removed_token
}

pub(super) fn persist_and_apply_channel_clear(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
    operation: &str,
) -> ClearChannelResult {
    state.last_persistence = Some(persistence.clone());
    let previous_queue = state.intervention_queue.clone();
    let queue_exit_events = state
        .intervention_queue
        .drain(..)
        .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded))
        .collect();
    if let Err(error) =
        super::persist_queue_or_restore(state, channel_id, &persistence, previous_queue, operation)
    {
        return ClearChannelResult {
            removed_token: None,
            queue_exit_events: Vec::new(),
            persistence_error: Some(error),
            refused_resume_transition: false,
        };
    }
    ClearChannelResult {
        removed_token: apply_prepared_channel_clear(state, channel_id, &persistence),
        queue_exit_events,
        persistence_error: None,
        refused_resume_transition: false,
    }
}

pub(super) fn abort_prepared_channel_clear(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    key: ResumeTransitionKey,
) -> AbortPreparedClearResult {
    let prepared = state.prepared_clear.take();
    let persistence_error = match prepared {
        Some(prepared) if prepared.key == key => {
            state.intervention_queue = prepared.previous_queue.clone();
            match super::persist_queue(channel_id, &state.intervention_queue, &prepared.persistence)
            {
                Ok(()) => None,
                Err(error) => {
                    state.intervention_queue.clear();
                    state.prepared_clear = Some(prepared);
                    Some(error)
                }
            }
        }
        prepared => {
            state.prepared_clear = prepared;
            Some("prepared channel clear identity mismatch".to_string())
        }
    };
    let transition_result = if persistence_error.is_none() {
        state
            .resume_transition
            .abort(key, std::time::Instant::now())
    } else {
        EndResumeTransitionResult::Refused(ResumeTransitionMutationRefusal::Stale {
            current: state.resume_transition.active_key(),
            terminal: None,
        })
    };
    AbortPreparedClearResult {
        transition_result,
        persistence_error,
    }
}

pub(super) fn prepare_channel_clear(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    transition_id: uuid::Uuid,
    persistence: QueuePersistenceContext,
) -> PrepareChannelClearResult {
    state.last_persistence = Some(persistence.clone());
    match state
        .resume_transition
        .begin(transition_id, std::time::Instant::now())
    {
        BeginResumeTransitionResult::Begun(key) => {
            let previous_queue = state.intervention_queue.clone();
            let queue_exit_events = state
                .intervention_queue
                .drain(..)
                .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded))
                .collect::<Vec<_>>();
            if let Err(error) = super::persist_queue_or_restore(
                state,
                channel_id,
                &persistence,
                previous_queue.clone(),
                "prepare_clear",
            ) {
                let _ = state
                    .resume_transition
                    .abort(key, std::time::Instant::now());
                PrepareChannelClearResult {
                    key: None,
                    persistence_error: Some(error),
                    refused_resume_transition: false,
                }
            } else {
                state.prepared_clear = Some(PreparedChannelClear {
                    key,
                    persistence,
                    previous_queue,
                    queue_exit_events,
                });
                PrepareChannelClearResult {
                    key: Some(key),
                    persistence_error: None,
                    refused_resume_transition: false,
                }
            }
        }
        BeginResumeTransitionResult::AlreadyReserved(key)
            if state.prepared_clear.as_ref().map(|prepared| prepared.key) == Some(key) =>
        {
            PrepareChannelClearResult {
                key: Some(key),
                persistence_error: None,
                refused_resume_transition: false,
            }
        }
        _ => PrepareChannelClearResult {
            key: None,
            persistence_error: None,
            refused_resume_transition: true,
        },
    }
}

pub(super) fn commit_prepared_channel_clear(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    key: ResumeTransitionKey,
) -> ClearChannelResult {
    match state.prepared_clear.take() {
        Some(prepared) if prepared.key == key => {
            let removed_token =
                apply_prepared_channel_clear(state, channel_id, &prepared.persistence);
            let _ = state
                .resume_transition
                .complete(key, std::time::Instant::now());
            ClearChannelResult {
                removed_token,
                queue_exit_events: prepared.queue_exit_events,
                persistence_error: None,
                refused_resume_transition: false,
            }
        }
        prepared => {
            state.prepared_clear = prepared;
            ClearChannelResult {
                removed_token: None,
                queue_exit_events: Vec::new(),
                persistence_error: None,
                refused_resume_transition: true,
            }
        }
    }
}
