use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::tmux_watcher_now_ms;
    use poise::serenity_prelude::ChannelId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

    fn test_watcher_handle(tmux_session_name: &str, output_path: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(tmux_watcher_now_ms())),
        }
    }

    // #3898 — cancel suppression is the surviving gate that protects the restart
    // handoff (and previously gated the now-removed "session ended" notice).
    #[test]
    fn cancel_suppression_only_applies_before_terminal_delivery() {
        // A cancel/turn-stop candidate that died before delivering its turn is
        // suppressed (no restart handoff; historically no notice either).
        assert!(cancel_suppression_applies_to_watcher_death(true, false));
        // Once a terminal turn was delivered, a later pane death is a real
        // lifecycle event for that turn — stale cancel suppression must NOT apply.
        assert!(!cancel_suppression_applies_to_watcher_death(true, true));
        // No cancel candidate → never suppressed.
        assert!(!cancel_suppression_applies_to_watcher_death(false, false));
        assert!(!cancel_suppression_applies_to_watcher_death(false, true));
    }

    // #3898 — the false-positive fix. A tmux pane that exits *after* delivering
    // its turn (normal idle / cleanup / force-kill, which leave no
    // normal-completion marker → `is_normal_completion == false`) must NOT
    // surface any lifecycle signal: the removed notice never fires, and the
    // restart handoff is gated off by `terminal_delivery_observed`. This is the
    // noise the issue reported — a normal idle exit emitting a spurious notice.
    #[test]
    fn delivered_then_idle_death_surfaces_no_lifecycle_signal() {
        let cancel_induced = cancel_suppression_applies_to_watcher_death(false, true);
        assert!(!cancel_induced);
        // Even with an "abnormal" exit reason (no normal-completion marker), a
        // delivered turn means this is a normal idle/cleanup/force-kill teardown.
        assert!(!tmux_death_should_attempt_restart_handoff(
            cancel_induced,
            /* prompt_too_long_killed */ false,
            /* terminal_delivery_observed */ true,
            /* is_normal_completion */ false,
        ));
        // The same holds when the pane DID exit through a normal-completion path.
        assert!(!tmux_death_should_attempt_restart_handoff(
            cancel_induced,
            false,
            true,
            true,
        ));
    }

    // #3898 — a genuinely abnormal mid-turn pane crash (turn aborted BEFORE
    // terminal delivery, no normal-completion marker) STILL surfaces a signal:
    // the resume-aborted restart handoff fires. This is the case the removed
    // notice never covered (it required `terminal_delivery_observed`), so
    // removing the notice does not lose any genuine-crash signal.
    #[test]
    fn genuine_mid_turn_crash_triggers_restart_handoff() {
        assert!(tmux_death_should_attempt_restart_handoff(
            /* cancel_induced */ false, /* prompt_too_long_killed */ false,
            /* terminal_delivery_observed */ false, /* is_normal_completion */ false,
        ));
        // Suppressed when the user canceled the turn themselves …
        assert!(!tmux_death_should_attempt_restart_handoff(
            true, false, false, false
        ));
        // … when the prompt-too-long teardown already handled it …
        assert!(!tmux_death_should_attempt_restart_handoff(
            false, true, false, false
        ));
        // … or when the pane exited through a normal-completion path.
        assert!(!tmux_death_should_attempt_restart_handoff(
            false, false, false, true
        ));
    }

    #[test]
    fn same_tmux_different_output_path_replaces_watcher() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_a = ChannelId::new(1485506232256168134);
        let channel_b = ChannelId::new(1485506232256168135);
        let tmux_name = "AgentDesk-codex-adk-cdx-path-change";

        let initial = test_watcher_handle(tmux_name, "/tmp/prelaunch-wrapper.jsonl");
        let initial_cancel = initial.cancel.clone();
        assert!(try_claim_watcher(&watchers, channel_a, initial));

        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_b,
            test_watcher_handle(tmux_name, "/tmp/provider-runtime.jsonl"),
            &ProviderKind::Codex,
            "unit-test-output-path-change",
        );

        assert_eq!(outcome.action, WatcherClaimAction::SpawnReplacedStale);
        assert_eq!(outcome.owner_channel_id(), channel_b);
        assert!(initial_cancel.load(Ordering::Relaxed));
        let watcher = watchers.get(&channel_b).expect("replacement watcher");
        assert_eq!(watcher.output_path, "/tmp/provider-runtime.jsonl");
        assert!(!watchers.contains_key(&channel_a));
    }

    /// #4455: a crossed-provider-turn Codex rebind must replace even a live
    /// same-session/same-output incumbent. Reuse would leave its stale
    /// `current_msg_id` render seed and converter generation in authority.
    #[test]
    fn forced_rebind_replaces_live_same_output_incumbent() {
        let watchers = TmuxWatcherRegistry::new();
        let owner = ChannelId::new(1_485_506_232_256_168_136);
        let dispatch = ChannelId::new(1_485_506_232_256_168_137);
        let tmux_name = "AgentDesk-codex-adk-cdx-4455-forced";
        let output = "/tmp/codex-4455-normalized.jsonl";
        let incumbent = test_watcher_handle(tmux_name, output);
        let incumbent_cancel = incumbent.cancel.clone();
        assert!(try_claim_watcher(&watchers, owner, incumbent));

        let outcome = claim_or_replace_watcher(
            &watchers,
            dispatch,
            test_watcher_handle(tmux_name, output),
            &ProviderKind::Codex,
            "recovery_restore_inflight_crossed_codex_turn",
        );

        assert_eq!(outcome.action, WatcherClaimAction::SpawnReplacedForced);
        assert!(outcome.should_spawn() && outcome.replaced_existing());
        assert_eq!(outcome.owner_channel_id(), dispatch);
        assert!(incumbent_cancel.load(Ordering::Relaxed));
        assert!(!watchers.contains_key(&owner));
        assert_eq!(
            watchers
                .get(&dispatch)
                .expect("forced replacement watcher")
                .output_path,
            output
        );
    }

    /// #3277 verify-2 truth table for the `recovery_restore_inflight` claim: a
    /// same-session incumbent is REPLACED only when it provably cannot own the
    /// relay — cancelled, heartbeat-stale (the Defect D hung-watcher subcase;
    /// `find_watcher_by_tmux_session` folds `heartbeat_stale()` into its
    /// replace predicate), paused, or bound to a different output path. A
    /// genuinely-live fresh-heartbeat unpaused same-output incumbent is REUSED
    /// untouched (never a duplicate-relay vector) and keeps the EXISTING owner
    /// channel (owner ≠ dispatch).
    #[test]
    fn recovery_restore_claim_replaces_dead_incumbent_only() {
        let tmux_name = "AgentDesk-claude-adk-cc-recovery-claim";
        let output = "/tmp/recovery-claim.jsonl";
        let owner = ChannelId::new(1_500_000_000_000_000_001);
        let dispatch = ChannelId::new(2_600_000_000_000_000_002);
        let claim = |incumbent: TmuxWatcherHandle, requested_output: &str| {
            let watchers = TmuxWatcherRegistry::new();
            assert!(try_claim_watcher(&watchers, owner, incumbent));
            claim_or_reuse_watcher(
                &watchers,
                dispatch,
                test_watcher_handle(tmux_name, requested_output),
                &ProviderKind::Claude,
                "recovery_restore_inflight",
            )
        };

        // Live fresh unpaused same-output → REUSED, owner channel preserved.
        let reused = claim(test_watcher_handle(tmux_name, output), output);
        assert_eq!(reused.as_str(), "reuse_existing");
        assert!(!reused.should_spawn());
        assert_eq!(reused.owner_channel_id(), owner);

        // Heartbeat-stale (NOT cancelled) → replaced: the Defect D hung watcher.
        let stale = test_watcher_handle(tmux_name, output);
        stale.last_heartbeat_ts_ms.store(1, Ordering::Release);
        let outcome = claim(stale, output);
        assert!(outcome.should_spawn() && outcome.replaced_existing());

        // Cancelled → replaced.
        let cancelled = test_watcher_handle(tmux_name, output);
        cancelled.cancel.store(true, Ordering::Relaxed);
        let outcome = claim(cancelled, output);
        assert!(outcome.should_spawn() && outcome.replaced_existing());

        // Paused (recovery source is not a turn-start) → replaced.
        let paused = test_watcher_handle(tmux_name, output);
        paused.paused.store(true, Ordering::Release);
        let outcome = claim(paused, output);
        assert!(outcome.should_spawn() && outcome.replaced_existing());

        // Different output path → replaced.
        let outcome = claim(
            test_watcher_handle(tmux_name, output),
            "/tmp/recovery-claim-other.jsonl",
        );
        assert!(outcome.should_spawn() && outcome.replaced_existing());
    }

    #[test]
    fn liveness_probe_clears_stale_marker_when_pane_alive() {
        assert_eq!(
            evaluate_liveness_probe(true, true),
            LivenessProbeOutcome::StaleMarkerClearAndAlive
        );
    }

    #[test]
    fn liveness_probe_honors_marker_when_pane_dead() {
        assert_eq!(
            evaluate_liveness_probe(true, false),
            LivenessProbeOutcome::MarkerHonoredDead
        );
    }

    #[test]
    fn liveness_probe_uses_pane_check_when_no_marker() {
        assert_eq!(
            evaluate_liveness_probe(false, true),
            LivenessProbeOutcome::PaneCheckOnly { alive: true }
        );
        assert_eq!(
            evaluate_liveness_probe(false, false),
            LivenessProbeOutcome::PaneCheckOnly { alive: false }
        );
    }

    #[test]
    fn restore_scan_only_skips_same_live_output_path() {
        assert!(restore_scan_should_skip_existing_watcher(
            false,
            false,
            "/tmp/wrapper.jsonl",
            "/tmp/wrapper.jsonl"
        ));
        assert!(!restore_scan_should_skip_existing_watcher(
            false,
            false,
            "/tmp/prelaunch.jsonl",
            "/tmp/restored.jsonl"
        ));
        assert!(!restore_scan_should_skip_existing_watcher(
            true,
            false,
            "/tmp/wrapper.jsonl",
            "/tmp/wrapper.jsonl"
        ));
        assert!(!restore_scan_should_skip_existing_watcher(
            false,
            true,
            "/tmp/wrapper.jsonl",
            "/tmp/wrapper.jsonl"
        ));
    }

    #[test]
    fn post_work_ready_evidence_ignores_task_notification_only_turns() {
        let tool_state = WatcherToolState::new();

        assert!(
            !watcher_has_post_work_ready_evidence(
                "",
                &tool_state,
                Some(TaskNotificationKind::Background),
            ),
            "a task notification alone can be older pane state and must not prove this turn finished"
        );
    }

    #[test]
    fn post_work_ready_evidence_accepts_response_or_tool_output() {
        let tool_state = WatcherToolState::new();
        assert!(watcher_has_post_work_ready_evidence(
            "done",
            &tool_state,
            None
        ));

        let mut tool_state = WatcherToolState::new();
        tool_state.any_tool_used = true;
        assert!(watcher_has_post_work_ready_evidence(
            "",
            &tool_state,
            Some(TaskNotificationKind::Subagent),
        ));
    }
}
