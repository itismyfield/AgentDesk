//! #5188 (R5/R6): make a transcript binding that points at a DEAD file visible.
//!
//! ## Why this needs its own signal
//! When `/clear` rotated the Claude session and delivery stayed bound to the
//! frozen transcript, the relay ran for 31 minutes and exited reporting
//! `RelayMetricsSnapshot { frames_received: 0, frames_delivered: 0,
//! terminal_commits: 0, terminal_skips: 0, dropped_frames: 0, sink_errors: 0 }`.
//!
//! Every failure counter was zero. Nothing had failed — the relay simply never
//! looked at a file that was growing, so there was nothing to fail at. That
//! snapshot is **byte-identical to a healthy idle channel**, which is exactly why
//! the incident was silent: no `ERROR`, and the only clue was a `WARN` whose
//! wording read like success.
//!
//! So "zero frames" alone must never raise an alarm — it would fire on every idle
//! channel and be tuned out. It only becomes evidence when CROSSED with proof
//! that the pane should be producing output right now. This module is that
//! cross-check, and it deliberately returns [`TranscriptBindingStall::None`] for
//! the ambiguous cases rather than guessing.

/// How long the bound transcript may stand still before it counts as frozen.
///
/// Comfortably longer than a slow tool call so an ordinary long turn never trips
/// it, and far shorter than the ~31 minutes the observed incident stayed silent.
pub(in crate::services::discord) const BOUND_TRANSCRIPT_FROZEN_ALERT_SECS: u64 = 5 * 60;

/// Evidence for the cross-check, captured by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(in crate::services::discord) struct TranscriptBindingStallView {
    /// A relay is attached for this session.
    pub relay_attached: bool,
    /// The tmux pane is alive — the TUI still exists to produce output.
    pub tmux_alive: bool,
    /// Seconds since the transcript the binding points at last grew. `None` when
    /// no bound transcript could be resolved or stat'ed, which is a different
    /// (already-reported) problem and never this verdict.
    pub bound_transcript_idle_secs: Option<u64>,
    /// An inflight turn is waiting on that transcript. A turn in flight is a
    /// promise that bytes are owed.
    pub inflight_present: bool,
    /// A Claude session rotation for this pane is on record and not yet fully
    /// propagated (`tui_prompt_dedupe::claude_session_rotation_for_tmux`).
    ///
    /// SHORT-LIVED: the settle pass retires that record within a poll or two of
    /// the rotation, which is orders of magnitude sooner than the 5-minute
    /// freeze threshold below. It is therefore almost never the field that
    /// catches a real incident — see `binding_session_is_superseded`.
    pub rotation_pending: bool,
    /// Delivery is bound to a session id that is NOT the one the running Claude
    /// process last reported for this pane
    /// (`tui_prompt_dedupe::hook_adopted_claude_session_id`).
    ///
    /// This is the field that makes the verdict reachable. It is derived from
    /// the pane-lifetime adoption store, so unlike `rotation_pending` it is still
    /// true five minutes later — which is exactly when a frozen transcript
    /// becomes reportable. It is also positive proof rather than a guess: the
    /// process itself named a different session, so the file this binding points
    /// at cannot grow again.
    pub binding_session_is_superseded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum TranscriptBindingStall {
    /// Nothing to report, INCLUDING the genuinely ambiguous "quiet channel with a
    /// quiet transcript" — reporting that would be crying wolf on every idle pane.
    None,
    /// Confirmed #5188 shape: the pane is alive, the bound transcript has stopped
    /// growing, and a session rotation is on record — delivery is reading a file
    /// Claude has abandoned.
    FrozenAfterSessionRotation,
    /// The pane is alive with a turn in flight, but the transcript that turn is
    /// waiting on has not grown for minutes. Bytes are owed and nothing is
    /// arriving, whatever the cause.
    FrozenWithLiveTurn,
}

impl TranscriptBindingStall {
    pub(in crate::services::discord) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::FrozenAfterSessionRotation => "frozen_after_session_rotation",
            Self::FrozenWithLiveTurn => "frozen_with_live_turn",
        }
    }

    pub(in crate::services::discord) const fn is_alerting(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Classify a bound transcript that has stopped growing. Pure.
pub(in crate::services::discord) fn classify_transcript_binding_stall(
    view: TranscriptBindingStallView,
) -> TranscriptBindingStall {
    if !view.relay_attached || !view.tmux_alive {
        return TranscriptBindingStall::None;
    }
    let Some(idle_secs) = view.bound_transcript_idle_secs else {
        return TranscriptBindingStall::None;
    };
    if idle_secs < BOUND_TRANSCRIPT_FROZEN_ALERT_SECS {
        return TranscriptBindingStall::None;
    }
    // Positive proof that Claude moved on: the file this binding names cannot
    // ever grow again.
    //
    // Two independent witnesses, because the obvious one is nearly always gone
    // by the time this check can fire. `rotation_pending` lives for a poll or
    // two; `binding_session_is_superseded` lives as long as the pane does. The
    // residual shape that matters in production — rotation record already
    // retired, no inflight left, binding still naming the frozen transcript — is
    // reachable ONLY through the second witness, and it is the shape the #5188
    // incident actually sat in for 31 minutes.
    if view.rotation_pending || view.binding_session_is_superseded {
        return TranscriptBindingStall::FrozenAfterSessionRotation;
    }
    if view.inflight_present {
        return TranscriptBindingStall::FrozenWithLiveTurn;
    }
    // Alive pane, quiet transcript, nothing owed: an idle channel. This is the
    // case that makes a bare `frames_received == 0` alarm useless, so it stays
    // silent here on purpose.
    TranscriptBindingStall::None
}

/// #4408 phase-2 (I1): resolve the transcript path / provider session the relay
/// tail is bound to, surfaced on `watcher-state` so the out-of-band watchdog can
/// compare the server's asserted selector (B) against its own growth-aware
/// transcript pick (F).
///
/// Precedence is per field: a live inflight row's persisted `output_path` /
/// `session_id` win because they are the authoritative binding; when the inflight
/// row is absent — or leaves a field blank — we fall back to the in-memory tmux
/// runtime binding's `relay_output_path` / `session_id`. Both inputs come from
/// sync single-shot lookups that never straddle an await (so no
/// `await_holding_lock` allow is introduced), and the side-effecting
/// claude-session-id GET path is intentionally NOT consulted. A field is `None`
/// when neither source knows it, so serialization omits it and the watchdog fails
/// closed instead of alarming on an unknown bind.
pub(in crate::services::discord) fn resolve_bound_selector(
    inflight_output_path: Option<&str>,
    inflight_session_id: Option<&str>,
    binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> (Option<String>, Option<String>) {
    fn non_blank(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    let bound_output_path = non_blank(inflight_output_path)
        .or_else(|| non_blank(binding.map(|binding| binding.relay_output_path())));
    let bound_session_id = non_blank(inflight_session_id)
        .or_else(|| non_blank(binding.and_then(|binding| binding.session_id.as_deref())));
    (bound_output_path, bound_session_id)
}

/// #5188 (R5/R6): gather the evidence for [`classify_transcript_binding_stall`]
/// and emit the bounded incident when it alerts.
///
/// Lives here rather than inline in the snapshot builder because it is the same
/// concern as [`resolve_bound_selector`] above — what transcript is delivery
/// bound to, and is that transcript still alive — and because `snapshot.rs` sits
/// against the 1000-line giant-file threshold.
///
/// Costs one `stat` of a path the caller already resolved plus an in-memory
/// ledger lookup, so it is safe on the health-endpoint path.
pub(in crate::services::discord) fn resolve_transcript_binding_stall(
    provider_name: &str,
    tmux_session: Option<&str>,
    bound_output_path: Option<&str>,
    bound_session_id: Option<&str>,
    relay_attached: bool,
    tmux_session_alive: Option<bool>,
    inflight_present: bool,
) -> TranscriptBindingStall {
    let bound_transcript_idle_secs = bound_output_path
        .map(std::path::Path::new)
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.elapsed().ok())
        .map(|elapsed| elapsed.as_secs());
    // Compare what delivery is bound to against what the running Claude process
    // last told us it is writing. Both sides must be known: an unknown bound
    // session, or a pane that never reported an adoption, is not evidence of
    // anything and must fail closed to `None`.
    let hook_adopted_session_id =
        tmux_session.and_then(crate::services::tui_prompt_dedupe::hook_adopted_claude_session_id);
    let binding_session_is_superseded = match (
        bound_session_id.map(str::trim),
        hook_adopted_session_id.as_deref(),
    ) {
        (Some(bound), Some(adopted)) if !bound.is_empty() => bound != adopted,
        _ => false,
    };
    let stall = classify_transcript_binding_stall(TranscriptBindingStallView {
        relay_attached,
        // Unknown liveness must not alert: an unprobed pane is not evidence that
        // a live TUI is being starved.
        tmux_alive: tmux_session_alive == Some(true),
        bound_transcript_idle_secs,
        inflight_present,
        rotation_pending: tmux_session.is_some_and(|tmux| {
            crate::services::tui_prompt_dedupe::claude_session_rotation_for_tmux(tmux).is_some()
        }),
        binding_session_is_superseded,
    });
    if stall.is_alerting() {
        tracing::warn!(
            provider = provider_name,
            tmux_session = tmux_session.unwrap_or(""),
            bound_output_path = bound_output_path.unwrap_or(""),
            bound_session_id = bound_session_id.unwrap_or(""),
            hook_adopted_session_id = hook_adopted_session_id.as_deref().unwrap_or(""),
            binding_session_is_superseded,
            bound_transcript_idle_secs = bound_transcript_idle_secs.unwrap_or(0),
            transcript_binding_stall = stall.as_str(),
            "#5188: delivery binding points at a transcript that stopped growing while the \
             pane is alive and output is owed; relay frame counters cannot show this because \
             nothing is ever attempted against a file that does not grow"
        );
    }
    stall
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frozen() -> TranscriptBindingStallView {
        TranscriptBindingStallView {
            relay_attached: true,
            tmux_alive: true,
            bound_transcript_idle_secs: Some(BOUND_TRANSCRIPT_FROZEN_ALERT_SECS),
            inflight_present: false,
            rotation_pending: false,
            binding_session_is_superseded: false,
        }
    }

    /// The shape the #5188 incident actually sat in, and the one the first cut of
    /// this classifier returned `None` for.
    ///
    /// By the time a transcript has been frozen for five minutes the rotation
    /// record is long gone (the settle pass retires it within a poll or two) and
    /// the stranded inflight has been settled, so BOTH of the original witnesses
    /// are false. What remains true — and stays true for the life of the pane —
    /// is that delivery is bound to a session the running Claude process has
    /// moved off. Without that witness this verdict was reachable only through
    /// the narrow window where an unresolved owner channel kept the rotation
    /// record alive.
    #[test]
    fn a_binding_left_on_a_superseded_session_alerts_after_the_rotation_record_is_gone() {
        let residual = TranscriptBindingStallView {
            rotation_pending: false,
            inflight_present: false,
            binding_session_is_superseded: true,
            ..frozen()
        };
        assert_eq!(
            classify_transcript_binding_stall(residual),
            TranscriptBindingStall::FrozenAfterSessionRotation,
            "rotation record retired + no inflight + binding still on the frozen \
             transcript is the residual #5188 shape; returning None here is what \
             let it stay silent for 31 minutes"
        );
        assert!(classify_transcript_binding_stall(residual).is_alerting());
    }

    /// The new witness must not turn the classifier into the bare zero-frames
    /// alarm it was written to avoid: a superseded binding whose transcript is
    /// still growing is a rotation mid-flight, which is normal.
    #[test]
    fn a_superseded_binding_whose_transcript_still_grows_is_not_alerted_on() {
        assert_eq!(
            classify_transcript_binding_stall(TranscriptBindingStallView {
                bound_transcript_idle_secs: Some(BOUND_TRANSCRIPT_FROZEN_ALERT_SECS - 1),
                binding_session_is_superseded: true,
                ..frozen()
            }),
            TranscriptBindingStall::None,
            "a rotation that is still draining the old transcript is expected"
        );
        assert_eq!(
            classify_transcript_binding_stall(TranscriptBindingStallView {
                tmux_alive: false,
                binding_session_is_superseded: true,
                ..frozen()
            }),
            TranscriptBindingStall::None,
            "a dead pane owes nothing, whatever session it is bound to"
        );
    }

    #[test]
    fn an_idle_channel_is_never_alerted_on() {
        assert_eq!(
            classify_transcript_binding_stall(frozen()),
            TranscriptBindingStall::None,
            "a quiet transcript on a quiet channel is the normal idle shape; \
             alerting here is what made a bare zero-frames signal worthless"
        );
    }

    #[test]
    fn a_pending_rotation_makes_a_frozen_binding_a_confirmed_fault() {
        let stall = classify_transcript_binding_stall(TranscriptBindingStallView {
            rotation_pending: true,
            ..frozen()
        });
        assert_eq!(stall, TranscriptBindingStall::FrozenAfterSessionRotation);
        assert!(stall.is_alerting());
    }

    #[test]
    fn a_live_turn_waiting_on_a_frozen_transcript_alerts() {
        assert_eq!(
            classify_transcript_binding_stall(TranscriptBindingStallView {
                inflight_present: true,
                ..frozen()
            }),
            TranscriptBindingStall::FrozenWithLiveTurn,
            "bytes are owed to a turn in flight and none are arriving"
        );
    }

    #[test]
    fn a_transcript_that_grew_recently_is_healthy_even_mid_rotation() {
        assert_eq!(
            classify_transcript_binding_stall(TranscriptBindingStallView {
                bound_transcript_idle_secs: Some(BOUND_TRANSCRIPT_FROZEN_ALERT_SECS - 1),
                rotation_pending: true,
                inflight_present: true,
                ..frozen()
            }),
            TranscriptBindingStall::None,
            "a rotation mid-drain is expected and must not alert while the old \
             transcript is still being consumed"
        );
    }

    #[test]
    fn a_dead_pane_or_detached_relay_is_somebody_elses_verdict() {
        for view in [
            TranscriptBindingStallView {
                tmux_alive: false,
                rotation_pending: true,
                inflight_present: true,
                ..frozen()
            },
            TranscriptBindingStallView {
                relay_attached: false,
                rotation_pending: true,
                inflight_present: true,
                ..frozen()
            },
        ] {
            assert_eq!(
                classify_transcript_binding_stall(view),
                TranscriptBindingStall::None,
                "a dead pane / detached relay already has its own dedicated \
                 stall states; this verdict is only about a LIVE pane whose \
                 delivery is pointed at a dead file"
            );
        }
    }

    #[test]
    fn an_unresolvable_bound_transcript_is_not_this_verdict() {
        assert_eq!(
            classify_transcript_binding_stall(TranscriptBindingStallView {
                bound_transcript_idle_secs: None,
                rotation_pending: true,
                inflight_present: true,
                ..frozen()
            }),
            TranscriptBindingStall::None,
        );
    }
}
