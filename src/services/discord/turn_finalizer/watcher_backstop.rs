//! #3479 r9 — watcher far-backstop liveness re-check split out of
//! `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): the proven-terminal fast-path tunables
//! (`WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL` / `WATCHER_BACKSTOP_TERMINAL_STREAK`)
//! and the reconciler's terminal-or-defer verdict pair
//! (`watcher_backstop_turn_is_terminal` / `watcher_backstop_signal_is_terminal`),
//! plus the pure signal-truth-table unit test. The parent re-imports the
//! consts + fns (`use self::watcher_backstop::{...}`) so the `reconcile` loop
//! call sites stay byte-identical.

use super::*;

/// #3277 (Defect C) — proven-terminal FAST path for the watcher far-backstop.
/// In the #3277 incident the handed-off turn was already PROVABLY complete
/// (JSONL terminator on disk) while its watcher owner sat parked at transcript
/// EOF, so no data-driven finalize ever fired and the channel stayed stranded
/// for the full 1800s. The reconciler therefore PROBES watcher-owned Pending
/// entries with the STRICT (`at_deadline = false`) form of
/// `watcher_backstop_turn_is_terminal`: after
/// `WATCHER_BACKSTOP_TERMINAL_STREAK` terminal probes this interval apart, the
/// far deadline is pulled in to `GATE_BACKSTOP` for a third (still strict)
/// confirmation before finalizing. A single non-terminal probe resets the
/// streak (paused / paused-live / flapping turns keep the generous horizon).
pub(super) const WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// Consecutive terminal probes required before the fast path pulls the
/// watcher far-backstop deadline in (see above).
pub(super) const WATCHER_BACKSTOP_TERMINAL_STREAK: u8 = 2;

/// #3016 phase-5a — the reconciler's terminal-or-defer verdict for a
/// watcher-owned `register_start` Pending. `at_deadline == true` is the
/// NATURAL 1800s far-backstop expiry; `false` (the #3277 fast-path probe AND
/// the re-check of a fast-path-PULLED deadline, codex r1) stays STRICTLY
/// transcript-proven. Never finalizes a legitimately long paused-live turn:
///   * NO LIVE handle — absent (also under the inflight `tmux_session_name`
///     re-key below: #3277 verify-1, a `claim_or_reuse_watcher` ReuseExisting
///     dispatch registers under the OWNER channel only), `cancel` set, or
///     `heartbeat_stale()` (#3268) → terminal ONLY at the natural deadline
///     (nothing is left to drive the pane). The strict mode DEFERS: a watcher
///     replace/reuse leaves the registry transiently absent/stale while the
///     transcript still says busy — absence proves nothing about the TURN;
///     dead/absent authority stays with the far horizon, never the fast path.
///   * live-but-`paused` (a Discord turn took the session over) → defer.
///   * else `watcher_backstop_signal_is_terminal` on the transcript: `Done`
///     terminal; `PausedLive` defers; `Unknown` (non-JSONL runtime) consults
///     the pane-ready fallback ONLY at the natural deadline.
pub(super) fn watcher_backstop_turn_is_terminal(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    at_deadline: bool,
) -> bool {
    let inflight_tmux =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
            .and_then(|state| state.tmux_session_name);
    let (tmux_session_name, output_path, paused) = {
        let handle = match inflight_tmux.as_deref() {
            Some(tmux) => shared.tmux_watchers.by_tmux_session.get(tmux),
            None => shared.tmux_watchers.get(&channel_id),
        };
        let Some(handle) = handle else {
            return at_deadline;
        };
        if handle.cancel.load(std::sync::atomic::Ordering::Relaxed) || handle.heartbeat_stale() {
            return at_deadline;
        }
        (
            handle.tmux_session_name.clone(),
            handle.output_path.clone(),
            handle.paused.load(std::sync::atomic::Ordering::Acquire),
        )
        // dashmap `Ref` dropped here, BEFORE the (blocking) pane capture below.
    };
    if paused {
        return false;
    }
    let runtime_kind =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux_session_name)
            .map(|binding| binding.runtime_kind)
            .or_else(|| {
                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&tmux_session_name)
            });
    watcher_backstop_signal_is_terminal(
        completion_signal_from_transcript(
            provider,
            runtime_kind,
            std::path::Path::new(&output_path),
        ),
        at_deadline,
        || {
            crate::services::provider::tmux_session_fallback_ready_for_input(
                &tmux_session_name,
                provider,
                runtime_kind,
            )
            .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
        },
    )
}

/// #3277 verify-3 — the verdict over the transcript completion signal. The
/// strict mode (`allow_pane_probe == false`: fast-path probe and pulled
/// re-check) treats `Unknown` (non-JSONL Gemini / OpenCode / Qwen / legacy
/// wrapper: no provable terminator) as NON-terminal: the synchronous
/// pane-capture fallback can misread a dialog or a long silent stretch as
/// idle, and probing it every 15s would amplify the old once-per-1800s
/// exposure ~120× (and block the actor task). Only the NATURAL at-deadline
/// re-check (`true`) consults `pane_ready` — lazily, only on `Unknown`.
pub(super) fn watcher_backstop_signal_is_terminal(
    signal: CompletionSignal,
    allow_pane_probe: bool,
    pane_ready: impl FnOnce() -> bool,
) -> bool {
    match signal {
        CompletionSignal::PausedLive => false,
        CompletionSignal::Done => true,
        CompletionSignal::Unknown => allow_pane_probe && pane_ready(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3277 verify-3 (MINOR) truth table: the fast-path probe
    /// (`allow_pane_probe == false`) must NEVER report `Unknown` (non-JSONL
    /// runtime) as terminal — and must not even RUN the pane capture — while
    /// the at-deadline re-check keeps the pane-ready fallback. `Done` /
    /// `PausedLive` verdicts are identical in both modes.
    #[test]
    fn non_jsonl_signal_never_terminal_on_fast_path_probe() {
        use std::cell::Cell;
        // Unknown + fast path: non-terminal AND the pane capture must not run.
        let captured = Cell::new(false);
        assert!(!watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            false,
            || {
                captured.set(true);
                true
            }
        ));
        assert!(
            !captured.get(),
            "the 15s fast-path probe must never run a blocking pane capture"
        );
        // Unknown + at-deadline: pane fallback decides (both directions).
        assert!(watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            true,
            || true
        ));
        assert!(!watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            true,
            || false
        ));
        // Done / PausedLive: identical in both modes, pane never consulted.
        for probe in [false, true] {
            assert!(watcher_backstop_signal_is_terminal(
                CompletionSignal::Done,
                probe,
                || unreachable!("Done must not consult the pane")
            ));
            assert!(!watcher_backstop_signal_is_terminal(
                CompletionSignal::PausedLive,
                probe,
                || unreachable!("PausedLive must not consult the pane")
            ));
        }
    }
}
