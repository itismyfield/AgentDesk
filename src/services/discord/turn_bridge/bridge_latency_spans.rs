//! #3813 AC#1 tail — bridge-side latency spans (observation-only).
//!
//! Phase 1a (`router/message_handler/latency_spans.rs`) measures the intake
//! path up to the provider-input handoff (accept → placeholder → prep → input).
//! This module closes the trailing half of acceptance criterion #1 — the two
//! bridge-side waypoints the intake spans cannot see:
//!   * `turn_start → first_output`: the first non-empty un-relayed assistant
//!     portion the bridge streaming loop observes, and
//!   * `turn_start → first_relay`: the first successful Discord delivery
//!     (streaming edit or rollover send) the bridge commits.
//!
//! It mirrors [`super::super::router::message_handler`]'s intake spans exactly
//! and is PURELY OBSERVATIONAL:
//!   * durations use monotonic [`std::time::Instant`] only — never wall-clock —
//!     so measurements are immune to NTP steps / clock skew;
//!   * `mark_*` is a single idempotent `Instant::now()` store — no `await`, no
//!     lock, no allocation on the capture path (measuring must never itself add
//!     latency), and it never reads or influences relay control flow;
//!   * the anchor reuses the bridge-entry `turn_start` `Instant` the loop
//!     already owns, so no new signature / `Context` field is introduced;
//!   * a missing waypoint (early cancel / stream error before the milestone)
//!     renders as `-`, so a partial span stays legible.
//!
//! SCOPE (honest boundary): only the BRIDGE-OWNED relay path is covered. When a
//! tmux watcher or standby sink owns the assistant relay, delivery happens
//! outside this loop, so both waypoints stay unset and the span is suppressed
//! entirely (see [`BridgeLatencySpans::log`]). Watcher-owned relay latency is
//! `tmux_watcher.rs`'s (a separate hotfile) concern and is out of scope here.

use std::time::Instant;

/// Monotonic bridge-side latency waypoints for a single turn.
///
/// `turn_start_at` is the bridge-entry anchor (`spawn_turn_bridge`'s existing
/// `turn_start`); the remaining marks are filled in as the streaming loop
/// reaches each milestone. Every field is `Copy`, so the struct threads through
/// the loop by value with zero heap cost.
#[derive(Debug, Clone, Copy)]
pub(super) struct BridgeLatencySpans {
    /// Bridge entry (reuses the loop's `turn_start`). Always present.
    turn_start_at: Instant,
    /// First non-empty un-relayed assistant portion observed by the bridge.
    first_output_at: Option<Instant>,
    /// First successful bridge-owned Discord relay (streaming edit / rollover).
    first_relay_at: Option<Instant>,
}

impl BridgeLatencySpans {
    /// Anchor the spans on the bridge-entry `turn_start` instant. Call once,
    /// reusing the loop's existing monotonic anchor (no new field required).
    #[must_use]
    pub(super) fn starting_at(turn_start_at: Instant) -> Self {
        Self {
            turn_start_at,
            first_output_at: None,
            first_relay_at: None,
        }
    }

    /// Mark the first observed non-empty assistant output. Idempotent: only the
    /// FIRST observation with `observed == true` records a timestamp; later
    /// passes (and empty / tool-only passes) are no-ops.
    pub(super) fn mark_first_output(&mut self, observed: bool) {
        if observed && self.first_output_at.is_none() {
            self.first_output_at = Some(Instant::now());
        }
    }

    /// Mark the first successful bridge-owned Discord relay. Idempotent: only the
    /// FIRST delivery records a timestamp; later relays are no-ops.
    pub(super) fn mark_first_relay(&mut self, delivered: bool) {
        if delivered && self.first_relay_at.is_none() {
            self.first_relay_at = Some(Instant::now());
        }
    }

    /// Segment: turn start → first output (ms). `None` until the output mark.
    fn start_to_first_output_ms(&self) -> Option<u128> {
        self.first_output_at.map(|end| {
            end.saturating_duration_since(self.turn_start_at)
                .as_millis()
        })
    }

    /// Segment: turn start → first relay (ms). `None` until the relay mark.
    fn start_to_first_relay_ms(&self) -> Option<u128> {
        self.first_relay_at.map(|end| {
            end.saturating_duration_since(self.turn_start_at)
                .as_millis()
        })
    }

    /// Emit the single structured `[latency-bridge]` line + one observability
    /// event, once per turn at loop exit.
    ///
    /// Observation-only: pure formatting + one `tracing::info!` + one
    /// non-blocking observability push (same family the intake spans use). No
    /// `await`, no lock. A turn that never relayed anything on the bridge path
    /// (both waypoints unset — e.g. a watcher-owned relay or an empty cancel)
    /// emits NOTHING, so the log is not flooded with all-dash lines.
    pub(super) fn log(&self, channel_id: u64, provider_label: &str) {
        let first_output_ms = self.start_to_first_output_ms();
        let first_relay_ms = self.start_to_first_relay_ms();
        if first_output_ms.is_none() && first_relay_ms.is_none() {
            return;
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] [latency-bridge] channel={channel_id} provider={provider_label} {}",
            format_bridge_spans(first_output_ms, first_relay_ms)
        );
        crate::services::observability::emit_bridge_latency_spans(
            provider_label,
            channel_id,
            first_output_ms.map(saturating_u64),
            first_relay_ms.map(saturating_u64),
        );
    }
}

/// Truncate a `u128` millisecond count into the `u64` observability schema,
/// saturating on the (practically impossible) overflow rather than wrapping.
fn saturating_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Render the `turn_start→first_output=… →first_relay=…` body. Pure and
/// side-effect-free; a missing segment renders as `-`.
fn format_bridge_spans(
    start_to_first_output_ms: Option<u128>,
    start_to_first_relay_ms: Option<u128>,
) -> String {
    fn ms(value: Option<u128>) -> String {
        value.map_or_else(|| "-".to_string(), |value| value.to_string())
    }
    format!(
        "turn_start→first_output={}ms →first_relay={}ms",
        ms(start_to_first_output_ms),
        ms(start_to_first_relay_ms),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_span_has_only_the_anchor() {
        // A turn that exits before relaying anything (early cancel / stream
        // error) must expose every derived segment as `None` — a partial span,
        // never a bogus zero.
        let spans = BridgeLatencySpans::starting_at(Instant::now());
        assert_eq!(spans.start_to_first_output_ms(), None);
        assert_eq!(spans.start_to_first_relay_ms(), None);
    }

    #[test]
    fn marks_unlock_segments_and_are_idempotent() {
        let mut spans = BridgeLatencySpans::starting_at(Instant::now());

        // Empty / tool-only passes never record output.
        spans.mark_first_output(false);
        assert_eq!(spans.start_to_first_output_ms(), None);

        spans.mark_first_output(true);
        let first = spans.first_output_at;
        assert!(spans.start_to_first_output_ms().is_some());
        // A second observation must NOT move the first-output anchor.
        spans.mark_first_output(true);
        assert_eq!(spans.first_output_at, first);

        // Relay waypoint is independent and equally idempotent.
        assert_eq!(spans.start_to_first_relay_ms(), None);
        spans.mark_first_relay(false);
        assert_eq!(spans.start_to_first_relay_ms(), None);
        spans.mark_first_relay(true);
        let relay = spans.first_relay_at;
        assert!(spans.start_to_first_relay_ms().is_some());
        spans.mark_first_relay(true);
        assert_eq!(spans.first_relay_at, relay);
    }

    #[test]
    fn full_span_renders_every_segment() {
        let line = format_bridge_spans(Some(120), Some(240));
        assert_eq!(line, "turn_start→first_output=120ms →first_relay=240ms");
    }

    #[test]
    fn partial_span_renders_dash_for_missing_relay() {
        // Output observed but delivery never committed (e.g. the loop exited
        // right after the first non-empty portion) → only the first segment.
        let line = format_bridge_spans(Some(75), None);
        assert_eq!(line, "turn_start→first_output=75ms →first_relay=-ms");
    }

    #[test]
    fn empty_span_renders_all_dashes() {
        let line = format_bridge_spans(None, None);
        assert_eq!(line, "turn_start→first_output=-ms →first_relay=-ms");
    }

    #[test]
    fn saturating_u64_clamps_overflow() {
        assert_eq!(saturating_u64(0), 0);
        assert_eq!(saturating_u64(1_234), 1_234);
        assert_eq!(saturating_u64(u128::from(u64::MAX)), u64::MAX);
        assert_eq!(saturating_u64(u128::from(u64::MAX) + 1), u64::MAX);
    }
}
