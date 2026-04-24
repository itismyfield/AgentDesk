//! Meeting state machine (issue #1008 first slice).
//!
//! Pure, side-effect-free state + transition reducer that the meeting
//! orchestrator can opt into. The existing [`super::meeting_orchestrator`]
//! continues to own the live [`super::meeting_orchestrator::MeetingStatus`]
//! field; this module provides an additive, test-friendly representation that
//! can be dropped into the orchestrator as `start_meeting`/`cancel_meeting`
//! migrate incrementally.
//!
//! The intent is to isolate the lifecycle so that:
//! * Invalid transitions (e.g. double-Start, Cancel-after-Complete) are
//!   statically rejected rather than coerced.
//! * The Discord `/meeting` command and the `/api/meetings/*` route collapse
//!   onto one reducer and one artifact repository.
//! * Artifact writes are gated behind idempotency keys (see
//!   [`super::meeting_artifact_store`]).
//!
//! The semantics intentionally mirror the current orchestrator so we can
//! migrate call sites without behaviour change:
//!
//! ```text
//!          Start            RoundComplete*            Summarize
//! Pending ───────► Starting ──────────────► Running ─────────────► Summarizing
//!    │                │                        │                       │
//!    │ Cancel         │ Cancel / ProviderFail  │ Cancel / ProviderFail │ MarkComplete / ProviderFail
//!    ▼                ▼                        ▼                       ▼
//! Cancelled      Cancelled / Failed      Cancelled / Failed    Completed / Failed
//! ```

use std::fmt;

/// Meeting lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MeetingState {
    /// Meeting row exists (e.g. slot claimed) but no orchestration has started.
    Pending,
    /// Orchestrator is selecting participants / establishing the thread.
    Starting,
    /// Participants selected; round turns are actively being dispatched.
    Running,
    /// Final round closed, summary agent is writing the summary.
    Summarizing,
    /// Terminal success.
    Completed,
    /// Terminal cancel (user-initiated or race-loser).
    Cancelled,
    /// Terminal failure (provider outage, unrecoverable error).
    Failed,
}

impl MeetingState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            MeetingState::Completed | MeetingState::Cancelled | MeetingState::Failed
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MeetingState::Pending => "pending",
            MeetingState::Starting => "starting",
            MeetingState::Running => "running",
            MeetingState::Summarizing => "summarizing",
            MeetingState::Completed => "completed",
            MeetingState::Cancelled => "cancelled",
            MeetingState::Failed => "failed",
        }
    }
}

impl fmt::Display for MeetingState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Events that drive state transitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MeetingEvent {
    /// Orchestration starting: participant selection + thread creation kicked off.
    Start,
    /// One round of utterances completed; state stays `Running`.
    RoundComplete,
    /// Final round closed; summary generation starting.
    Summarize,
    /// User / API requested cancellation.
    Cancel,
    /// Provider / dispatch failure — terminal.
    ProviderFailed,
    /// Summary written successfully — terminal.
    MarkComplete,
}

/// Error returned when an event cannot drive the current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: MeetingState,
    pub event: MeetingEvent,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid meeting transition: {:?} cannot apply event {:?}",
            self.from, self.event
        )
    }
}

impl std::error::Error for InvalidTransition {}

/// Pure reducer. The only entry point that mutates meeting state.
///
/// The function is deliberately side-effect-free — callers persist the result
/// into their storage (orchestrator slot, DB row, artifact repo) themselves.
pub fn transition(
    state: MeetingState,
    event: MeetingEvent,
) -> Result<MeetingState, InvalidTransition> {
    use MeetingEvent::*;
    use MeetingState::*;

    let next = match (state, event) {
        // Start path
        (Pending, Start) => Starting,
        (Starting, RoundComplete) => Running,
        (Running, RoundComplete) => Running,
        (Running, Summarize) => Summarizing,
        (Starting, Summarize) => Summarizing, // degenerate: no rounds but summary requested
        (Summarizing, MarkComplete) => Completed,

        // Cancel path — allowed from any non-terminal state
        (Pending, Cancel) => Cancelled,
        (Starting, Cancel) => Cancelled,
        (Running, Cancel) => Cancelled,
        (Summarizing, Cancel) => Cancelled,

        // Failure path — allowed from any non-terminal state
        (Pending, ProviderFailed) => Failed,
        (Starting, ProviderFailed) => Failed,
        (Running, ProviderFailed) => Failed,
        (Summarizing, ProviderFailed) => Failed,

        // Everything else is invalid (including terminal-state mutations
        // and nonsensical events like MarkComplete from Pending).
        _ => return Err(InvalidTransition { from: state, event }),
    };

    Ok(next)
}

/// Convenience: reducer that treats idempotent re-delivery of a terminal
/// transition as a no-op. Useful for cancel-race scenarios where two concurrent
/// cancels arrive — the second observes the terminal state and short-circuits
/// without returning an error.
pub fn transition_idempotent_terminal(
    state: MeetingState,
    event: MeetingEvent,
) -> Result<MeetingState, InvalidTransition> {
    match (state, event) {
        (MeetingState::Cancelled, MeetingEvent::Cancel) => Ok(MeetingState::Cancelled),
        (MeetingState::Completed, MeetingEvent::MarkComplete) => Ok(MeetingState::Completed),
        (MeetingState::Failed, MeetingEvent::ProviderFailed) => Ok(MeetingState::Failed),
        _ => transition(state, event),
    }
}

#[cfg(test)]
mod tests {
    use super::MeetingEvent::*;
    use super::MeetingState::*;
    use super::*;

    #[test]
    fn happy_path_pending_to_completed() {
        let s = transition(Pending, Start).unwrap();
        assert_eq!(s, Starting);
        let s = transition(s, RoundComplete).unwrap();
        assert_eq!(s, Running);
        let s = transition(s, RoundComplete).unwrap();
        assert_eq!(s, Running);
        let s = transition(s, Summarize).unwrap();
        assert_eq!(s, Summarizing);
        let s = transition(s, MarkComplete).unwrap();
        assert_eq!(s, Completed);
        assert!(s.is_terminal());
    }

    #[test]
    fn cancel_allowed_from_every_nonterminal_state() {
        for state in [Pending, Starting, Running, Summarizing] {
            assert_eq!(
                transition(state, Cancel).unwrap(),
                Cancelled,
                "Cancel from {:?} should land in Cancelled",
                state
            );
        }
    }

    #[test]
    fn provider_failure_allowed_from_every_nonterminal_state() {
        for state in [Pending, Starting, Running, Summarizing] {
            assert_eq!(
                transition(state, ProviderFailed).unwrap(),
                Failed,
                "ProviderFailed from {:?} should land in Failed",
                state
            );
        }
    }

    #[test]
    fn terminal_states_reject_all_events() {
        for state in [Completed, Cancelled, Failed] {
            for event in [
                Start,
                RoundComplete,
                Summarize,
                Cancel,
                ProviderFailed,
                MarkComplete,
            ] {
                assert!(
                    transition(state, event).is_err(),
                    "terminal state {:?} must reject event {:?}",
                    state,
                    event
                );
            }
        }
    }

    #[test]
    fn invalid_start_after_start() {
        let s = transition(Pending, Start).unwrap();
        // Can't double-Start.
        assert_eq!(
            transition(s, Start),
            Err(InvalidTransition {
                from: Starting,
                event: Start
            })
        );
    }

    #[test]
    fn invalid_mark_complete_without_summarize() {
        // MarkComplete is only valid from Summarizing.
        for state in [Pending, Starting, Running] {
            assert!(
                transition(state, MarkComplete).is_err(),
                "MarkComplete should not be valid from {:?}",
                state
            );
        }
    }

    #[test]
    fn invalid_round_complete_from_pending_or_summarizing() {
        assert!(transition(Pending, RoundComplete).is_err());
        assert!(transition(Summarizing, RoundComplete).is_err());
    }

    #[test]
    fn invalid_summarize_from_pending() {
        assert!(transition(Pending, Summarize).is_err());
    }

    #[test]
    fn idempotent_cancel_is_noop_on_cancelled() {
        assert_eq!(
            transition_idempotent_terminal(Cancelled, Cancel).unwrap(),
            Cancelled
        );
    }

    #[test]
    fn idempotent_helper_still_rejects_wrong_terminal_event() {
        // Cancelled + Start must still fail even under the idempotent helper.
        assert!(transition_idempotent_terminal(Cancelled, Start).is_err());
        // Completed + Cancel is still invalid (can't cancel a completed meeting).
        assert!(transition_idempotent_terminal(Completed, Cancel).is_err());
    }

    #[test]
    fn display_renders_lowercase_slug() {
        assert_eq!(Pending.to_string(), "pending");
        assert_eq!(Completed.to_string(), "completed");
        assert_eq!(Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn invalid_transition_error_is_informative() {
        let err = transition(Completed, Start).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Completed"));
        assert!(msg.contains("Start"));
    }
}
