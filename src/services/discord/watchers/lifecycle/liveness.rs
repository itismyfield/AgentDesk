use super::*;

pub(super) fn evaluate_liveness_probe(
    marker_present: bool,
    pane_alive: bool,
) -> LivenessProbeOutcome {
    match (marker_present, pane_alive) {
        (true, true) => LivenessProbeOutcome::StaleMarkerClearAndAlive,
        (true, false) => LivenessProbeOutcome::MarkerHonoredDead,
        (false, alive) => LivenessProbeOutcome::PaneCheckOnly { alive },
    }
}

pub(super) async fn probe_tmux_session_liveness(tmux_session_name: &str) -> bool {
    let marker_path = crate::services::tmux_common::session_dead_marker_path(tmux_session_name);
    let marker_present = std::path::Path::new(&marker_path).exists();

    let pane_alive = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking({
            let name = tmux_session_name.to_string();
            move || tmux_session_has_live_pane(&name)
        }),
    )
    .await
    .unwrap_or(Ok(false))
    .unwrap_or(false);

    match evaluate_liveness_probe(marker_present, pane_alive) {
        LivenessProbeOutcome::StaleMarkerClearAndAlive => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 clearing stale .pane_dead marker for {tmux_session_name} — tmux session is alive"
            );
            let _ = std::fs::remove_file(&marker_path);
            true
        }
        LivenessProbeOutcome::MarkerHonoredDead => false,
        LivenessProbeOutcome::PaneCheckOnly { alive } => alive,
    }
}

pub(super) fn watcher_output_file_offset(output_path: &str) -> Option<u64> {
    std::fs::metadata(output_path).ok().map(|meta| meta.len())
}

pub(super) fn cancel_suppression_applies_to_watcher_death(
    cancel_induced_candidate: bool,
    terminal_delivery_observed: bool,
) -> bool {
    cancel_induced_candidate && !terminal_delivery_observed
}

/// #3898 — whether a watcher-observed tmux death should attempt the
/// resume-aborted restart handoff (`resume_aborted_restart_turn`). This is the
/// ONLY user-facing lifecycle signal for a genuinely abnormal mid-turn pane
/// crash: the turn was aborted *before* terminal delivery and the pane did not
/// exit through a normal-completion path (`turn completed` / `exit:0` /
/// `routine fresh`).
///
/// The legacy "session ended: tmux pane exited. Send a new message to start a
/// new session." Discord notice (removed in #3898) is intentionally NOT
/// reinstated here. It was both noise and factually wrong:
/// - It required `terminal_delivery_observed`, so it never covered a genuine
///   mid-turn crash — that case routes to the restart handoff below, not to a
///   notice. The only deaths it actually fired on were delivered-then-idle /
///   cleanup / force-kill teardowns that left no normal-completion marker,
///   i.e. normal idle exits (false positive).
/// - A pane death does NOT start a fresh session: the DB `claude_session_id`
///   persists and the next message resumes the conversation with `--resume`,
///   so "start a new session" was incorrect for every death that reached it.
pub(super) fn tmux_death_should_attempt_restart_handoff(
    cancel_induced: bool,
    prompt_too_long_killed: bool,
    terminal_delivery_observed: bool,
    is_normal_completion: bool,
) -> bool {
    !cancel_induced
        && !prompt_too_long_killed
        && !terminal_delivery_observed
        && !is_normal_completion
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_tmux_watcher_observed_death(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    output_path: &str,
    _watcher_provider: &ProviderKind,
    prompt_too_long_killed: bool,
    terminal_delivery_observed: bool,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let diagnostic = build_tmux_death_diagnostic(tmux_session_name, Some(output_path));
    if let Some(diag) = diagnostic.as_deref() {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping ({diag})"
        );
    } else {
        tracing::info!("  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping");
    }
    let reason_short = read_tmux_exit_reason(tmux_session_name);
    let is_normal_completion =
        tmux_death_is_normal_completion(reason_short.as_deref(), diagnostic.as_deref());
    // The watcher cleanup path that follows an explicit cancel (user removed
    // the activity reaction or invoked /stop) writes
    // `record_tmux_exit_reason("watcher cleanup: dead session after turn")`
    // and tears the session down. Without this gate that synthetic reason
    // surfaces as a 🔴 lifecycle notification AND as the "대화를 이어붙이지
    // 못했습니다" handoff — both of which are noise for a user who just
    // canceled the turn themselves. The same suppression applies to the
    // immediate-respawn watcher death that can fire seconds later when the
    // next message arrives, since both are direct consequences of the cancel.
    //
    // For provider-native TUI relays the active watcher may be tailing a
    // rollout/transcript path rather than the legacy tmux-wrapper jsonl. Use
    // this watcher instance's actual output path for the EOF boundary check.
    // Also, once this watcher has observed terminal delivery for a fresh turn,
    // a later pane death is a lifecycle event for that turn, not the previous
    // reset/cancel cleanup. This signal is intentionally broader than the
    // watcher-local `turn_result_relayed` flag: session-bound StreamRelay can
    // deliver the Discord response before the watcher finishes its later
    // inflight/mailbox cleanup block.
    let death_output_offset = watcher_output_file_offset(output_path);
    let cancel_induced_candidate = cancel_induced_watcher_death_async(
        channel_id,
        tmux_session_name,
        death_output_offset,
        shared.pg_pool.as_ref(),
    )
    .await;
    let cancel_induced = cancel_suppression_applies_to_watcher_death(
        cancel_induced_candidate,
        terminal_delivery_observed,
    );
    // #3898 — the legacy "session ended … start a new session" Discord notice was
    // removed. It false-fired on normal idle / cleanup / force-kill teardown
    // (no `turn completed` / `exit:0` / `routine fresh` marker → classified
    // abnormal) and was factually wrong (a pane death resumes via `--resume`, it
    // does not start a fresh session). The genuine mid-turn crash signal is the
    // restart handoff below; cancel suppression is still computed because it
    // gates that handoff. `is_normal_completion` already folds in the exit-reason
    // normal-completion check (`tmux_death_is_normal_completion`), so it is the
    // single source of truth for the restart-handoff suppression.
    let attempt_restart_handoff = tmux_death_should_attempt_restart_handoff(
        cancel_induced,
        prompt_too_long_killed,
        terminal_delivery_observed,
        is_normal_completion,
    );
    if cancel_induced {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended after recent cancel/turn-stop, skipping lifecycle notification + restart handoff"
        );
    } else if cancel_induced_candidate {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended after a relayed terminal turn; ignoring stale cancel/turn-stop suppression"
        );
    } else if !is_normal_completion {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended without normal completion, skipping Discord lifecycle notification"
        );
    } else {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended after normal completion, skipping lifecycle notification"
        );
    }
    if attempt_restart_handoff {
        let _ =
            resume_aborted_restart_turn(channel_id, http, shared, tmux_session_name, output_path)
                .await;
    }
}
