//! #5943: how a queued `resume_offset` resolves into the watcher's next read
//! position, its duplicate-relay floor, and its `terminal_delivery_observed`
//! latch.
//!
//! Three producers write `TmuxWatcherHandle::resume_offset` with different
//! intents: `turn_bridge::finalize_epilogue::resume_pinned_watcher` hands the
//! watcher forward after the bridge DELIVERED a turn, the turn-start handoff in
//! `turn_bridge::runtime_handoff_loop` seeds a NEW turn (clearing
//! `turn_delivered` itself), and `health::relay_auto_heal`'s redrive asks a live
//! watcher to re-read an UNDELIVERED backlog of the turn already in flight.
//!
//! Only the redrive can name a point BEHIND what this watcher has already read,
//! and since #5943 it no longer clears `turn_delivered` on its way past — so at
//! a backward resume that marker still holds an EARLIER turn's delivery.
//!
//! This is its own module because both candidate homes
//! (`loop_poll_prologue.rs`, `liveness.rs`) sit exactly at their 700-line
//! namespace cap, and a cap is not the thing to raise. It hangs off
//! `loop_poll_prologue` rather than the watcher root because `tmux_watcher.rs`
//! is a registered giant (`scripts/giant_file_registry.toml`, #4712) whose
//! production line count may not grow — `giant_file_progress.py` fails the PR
//! for a single added line, and that number is not the thing to raise either.

pub(in crate::services::discord::tmux) struct WatcherResumeOutcome {
    pub(in crate::services::discord::tmux) terminal_delivery_observed: bool,
    pub(in crate::services::discord::tmux) last_relayed_offset: Option<u64>,
}

/// Resolve a queued resume point.
///
/// **The floor rule is unchanged from before #5943** and is deliberately left
/// alone: when the bridge delivered the turn, the floor is pinned at the resume
/// point itself, otherwise it is dropped. Pinning it AT the resume point is what
/// keeps `pre_emit_guard`'s duplicate-relay branch unreachable from here — that
/// branch fires on `data_start_offset < last_relayed_offset`, the caller assigns
/// `data_start_offset = current_offset = requested_offset`, and the two are
/// equal by construction. It matters because the branch is not a trim of the
/// already-relayed prefix: it suppresses the WHOLE batch, deletes the
/// placeholder and discards the pending buffer. Carrying a HIGHER pre-existing
/// floor past a rewind — as an earlier attempt at #5943 did — is what turns this
/// path into silent loss of everything between that floor and EOF.
///
/// **The latch rule is the #5943 change.** `terminal_delivery_observed` is
/// STICKY: `tmux_watcher` initialises it to `false` once per dispatch and
/// nothing resets it. It reaches
/// `watchers::lifecycle::liveness::tmux_death_should_attempt_restart_handoff`,
/// whose `!terminal_delivery_observed` requirement is the ONLY user-facing
/// signal for an abnormal mid-turn pane crash. So a marker latched past its own
/// turn does not merely mis-report one death — it retires that signal for the
/// whole remaining life of the watcher. A BACKWARD resume re-opens the current
/// turn rather than retiring one, so its marker is stale and must not latch.
/// Nothing current is lost by declining: every watcher-observed death site ORs
/// the LIVE `turn_delivered` in for itself, so a marker still set when the pane
/// dies is still seen.
pub(in crate::services::discord::tmux) fn watcher_resume_outcome(
    terminal_delivery_observed: bool,
    bridge_delivered_turn: bool,
    requested_offset: u64,
    read_offset: u64,
) -> WatcherResumeOutcome {
    let rewinds_read_position = requested_offset < read_offset;
    WatcherResumeOutcome {
        terminal_delivery_observed: super::watcher_lifecycle_terminal_delivery_observed(
            terminal_delivery_observed,
            bridge_delivered_turn && !rewinds_read_position,
        ),
        last_relayed_offset: bridge_delivered_turn.then_some(requested_offset),
    }
}

#[cfg(test)]
mod tests {
    use super::watcher_resume_outcome;

    /// #5943. The live shape from `~/.adk/release/logs/dcserver.stdout.log` at
    /// 2026-09-15T21:37 on channel 1479671298497183835: the watcher had read
    /// 10_914_930 bytes and the redrive asked it to resume at `0`.
    ///
    /// Before #5943 the redrive cleared `turn_delivered` itself, so this fold
    /// saw `false` and the latch stayed open. With that clear removed the marker
    /// arrives still set — from a turn that ended long before — and latching it
    /// would retire this watcher's only abnormal-mid-turn-crash signal for the
    /// rest of its life.
    #[test]
    fn a_rewinding_resume_does_not_latch_an_earlier_turns_delivery_5943() {
        let resumed = watcher_resume_outcome(false, true, 0, 10_914_930);
        assert!(
            !resumed.terminal_delivery_observed,
            "a backward resume re-opens the current turn; its marker is stale"
        );
        // An ALREADY-latched observation is this watcher's own, from
        // `tmux_watcher`'s terminal commit, and a resume never clears it.
        assert!(watcher_resume_outcome(true, true, 0, 10_914_930).terminal_delivery_observed);
    }

    /// Control: a forward resume keeps the pre-#5943 latch contract byte for
    /// byte. The bridge handing a delivered turn back still latches; an
    /// undelivered handback still does not.
    #[test]
    fn a_forward_resume_keeps_the_pre_5943_latch_contract_5943() {
        assert!(watcher_resume_outcome(false, true, 4_096, 1_024).terminal_delivery_observed);
        assert!(!watcher_resume_outcome(false, false, 4_096, 1_024).terminal_delivery_observed);
    }

    /// #5943 M3. The `requested == read` boundary, which the two directional
    /// tests above straddle without touching.
    ///
    /// It is not hypothetical: a bridge handback for a turn that produced no new
    /// bytes hands the watcher back the position it is already at. That is a
    /// FORWARD resume — the turn really was delivered and really is over — so the
    /// marker must still latch. Reading the boundary as a rewind (`<=`) would
    /// leave a normally delivered turn looking like a mid-turn crash candidate
    /// for the rest of this watcher's life.
    #[test]
    fn a_resume_at_the_current_read_position_is_not_a_rewind_5943() {
        assert!(
            watcher_resume_outcome(false, true, 4_096, 4_096).terminal_delivery_observed,
            "resuming exactly where the watcher already is retires the turn"
        );
        assert!(
            !watcher_resume_outcome(false, true, 4_095, 4_096).terminal_delivery_observed,
            "one byte behind IS a rewind"
        );
        assert!(watcher_resume_outcome(false, true, 4_097, 4_096).terminal_delivery_observed);
    }

    /// #5943 M6. The floor's PRESENCE, not just its value.
    ///
    /// The sweep below only checks that a floor, when one exists, never sits
    /// above the resume point — it says nothing about a floor that should not
    /// exist at all. An undelivered handback must drop the floor: raising one
    /// there both re-arms the duplicate-relay guard against bytes nobody
    /// delivered, and drags `loop_poll_prologue`'s `last_relayed_offset.is_some()`
    /// branch true, which pins a `.generation` mtime baseline the #1275 P2 #2
    /// comment exists to keep unset in exactly this case.
    #[test]
    fn an_undelivered_resume_drops_the_duplicate_relay_floor_5943() {
        for (requested, read) in [(0u64, 10_914_930u64), (4_096, 1_024), (4_096, 4_096)] {
            assert_eq!(
                watcher_resume_outcome(false, false, requested, read).last_relayed_offset,
                None,
                "undelivered resume {requested}/{read} must not arm a floor"
            );
            assert_eq!(
                watcher_resume_outcome(false, true, requested, read).last_relayed_offset,
                Some(requested),
                "a delivered resume pins the floor AT the resume point"
            );
        }
    }

    /// #5943: the floor must never land ABOVE the resume point.
    ///
    /// `pre_emit_guard` suppresses the whole batch — placeholder deleted,
    /// pending buffer discarded — when `data_start_offset < last_relayed_offset`,
    /// and the caller sets `data_start_offset` to exactly this resume point. An
    /// earlier attempt at #5943 preserved the pre-rewind floor here, which made
    /// that predicate true and destroyed `[floor, EOF)` instead of re-relaying
    /// it. Swept rather than sampled, so no ordering of the inputs can produce
    /// the suppression condition.
    #[test]
    fn no_resume_can_make_the_pre_emit_guard_suppress_the_batch_5943() {
        let points = [0u64, 1, 512, 1_024, 8_000_000, 9_000_000, 10_914_930];
        for &read_offset in &points {
            for &requested_offset in &points {
                for delivered in [false, true] {
                    for latched in [false, true] {
                        let resumed = watcher_resume_outcome(
                            latched,
                            delivered,
                            requested_offset,
                            read_offset,
                        );
                        if let Some(floor) = resumed.last_relayed_offset {
                            assert!(
                                requested_offset >= floor,
                                "resume point {requested_offset} is below the floor {floor}; \
                                 pre_emit_guard would suppress the whole batch"
                            );
                        }
                    }
                }
            }
        }
    }
}
