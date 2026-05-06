use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeadSessionCleanupPlan {
    pub(super) preserve_tmux_session: bool,
    pub(super) report_idle_status: bool,
}

pub(super) fn dead_session_cleanup_plan(dispatch_protected: bool) -> DeadSessionCleanupPlan {
    DeadSessionCleanupPlan {
        preserve_tmux_session: dispatch_protected,
        report_idle_status: true,
    }
}

/// Default idle window the post-terminal-success watcher uses to classify
/// continuation state while tmux is still alive and confirmed_end has caught up
/// to the tail offset.
/// See issue #1137: codex agents (G2/G3/G4) were observed emitting additional
/// output for several seconds AFTER the terminal-success log. Issue #1171
/// makes tmux liveness, not post-result idleness, the normal watcher shutdown
/// authority.
pub(crate) const WATCHER_POST_TERMINAL_IDLE_WINDOW: Duration = Duration::from_secs(5);

/// Input snapshot for [`watcher_stop_decision_after_terminal_success`].
/// Kept as a plain copyable struct so the helper is trivially unit-testable
/// without mocking tokio time or tmux. See issue #1137 for the watcher
/// strictness contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatcherStopInput {
    /// True once the watcher has relayed a terminal-success result to Discord
    /// for the current dispatch (i.e. `turn_result_relayed`).
    pub(crate) terminal_success_seen: bool,
    /// Tmux pane liveness — `crate::services::platform::tmux::has_session`
    /// (or the watcher's wrapper `tmux_session_has_live_pane`).
    pub(crate) tmux_alive: bool,
    /// Shared `confirmed_end_offset` watermark across all watcher replicas
    /// for this channel.
    pub(crate) confirmed_end: u64,
    /// Current tmux jsonl tail offset (`std::fs::metadata(output).len()`).
    pub(crate) tmux_tail_offset: u64,
    /// Time since the last new-output observation. `None` means we have not
    /// observed any output yet during this watcher iteration.
    pub(crate) idle_duration: Option<Duration>,
    /// Idle window used to classify post-terminal-success continuation state.
    pub(crate) idle_threshold: Duration,
}

/// Outcome of the watcher-stop strictness check (#1137).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherStopDecision {
    /// Watcher should keep running. Either the dispatch hasn't reached
    /// terminal-success yet, or new output is still arriving / the
    /// confirmed-end watermark hasn't caught up.
    Continue,
    /// Terminal success was relayed but additional tmux output is still
    /// being produced (or the idle window hasn't elapsed). The caller should
    /// log "post-terminal-success continuation" exactly once when this
    /// transitions in, then keep the watcher alive.
    PostTerminalSuccessContinuation,
    /// Watcher may stop quietly because the tmux pane died. Normal completion
    /// must route through tmux death detection rather than post-result idleness.
    Stop,
}

/// Decide whether the tmux output watcher may stop after a terminal-success
/// event. Issue #1137 widened the legacy "exit on terminal success" rule, and
/// issue #1171 makes tmux liveness the only normal watcher-stop authority:
///
/// - dead tmux pane                                      -> Stop
/// - terminal success seen + tmux still alive + (tail
///   has advanced past confirmed_end OR idle window has
///   not elapsed yet)                                    -> PostTerminalSuccessContinuation
/// - otherwise, including terminal success with an alive
///   idle tmux pane                                      -> Continue
pub(crate) fn watcher_stop_decision_after_terminal_success(
    input: WatcherStopInput,
) -> WatcherStopDecision {
    if !input.tmux_alive {
        return WatcherStopDecision::Stop;
    }

    if !input.terminal_success_seen {
        return WatcherStopDecision::Continue;
    }

    let confirmed_caught_up = input.confirmed_end >= input.tmux_tail_offset;
    if !confirmed_caught_up {
        return WatcherStopDecision::PostTerminalSuccessContinuation;
    }

    match input.idle_duration {
        Some(idle) if idle >= input.idle_threshold => WatcherStopDecision::Continue,
        _ => WatcherStopDecision::PostTerminalSuccessContinuation,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TmuxLivenessDecision {
    Continue,
    QuietStop,
    TmuxDied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatcherOutputPollDecision {
    DrainOutput,
    Continue,
    QuietStop,
    TmuxDied,
}

pub(super) fn tmux_liveness_decision(
    cancelled: bool,
    shutting_down: bool,
    tmux_alive: bool,
) -> TmuxLivenessDecision {
    if cancelled || shutting_down {
        TmuxLivenessDecision::QuietStop
    } else if tmux_alive {
        TmuxLivenessDecision::Continue
    } else {
        TmuxLivenessDecision::TmuxDied
    }
}

pub(super) fn watcher_output_poll_decision(
    bytes_read: usize,
    liveness_after_empty_read: Option<TmuxLivenessDecision>,
) -> WatcherOutputPollDecision {
    if bytes_read > 0 {
        return WatcherOutputPollDecision::DrainOutput;
    }

    match liveness_after_empty_read.expect("empty watcher read must probe tmux liveness") {
        TmuxLivenessDecision::Continue => WatcherOutputPollDecision::Continue,
        TmuxLivenessDecision::QuietStop => WatcherOutputPollDecision::QuietStop,
        TmuxLivenessDecision::TmuxDied => WatcherOutputPollDecision::TmuxDied,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MissingInflightFallbackPlan {
    pub(super) warn: bool,
    pub(super) mark_degraded: bool,
    pub(super) suppressed_by_recent_stop: bool,
}

pub(super) fn missing_inflight_fallback_observation(
    inflight_missing: bool,
    dispatch_resolved: bool,
    terminal_output_committed: bool,
    recent_turn_stop: bool,
    _placeholder_cleanup_committed: bool,
    tmux_alive: bool,
) -> MissingInflightFallbackPlan {
    let unresolved_live_tmux =
        inflight_missing && !dispatch_resolved && terminal_output_committed && tmux_alive;
    let suppressed = unresolved_live_tmux && recent_turn_stop;
    MissingInflightFallbackPlan {
        warn: inflight_missing,
        mark_degraded: inflight_missing && !dispatch_resolved && !suppressed,
        suppressed_by_recent_stop: suppressed,
    }
}

pub(super) fn should_flush_post_terminal_success_continuation(
    terminal_success_seen: bool,
    found_result: bool,
    full_response: &str,
) -> bool {
    terminal_success_seen && !found_result && !full_response.trim().is_empty()
}

pub(super) fn should_resume_watcher_after_turn(
    defer_watcher_resume: bool,
    has_local_queued_turns: bool,
    can_chain_locally: bool,
) -> bool {
    !defer_watcher_resume && !(has_local_queued_turns && can_chain_locally)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_input() -> WatcherStopInput {
        WatcherStopInput {
            terminal_success_seen: true,
            tmux_alive: true,
            confirmed_end: 100,
            tmux_tail_offset: 100,
            idle_duration: Some(WATCHER_POST_TERMINAL_IDLE_WINDOW),
            idle_threshold: WATCHER_POST_TERMINAL_IDLE_WINDOW,
        }
    }

    #[test]
    fn normal_completion_waits_for_tmux_death_authority() {
        assert_eq!(
            watcher_stop_decision_after_terminal_success(stop_input()),
            WatcherStopDecision::Continue
        );
        assert_eq!(
            watcher_stop_decision_after_terminal_success(WatcherStopInput {
                tmux_alive: false,
                ..stop_input()
            }),
            WatcherStopDecision::Stop
        );
    }

    #[test]
    fn active_stream_keeps_watcher_running_before_terminal_success() {
        assert_eq!(
            watcher_stop_decision_after_terminal_success(WatcherStopInput {
                terminal_success_seen: false,
                idle_duration: None,
                ..stop_input()
            }),
            WatcherStopDecision::Continue
        );
    }

    #[test]
    fn stream_disconnect_with_dead_tmux_becomes_stop_signal() {
        let liveness = tmux_liveness_decision(false, false, false);
        assert_eq!(liveness, TmuxLivenessDecision::TmuxDied);
        assert_eq!(
            watcher_output_poll_decision(0, Some(liveness)),
            WatcherOutputPollDecision::TmuxDied
        );
    }

    #[test]
    fn missing_inflight_committed_output_marks_live_tmux_degraded() {
        let plan = missing_inflight_fallback_observation(true, false, true, false, false, true);
        assert!(plan.warn);
        assert!(plan.mark_degraded);
    }

    #[test]
    fn missing_inflight_without_resolved_dispatch_marks_degraded() {
        let plan = missing_inflight_fallback_observation(true, false, false, false, false, true);
        assert!(plan.warn);
        assert!(plan.mark_degraded);
    }

    #[test]
    fn explicit_background_work_defers_watcher_resume() {
        assert!(!should_resume_watcher_after_turn(true, false, false));
        assert!(!should_resume_watcher_after_turn(false, true, true));
        assert!(should_resume_watcher_after_turn(false, true, false));
    }
}
