//! The inflight row the watcher's completion Path B records, and the dispatch bound to it.

use std::future::Future;

use super::*;
use crate::db::session_transcripts::{PersistSessionTranscript, SessionTranscriptEvent};

/// Which read supplied the recorded row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PathBSource {
    /// The re-read after delivery.
    Late,
    /// The pre-relay pin, used when a concurrent finalizer already removed the row.
    Pinned,
}

pub(super) struct PathBRecord<'a> {
    pub(super) state: &'a InflightTurnState,
    pub(super) source: PathBSource,
}

/// Where a Path B transcript row goes, apart from the recorded turn itself.
pub(super) struct PathBTranscript<'a> {
    pub(super) pool: Option<&'a sqlx::PgPool>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: &'a ProviderKind,
    pub(super) agent_id: Option<&'a str>,
    pub(super) assistant_message: &'a str,
    pub(super) events: &'a [SessionTranscriptEvent],
    /// An isolated scheduled turn writes no channel transcript.
    pub(super) channel_effects: crate::services::discord::scheduled_turn::ChannelEffects,
    /// `(tmux_session_name, current_offset)` of the committed output range.
    pub(super) completion_range: (&'a str, u64),
    /// The pre-relay row; the committed range settles its turn before the late row's.
    pub(super) settle_pin: Option<&'a InflightTurnState>,
    /// Native custody of `(pin, late)`; a held row's records belong to its native completion.
    pub(super) native_held: (bool, bool),
}

/// Whether native custody holds `row`, so the watcher records nothing of it.
pub(super) async fn native_held(shared: &SharedData, row: Option<&InflightTurnState>) -> bool {
    match row {
        Some(row) => {
            crate::services::discord::turn_finalizer::native_prepare::holds_row(shared, row).await
        }
        None => false,
    }
}

impl<'a> PathBRecord<'a> {
    /// The late row when present, else the pin when this committed range is the pin's turn.
    pub(super) fn select(
        late: Option<&'a InflightTurnState>,
        pin: Option<&'a InflightTurnState>,
        tmux_session_name: &str,
        current_offset: u64,
    ) -> Option<Self> {
        let record = match late {
            Some(state) => Self {
                state,
                source: PathBSource::Late,
            },
            None if super::turn_identity::pinned_finalize_user_msg_id(
                pin,
                tmux_session_name,
                current_offset,
            ) != 0 =>
            {
                Self {
                    state: pin?,
                    source: PathBSource::Pinned,
                }
            }
            None => return None,
        };
        watcher_completion_lifecycle_applies(record.state).then_some(record)
    }

    /// Dispatch evidence bound to this row; only a late row may borrow the thread's pending one.
    pub(super) async fn dispatch_id<F>(&self, thread_fallback: impl FnOnce() -> F) -> Option<String>
    where
        F: Future<Output = Option<String>>,
    {
        let bound = self.state.dispatch_id.clone().or_else(|| {
            crate::services::discord::adk_session::parse_dispatch_id(&self.state.user_text)
        });
        match (bound, self.source) {
            (Some(dispatch_id), _) => Some(dispatch_id),
            (None, PathBSource::Late) => thread_fallback().await,
            (None, PathBSource::Pinned) => None,
        }
    }

    /// Writes this turn's transcript row and returns the dispatch it attributed.
    pub(super) async fn persist_transcript<F>(
        &self,
        target: PathBTranscript<'_>,
        thread_fallback: impl FnOnce() -> F,
    ) -> Option<String>
    where
        F: Future<Output = Option<String>>,
    {
        let state = self.state;
        let dispatch_id = self.dispatch_id(thread_fallback).await;
        self.settle_scheduled(&target).await;
        if target.channel_effects.suppressed() || self.held(&target) {
            return dispatch_id;
        }
        let turn_id = format!("discord:{}:{}", target.channel_id.get(), state.user_msg_id);
        let channel_id = target.channel_id.get().to_string();
        let persisted = crate::db::session_transcripts::persist_turn_db(
            target.pool,
            PersistSessionTranscript {
                turn_id: &turn_id,
                session_key: state.session_key.as_deref(),
                channel_id: Some(channel_id.as_str()),
                agent_id: target.agent_id,
                provider: Some(target.provider.as_str()),
                dispatch_id: dispatch_id.as_deref(),
                user_message: &state.user_text,
                assistant_message: target.assistant_message,
                events: target.events,
                duration_ms: inflight_duration_ms(Some(state.started_at.as_str())),
                turn_started_at_millis:
                    crate::db::session_transcripts::discord_message_started_at_millis(Some(
                        serenity::MessageId::new(state.user_msg_id),
                    )),
            },
        )
        .await;
        if let Err(e) = persisted {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ watcher: failed to persist session transcript: {e}");
        }
        dispatch_id
    }

    fn held(&self, target: &PathBTranscript<'_>) -> bool {
        match self.source {
            PathBSource::Late => target.native_held.1,
            PathBSource::Pinned => target.native_held.0,
        }
    }

    /// Idempotent fallback of [`settle_committed_range`] for the pinned turn, else this row.
    async fn settle_scheduled(&self, target: &PathBTranscript<'_>) {
        if !crate::services::discord::scheduled_turn::fallback_settle_enabled() {
            return;
        }
        let held = match target.settle_pin {
            Some(_) => target.native_held.0,
            None => self.held(target),
        };
        settle_committed_range(
            target.pool,
            (target.settle_pin.unwrap_or(self.state), held),
            target.completion_range,
            target.assistant_message,
            target.events,
        )
        .await;
    }
}

/// Settle `state`'s scheduled delivery only when its turn owns this committed range (same session,
/// started before its end) and native custody does not hold it. The watcher runs it before any release.
pub(super) async fn settle_committed_range(
    pool: Option<&sqlx::PgPool>,
    (state, native_held): (&InflightTurnState, bool),
    (tmux_session_name, current_offset): (&str, u64),
    assistant_message: &str,
    events: &[SessionTranscriptEvent],
) {
    if native_held
        || !state.scheduled.may_settle()
        || state.user_msg_id == 0
        || super::turn_identity::pinned_finalize_user_msg_id(
            Some(state),
            tmux_session_name,
            current_offset,
        ) != state.user_msg_id
    {
        return;
    }
    let outcome = crate::services::scheduled_messages::turn_completion::classify(
        assistant_message,
        crate::services::scheduled_messages::turn_completion::terminal_error_event(events),
        false,
    );
    crate::services::discord::scheduled_turn::settle(pool, state, outcome).await;
}

/// Commits the receipt of the turn that owns this committed range before any release can drop
/// its row. True when the commit failed and the row stays marked instead.
pub(super) async fn record_receipt(
    pool: Option<&sqlx::PgPool>,
    state: Option<&InflightTurnState>,
    (tmux_session_name, current_offset): (&str, u64),
    outcome: crate::db::turns::TerminalOutcome,
) -> bool {
    let Some(state) = state.filter(|state| {
        state.user_msg_id != 0
            && super::turn_identity::pinned_finalize_user_msg_id(
                Some(state),
                tmux_session_name,
                current_offset,
            ) == state.user_msg_id
    }) else {
        return false;
    };
    !crate::services::discord::receipt_settle::record_or_stamp(pool, state, outcome).await
}

pub(in crate::services::discord) type OwnedRow<'a> = (bool, Option<&'a InflightTurnState>);

pub(super) fn native_owns_work(pin: OwnedRow<'_>, late: OwnedRow<'_>) -> bool {
    [pin, late]
        .into_iter()
        .any(|(held, row)| held && row.is_some_and(|row| row.dispatch_id.is_some()))
}

pub(in crate::services::discord) struct WatcherDispatch<'a> {
    pub(in crate::services::discord) shared: &'a SharedData,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) watcher_provider: &'a ProviderKind,
    pub(in crate::services::discord) tmux_session_name: &'a str,
    pub(in crate::services::discord) pin: OwnedRow<'a>,
    pub(in crate::services::discord) late: OwnedRow<'a>,
    pub(in crate::services::discord) full_response: &'a str,
    pub(in crate::services::discord) has_assistant_response: bool,
    pub(in crate::services::discord) lifecycle_stage_paused: bool,
    pub(in crate::services::discord) completion_is_stale_for_newer_turn: bool,
}

pub(in crate::services::discord) async fn finalize_dispatch(
    request: WatcherDispatch<'_>,
) -> (Option<String>, bool) {
    let WatcherDispatch {
        shared,
        channel_id,
        watcher_provider,
        tmux_session_name,
        pin,
        late,
        full_response,
        has_assistant_response,
        lifecycle_stage_paused,
        completion_is_stale_for_newer_turn,
    } = request;
    let resolved_did = late
        .1
        .and_then(|state| state.dispatch_id.clone())
        .or_else(|| {
            late.1.and_then(|state| {
                crate::services::discord::adk_session::parse_dispatch_id(&state.user_text)
            })
        })
        .or(
            crate::services::discord::adk_session::lookup_pending_dispatch_for_thread(
                shared.api_port,
                channel_id.get(),
            )
            .await,
        )
        .or_else(|| {
            resolve_dispatched_thread_dispatch_from_db(shared.pg_pool.as_ref(), channel_id.get())
        });

    if resolved_did.is_none() && has_assistant_response {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: no dispatch id resolved for channel {} after terminal success",
            channel_id.get()
        );
    }
    let current_worktree_path = {
        let mut data = shared.core.lock().await;
        data.sessions
            .get_mut(&channel_id)
            .and_then(|session| session.validated_path(channel_id.get()))
    };

    // A paused lifecycle keeps dispatch work queued until terminal delivery is ready.
    let dispatch_ok = if lifecycle_stage_paused {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            provider = %watcher_provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            "[{ts}] ⚠ watcher: dispatch finalization deferred — TUI quiescence gate timed out (#2161)"
        );
        false
    } else if let Some(did) = resolved_did
        .as_deref()
        .filter(|_| !completion_is_stale_for_newer_turn && !native_owns_work(pin, late))
    {
        // A stale range cannot complete a newer row’s dispatch.
        #[cfg(test)]
        crate::services::discord::inflight::o_seed_observation::record_event(
            watcher_provider,
            channel_id.get(),
            "watcher_dispatch_call",
        );
        let finalization =
            crate::services::discord::streaming_finalizer::finalize_watcher_streaming_dispatch(
                crate::services::discord::streaming_finalizer::WatcherStreamingFinalRequest {
                    pg_pool: shared.pg_pool.as_ref(),
                    dispatch_id: did,
                    adk_cwd: current_worktree_path.as_deref(),
                    full_response,
                    has_assistant_response,
                },
            )
            .await;
        if !finalization.completed {
            tracing::debug!(
                disposition = ?finalization.disposition,
                dispatch_type = ?finalization.dispatch_type,
                error = ?finalization.error,
                "watcher streaming finalizer preserved dispatch state"
            );
        }
        finalization.completed
    } else {
        true
    };
    (resolved_did, dispatch_ok)
}

#[cfg(test)]
#[path = "path_b_record_tests.rs"]
mod tests;
