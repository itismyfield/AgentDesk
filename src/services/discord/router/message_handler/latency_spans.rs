//! #3813 Phase 1a — intake-side latency spans (observation-only).
//!
//! Measures the coarse segments of the Discord intake path — turn claimed →
//! placeholder posted → prompt prep done → provider input written — using
//! monotonic [`std::time::Instant`] deltas.
//!
//! This module is PURELY OBSERVATIONAL (design §Phase 1a invariants):
//!   * durations use monotonic `Instant` only — never wall-clock `SystemTime`,
//!     so measurements are immune to NTP steps / clock skew;
//!   * no `await`, no lock, no allocation on the capture path (`mark_*` is a
//!     single `Instant::now()` store) — measuring must never itself add latency;
//!   * it never reads or influences turn / relay control flow — it only records
//!     timestamps and, once per turn, emits one structured `[latency]` log line
//!     plus one observability event;
//!   * missing marks (early return / cancel / error before a milestone) render
//!     as `-`, so a partial span stays legible.
//!
//! These spans intentionally measure FINER sub-intervals of the intake work the
//! existing `[prompt-prep]` `duration_ms` log already times as one number: that
//! log covers the whole prep window (`intake_turn.rs:1152 → :1865`), whereas the
//! `claim` / `placeholder` / `prep` marks captured here land INSIDE that window,
//! so the two instruments OVERLAP on the time axis BY DESIGN. Read them
//! independently — do NOT add `[prompt-prep] duration_ms` and these segments
//! together (double counting). Only the trailing `prep → input` segment reaches
//! past the prep window, out to the provider-input handoff.

use std::time::Instant;

/// Monotonic intake latency waypoints for a single turn.
///
/// `accepted_at` is captured at construction (the mailbox claim); the remaining
/// marks are filled in as the intake path reaches each milestone. Every field
/// is `Copy`, so the whole struct threads through the intake function by value
/// with zero heap cost.
#[derive(Debug, Clone, Copy)]
pub(super) struct IntakeLatencySpans {
    /// Turn claimed (this message won the mailbox claim). Always present.
    accepted_at: Instant,
    /// Intake placeholder POST returned a live message id.
    placeholder_at: Option<Instant>,
    /// Prompt prep complete (paired with the existing `[prompt-prep]` point).
    prep_done_at: Option<Instant>,
    /// Provider input handed off to the turn bridge (`spawn_turn_bridge`).
    input_written_at: Option<Instant>,
}

impl IntakeLatencySpans {
    /// Capture the turn-claimed anchor. Call once, right after the mailbox
    /// claim is confirmed (`started == true`).
    #[must_use]
    pub(super) fn turn_claimed() -> Self {
        Self {
            accepted_at: Instant::now(),
            placeholder_at: None,
            prep_done_at: None,
            input_written_at: None,
        }
    }

    /// Mark the moment the intake placeholder POST returned a live id.
    pub(super) fn mark_placeholder_posted(&mut self) {
        self.placeholder_at = Some(Instant::now());
    }

    /// Mark prompt-prep completion (the existing `:1859` measurement point).
    pub(super) fn mark_prep_done(&mut self) {
        self.prep_done_at = Some(Instant::now());
    }

    /// Mark the provider-input handoff (right before `spawn_turn_bridge`).
    pub(super) fn mark_input_written(&mut self) {
        self.input_written_at = Some(Instant::now());
    }

    /// Segment: accept → placeholder (ms). `None` until the placeholder mark.
    fn accept_to_placeholder_ms(&self) -> Option<u128> {
        self.placeholder_at
            .map(|end| end.saturating_duration_since(self.accepted_at).as_millis())
    }

    /// Segment: placeholder → prep (ms). Needs both marks.
    fn placeholder_to_prep_ms(&self) -> Option<u128> {
        match (self.placeholder_at, self.prep_done_at) {
            (Some(start), Some(end)) => Some(end.saturating_duration_since(start).as_millis()),
            _ => None,
        }
    }

    /// Segment: prep → input (ms). Needs both marks.
    fn prep_to_input_ms(&self) -> Option<u128> {
        match (self.prep_done_at, self.input_written_at) {
            (Some(start), Some(end)) => Some(end.saturating_duration_since(start).as_millis()),
            _ => None,
        }
    }

    /// Total: accept → input (ms). `None` until the input mark.
    fn accept_to_input_ms(&self) -> Option<u128> {
        self.input_written_at
            .map(|end| end.saturating_duration_since(self.accepted_at).as_millis())
    }

    /// Emit the single structured `[latency]` line + one observability event.
    ///
    /// Observation-only: pure formatting + one `tracing::info!` + one
    /// non-blocking observability push (same family the intake path already
    /// calls). No `await`, no lock. `outcome` distinguishes a completed handoff
    /// (`"submitted"`) from a pre-submission defer (`"deferred_busy"`) so a
    /// partial span is never confused with a full one.
    pub(super) fn log(&self, channel_id: u64, provider_label: &str, outcome: &str) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] [latency] channel={channel_id} provider={provider_label} outcome={outcome} {}",
            format_spans(
                self.accept_to_placeholder_ms(),
                self.placeholder_to_prep_ms(),
                self.prep_to_input_ms(),
                self.accept_to_input_ms(),
            )
        );
        crate::services::observability::emit_intake_latency_spans(
            provider_label,
            channel_id,
            outcome,
            self.accept_to_placeholder_ms().map(saturating_u64),
            self.placeholder_to_prep_ms().map(saturating_u64),
            self.prep_to_input_ms().map(saturating_u64),
            self.accept_to_input_ms().map(saturating_u64),
        );
    }
}

/// Truncate a `u128` millisecond count into the `u64` observability schema,
/// saturating on the (practically impossible) overflow rather than wrapping.
fn saturating_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Render the `accept→placeholder=… →prep=… →input=… total=…` body. Pure and
/// side-effect-free; missing segments render as `-`.
fn format_spans(
    accept_to_placeholder_ms: Option<u128>,
    placeholder_to_prep_ms: Option<u128>,
    prep_to_input_ms: Option<u128>,
    accept_to_input_ms: Option<u128>,
) -> String {
    fn ms(value: Option<u128>) -> String {
        value.map_or_else(|| "-".to_string(), |value| value.to_string())
    }
    format!(
        "accept→placeholder={}ms →prep={}ms →input={}ms total={}ms",
        ms(accept_to_placeholder_ms),
        ms(placeholder_to_prep_ms),
        ms(prep_to_input_ms),
        ms(accept_to_input_ms),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_span_has_only_accept_anchor() {
        // A just-claimed turn that never reaches a later milestone (e.g. an
        // early return before the placeholder POST) must expose every derived
        // segment as `None` — a partial span, not a bogus zero.
        let spans = IntakeLatencySpans::turn_claimed();
        assert_eq!(spans.accept_to_placeholder_ms(), None);
        assert_eq!(spans.placeholder_to_prep_ms(), None);
        assert_eq!(spans.prep_to_input_ms(), None);
        assert_eq!(spans.accept_to_input_ms(), None);
    }

    #[test]
    fn marks_unlock_segments_in_order() {
        let mut spans = IntakeLatencySpans::turn_claimed();

        spans.mark_placeholder_posted();
        assert!(spans.accept_to_placeholder_ms().is_some());
        // prep/input segments still need their downstream marks.
        assert_eq!(spans.placeholder_to_prep_ms(), None);
        assert_eq!(spans.prep_to_input_ms(), None);
        assert_eq!(spans.accept_to_input_ms(), None);

        spans.mark_prep_done();
        assert!(spans.placeholder_to_prep_ms().is_some());
        assert_eq!(spans.prep_to_input_ms(), None);

        spans.mark_input_written();
        assert!(spans.prep_to_input_ms().is_some());
        assert!(spans.accept_to_input_ms().is_some());
    }

    #[test]
    fn full_span_renders_every_segment() {
        let line = format_spans(Some(12), Some(34), Some(56), Some(102));
        assert_eq!(
            line,
            "accept→placeholder=12ms →prep=34ms →input=56ms total=102ms"
        );
    }

    #[test]
    fn partial_span_renders_dashes_for_missing_marks() {
        // prep + input never happened → only the first segment is known.
        let line = format_spans(Some(7), None, None, None);
        assert_eq!(
            line,
            "accept→placeholder=7ms →prep=-ms →input=-ms total=-ms"
        );
    }

    #[test]
    fn empty_span_renders_all_dashes() {
        let line = format_spans(None, None, None, None);
        assert_eq!(
            line,
            "accept→placeholder=-ms →prep=-ms →input=-ms total=-ms"
        );
    }

    #[test]
    fn saturating_u64_clamps_overflow() {
        assert_eq!(saturating_u64(0), 0);
        assert_eq!(saturating_u64(1_234), 1_234);
        assert_eq!(saturating_u64(u128::from(u64::MAX)), u64::MAX);
        assert_eq!(saturating_u64(u128::from(u64::MAX) + 1), u64::MAX);
    }
}
