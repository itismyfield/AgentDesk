//! Latest-rollout handoff entry points and Discord tail options.

use super::*;

pub(super) fn pane_busy_probe_for_tmux(
    tmux_session_name: &str,
) -> Box<dyn FnMut() -> super::super::input::CodexPaneBusySignal + Send> {
    let tmux_session_name = tmux_session_name.to_string();
    let mut tracker = super::super::input::CodexPaneBusySignalTracker::default();
    Box::new(move || tracker.probe_tmux(&tmux_session_name))
}

/// The options every Discord-originated handoff tail sets, built in one place.
///
/// #5264 PR-B: `terminal_range_eligible` defaults to `false`, and `emit_done` gates the
/// `CodexTuiTerminalDone` emission on it. An entry point that forgets to decide it
/// therefore falls back to the legacy `Done`, produces no `CodexRange`, and skips the
/// pinned terminal cutover — with no compile error and no failing assertion anywhere.
/// The resumed entry point shipped with exactly that omission. Constructing the options
/// once makes the omission impossible instead of merely detectable.
pub(super) fn discord_handoff_tail_options(
    tmux_session_name: &str,
    discord_origin_prompt: Option<&str>,
) -> RolloutTailOptions {
    RolloutTailOptions {
        tmux_session_name: Some(tmux_session_name.to_string()),
        discord_origin_prompt: discord_origin_prompt.map(ToString::to_string),
        pane_busy_probe: Some(pane_busy_probe_for_tmux(tmux_session_name)),
        terminal_range_eligible: discord_origin_prompt.is_some_and(|text| !text.trim().is_empty()),
        ..RolloutTailOptions::default()
    }
}

pub(super) fn tail_latest_rollout_for_cwd_with_handoff_options<L: Into<PaneLiveness>>(
    cwd: &Path,
    modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    mut is_alive: impl FnMut() -> L,
    options: RolloutTailOptions,
) -> Result<CodexTuiTailResult, String> {
    if super::super::verified_tail::is_verified(options.tmux_session_name.as_deref()) {
        let binding = super::super::verified_tail::wait_for_binding(
            options.tmux_session_name.as_deref().unwrap_or_default(),
            cancel_token.as_deref(),
        )?;
        let rollout_path = PathBuf::from(&binding.output_path);
        return tail_rollout_file_until_assistant_response_with_pane_busy_probe(
            &rollout_path,
            binding.last_offset,
            binding.session_id,
            &sender,
            cancel_token,
            is_alive,
            options,
        )
        .map(|(read_result, outcome)| CodexTuiTailResult {
            read_result,
            rollout_path: outcome.rollout_path,
            final_offset: outcome.final_offset,
            session_id: outcome.session_id,
        });
    }
    let sessions_dir = default_codex_sessions_dir()
        .ok_or_else(|| "Codex sessions directory is unavailable".to_string())?;
    let mut death = ConfirmedDeadPane::default();
    let rollout_path = wait_for_latest_rollout_for_cwd(
        cwd,
        modified_since,
        &sessions_dir,
        cancel_token.as_deref(),
        &mut || !death.observe(is_alive().into(), None),
        options.wait_for_rollout,
    )?;
    let rollout_session_id = super::super::rollout_index::read_rollout_session_meta(&rollout_path)
        .and_then(|meta| meta.id);
    persist_codex_tui_rollout_marker(
        options.tmux_session_name.as_deref(),
        &rollout_path,
        rollout_session_id.as_deref(),
        Some(0),
    );
    tail_rollout_file_until_assistant_response_with_pane_busy_probe(
        &rollout_path,
        0,
        None,
        &sender,
        cancel_token,
        is_alive,
        options,
    )
    .map(|(read_result, outcome)| CodexTuiTailResult {
        read_result,
        rollout_path: outcome.rollout_path,
        final_offset: outcome.final_offset,
        session_id: outcome.session_id,
    })
}
