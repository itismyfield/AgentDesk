use super::super::super::codex_adoption_runtime::boot::LiveSource;
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
    let _ = rehydrate_existing_codex_tui_bindings(shared, None);
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

#[cfg(unix)]
pub(super) fn codex_rollout_session_id(rollout_path: &Path) -> Option<String> {
    let file = std::fs::File::open(rollout_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line).ok()?;
    let value: serde_json::Value = serde_json::from_str(first_line.trim()).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return None;
    }
    value
        .get("payload")
        .and_then(|payload| payload.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[cfg(unix)]
pub(super) fn codex_tui_launch_modified_since(tmux_session_name: &str) -> std::time::SystemTime {
    crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "sh")
        .and_then(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        })
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// A live pane of `channel` as discovery evidence: bound only on what the pass left registered.
pub(super) fn live_source(
    shared: &Arc<SharedData>,
    tmux: &str,
    channel: u64,
    observe: bool,
) -> LiveSource {
    LiveSource {
        key: tmux.to_owned(),
        channel: Some(channel),
        bound: observe && source_bound(shared, tmux, channel),
    }
}

/// An unplaced live pane may be any channel's source; off, its liveness is not even probed.
pub(super) fn unplaced_live_source(tmux: String, observe: bool) -> Option<LiveSource> {
    (observe && codex_pass_pane_is_live(&tmux)).then_some(LiveSource {
        key: tmux,
        channel: None,
        bound: false,
    })
}

/// Whether the pass left `tmux` routed to `channel` by the watcher registry, the relay's
/// authority, with a Codex binding on an existing rollout.
fn source_bound(shared: &Arc<SharedData>, tmux: &str, channel: u64) -> bool {
    let routed = shared.tmux_watchers.owner_channel_for_tmux_session(tmux);
    routed == Some(ChannelId::new(channel))
        && crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).is_some_and(
            |binding| {
                binding.runtime_kind == RuntimeHandoffKind::CodexTui
                    && Path::new(&binding.output_path).exists()
            },
        )
}
