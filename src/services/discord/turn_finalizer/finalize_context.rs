//! #3894 — per-submission finalize context split out of `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): the `FinalizeContext` per-site knobs struct
//! and its constructor set (`bridge` / `watcher` / `monitor` / `gate_backstop`
//! / `delivery_lease`) lifted verbatim from the parent. Re-exported
//! (`pub(in crate::services::discord) use self::finalize_context::FinalizeContext`)
//! so every routed call site stays byte-identical. The private `gate_backstop`
//! constructor is widened to `pub(super)` (the sole visibility change) so the
//! reconcile/backstop child can still build the deadline-armed context.

/// Per-submission knobs that keep each routed call-site behaviourally
/// identical to its pre-#3016 inline sequence during the incremental window.
/// Routed sites preserve their old side-effects; only ownership moves.
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) struct FinalizeContext {
    /// Whether `do_finalize` clears inflight as part of the finalize. Bridge
    /// branches and the watcher clear inflight inline in their own flow before
    /// submitting, so they pass `false`; only the deadline-armed reconcile
    /// backstop (no caller to clear it) passes `true`.
    pub(in crate::services::discord) clear_inflight: bool,
    /// Whether to mark the removed token's completion-cleanup. The bridge did
    /// this on a non-cancel terminal (still gated on `!event.is_cancel()`); the
    /// watcher's `finish_restored_watcher_active_turn` did NOT — it only set
    /// `cancelled`. Keeping this per-site avoids changing provider-watchdog
    /// semantics on the watcher path.
    pub(in crate::services::discord) allow_completion_cleanup: bool,
    /// Whether to drain voice barge-in deferred prompts as part of finalize.
    /// The bridge branches drain voice; the watcher path did NOT.
    pub(in crate::services::discord) drain_voice: bool,
    /// Legacy context bit for paths that historically owned post-finalize queue
    /// admission. Queue drain is now driven by the #4048 completion-event
    /// listener; this bit remains as part of the backstop/no-owner context shape
    /// used by reaction cleanup and outcome assertions.
    pub(in crate::services::discord) kickoff_queue: bool,
    /// Whether an identity-guard miss is expected/idempotent for this submitter.
    /// The real reconcile backstop is intentionally quiet on misses; recovery
    /// submitters with the same side-effect knobs must still warn.
    pub(in crate::services::discord) expected_idempotent_guard_miss: bool,
}

impl FinalizeContext {
    /// Bridge non-delegation / missing-handoff branches: bridge owns the
    /// inflight clear elsewhere, marks completion-cleanup on non-cancel, drains
    /// voice, and leaves queue admission to the completion-event listener.
    pub(in crate::services::discord) fn bridge() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: true,
            drain_voice: true,
            kickoff_queue: false,
            expected_idempotent_guard_miss: false,
        }
    }

    /// Watcher terminal via `finish_restored_watcher_active_turn`: the watcher
    /// clears inflight inline before submitting, does NOT mark completion
    /// cleanup, does NOT drain voice. Queue admission is now driven by the
    /// #4048 completion-event listener; watcher-specific dispatch_ok guards
    /// remain outside finalizer cleanup.
    pub(in crate::services::discord) fn watcher() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: false,
            expected_idempotent_guard_miss: false,
        }
    }

    /// #4106: the watcher's late normal-completion finalize when the pre-panel
    /// early release (`release_restored_watcher_active_turn_before_panel_edit`)
    /// already released THIS turn's mailbox slot ahead of the awaited status-
    /// panel edit. Identical side-effect knobs to `watcher()`, but the identity-
    /// guard miss is now EXPECTED: the early release is the real releaser and
    /// this late submit is a deterministic idempotent no-op, so the guarded-miss
    /// log downgrades from WARN to debug. This keeps the WARN scoped to a GENUINE
    /// wrong-turn finalize (the anomaly operators diagnose the concurrency-cap
    /// wedge with) instead of firing on every steady-state normal completion.
    pub(in crate::services::discord) fn watcher_after_pre_panel_release() -> Self {
        Self {
            expected_idempotent_guard_miss: true,
            ..Self::watcher()
        }
    }

    /// Monitor-auto-turn / recovery terminal (#3016 phase 4): the caller owns
    /// the inflight clear (or there is none — synthetic monitor turn / recovery
    /// already cleared it), does NOT mark completion-cleanup, does NOT drain
    /// voice, and keeps the legacy queue-admission context bit. The actual
    /// drain trigger is now the #4048 completion-event listener.
    pub(in crate::services::discord) fn monitor() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: true,
            expected_idempotent_guard_miss: false,
        }
    }

    /// #4485 stale-busy mailbox recovery. The detector has positively proved
    /// the managed tmux session absent, so no live owner remains to clear the
    /// inflight row. The finalizer must clear it while retaining watcher-style
    /// cancellation semantics and completion-event queue admission.
    pub(in crate::services::discord) fn stale_busy_mailbox() -> Self {
        Self {
            clear_inflight: true,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: true,
            expected_idempotent_guard_miss: true,
        }
    }

    /// Deadline-armed gate-timeout backstop, fired from the reconciler with no
    /// caller to have cleared inflight: finalize fully (clear inflight here),
    /// no completion-cleanup or voice drain (watcher semantics), and preserve
    /// the legacy queue-admission context bit for cleanup/outcome semantics.
    pub(super) fn gate_backstop() -> Self {
        Self {
            clear_inflight: true,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: true,
            expected_idempotent_guard_miss: true,
        }
    }

    /// #3041 §3 P1-0 (DORMANT): context for a lease-release-driven finalize once
    /// the watcher terminal migrates onto the delivery lease (P1-1..). Mirrors
    /// `watcher()` today (no live caller), but kept as a distinct constructor so
    /// wired phases can tune the lease-release knobs independently.
    #[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
    pub(in crate::services::discord) fn delivery_lease() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: false,
            expected_idempotent_guard_miss: false,
        }
    }

    pub(in crate::services::discord) fn is_backstop_reconcile_path(self) -> bool {
        self.expected_idempotent_guard_miss
    }
}

#[cfg(test)]
mod tests {
    use super::FinalizeContext;

    /// #4106: the post-early-release watcher context must be identical to the
    /// plain `watcher()` context on EVERY side-effect knob and differ ONLY in
    /// expecting the identity-guard miss. That single difference is what
    /// downgrades the now-deterministic late guarded miss from WARN to debug
    /// WITHOUT changing any finalize side effect (inflight/completion-cleanup/
    /// voice/queue semantics stay exactly the watcher path's).
    #[test]
    fn watcher_after_pre_panel_release_only_flips_expected_guard_miss() {
        let base = FinalizeContext::watcher();
        let after = FinalizeContext::watcher_after_pre_panel_release();

        // The one intended difference.
        assert!(!base.expected_idempotent_guard_miss);
        assert!(after.expected_idempotent_guard_miss);
        assert!(!base.is_backstop_reconcile_path());
        assert!(after.is_backstop_reconcile_path());

        // Every other knob is unchanged — no side-effect drift.
        assert_eq!(after.clear_inflight, base.clear_inflight);
        assert_eq!(
            after.allow_completion_cleanup,
            base.allow_completion_cleanup
        );
        assert_eq!(after.drain_voice, base.drain_voice);
        assert_eq!(after.kickoff_queue, base.kickoff_queue);
    }
}
