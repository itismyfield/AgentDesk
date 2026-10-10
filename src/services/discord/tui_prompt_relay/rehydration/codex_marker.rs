use super::*;

#[cfg(all(unix, test))]
pub(crate) fn run_codex_rehydrate_pass_for_tests(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> Option<u64> {
    struct RestoreView(Option<Vec<String>>);
    impl Drop for RestoreView {
        fn drop(&mut self) {
            CODEX_PASS_TMUX_VIEW.set(self.0.take());
        }
    }
    let _view = RestoreView(CODEX_PASS_TMUX_VIEW.replace(Some(vec![tmux_session_name.to_owned()])));
    let _ = rehydrate_existing_codex_tui_bindings(shared, false);
    shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)
        .map(|channel| channel.get())
}

pub(super) fn restore_codex_owner_channel(
    shared: &Arc<SharedData>,
    tmux: &str,
    channel: u64,
) -> Option<bool> {
    crate::services::tmux_common::with_tmux_source_authority(tmux, |authority| {
        crate::services::tui_prompt_dedupe::codex_verified_channel_allowed_under_source_authority(
            authority, channel,
        )
        .then(|| {
            shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(tmux, ChannelId::new(channel))
        })
    })
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CodexTuiMarkerRehydrateDecision {
    Use {
        rollout_path: PathBuf,
        session_id: Option<String>,
    },
    TryFallback,
}

#[cfg(unix)]
pub(super) fn codex_tui_marker_rehydrate_decision(
    marker: &crate::services::codex_tui::session::CodexTuiRolloutMarker,
    claimed_rollout_paths: &HashSet<PathBuf>,
    duplicate_marker_paths: &HashSet<PathBuf>,
) -> CodexTuiMarkerRehydrateDecision {
    let path = &marker.rollout_path;
    let claim_path = canonical_rollout_claim_path(path);
    if path.exists()
        && !crate::services::codex_tui::rollout_index::rollout_is_subagent(path)
        && !duplicate_marker_paths.contains(&claim_path)
        && !rollout_path_is_claimed_for_other_session(path, claimed_rollout_paths)
    {
        return CodexTuiMarkerRehydrateDecision::Use {
            rollout_path: path.clone(),
            session_id: marker.session_id.clone(),
        };
    }
    CodexTuiMarkerRehydrateDecision::TryFallback
}
