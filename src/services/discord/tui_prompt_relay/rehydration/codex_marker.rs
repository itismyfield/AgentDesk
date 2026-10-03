use super::*;

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
