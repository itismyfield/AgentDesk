//! Inflight-state derivation helpers for recovery (#3479 item-2 split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: the helpers that
//! derive recovery inputs from an `InflightTurnState` — the stale/interrupted
//! handoff message text, the ready-for-input probes, the worktree path/branch/
//! dispatch-id accessors, the worktree-info reconstruction (git + canonicalize),
//! and the spawn-cwd / session-worktree wiring that those feed. They depend only
//! on the parent module's re-exported types (`InflightTurnState`, `ProviderKind`,
//! `WorktreeInfo`, `DiscordSession`, `GitCommand`, …), pulled in via
//! `use super::*`, so this cluster lives in a leaf module. The handoff helper is
//! re-exported by the root module because `recovery_paths::restart` calls it; the
//! remaining members are re-imported so existing call sites stay byte-identical.

use super::*;

pub(super) fn interrupted_recovery_message(
    state: &inflight::InflightTurnState,
    saved_response: &str,
) -> String {
    state
        .restart_mode
        .map(|mode| super::turn_bridge::handoff_interrupted_message(mode, saved_response))
        .unwrap_or_else(|| stale_inflight_message(saved_response))
}

/// WARN-only trace (160-char response tail) — writes NOTHING to disk. The
/// durable full-response artifact for force-clears is
/// `recovery_paths::restart::persist_force_clear_report` (#3297 finding 3).
pub(in crate::services::discord) fn save_missing_session_handoff(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    best_response: &str,
) {
    let partial = best_response.trim();
    let partial_summary = if partial.is_empty() {
        "partial response unavailable".to_string()
    } else {
        tail_with_ellipsis(partial, 160)
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ recovery: suppressed auto post-restart handoff for channel {} (provider={}, user_msg_id={}, partial={})",
        state.channel_id,
        provider.as_str(),
        state.user_msg_id,
        partial_summary
    );
}

fn inflight_ready_for_input_without_tui_pane(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    require_consumed: bool,
) -> Option<crate::services::tui_turn_state::TuiReadyState> {
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        state.runtime_kind,
        std::path::Path::new(output_path),
        require_consumed.then_some(state.last_offset),
    )
}

pub(super) fn inflight_or_legacy_tmux_ready_for_input(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    tmux_session_name: &str,
    require_consumed: bool,
) -> bool {
    inflight_ready_for_input_without_tui_pane(provider, state, require_consumed)
        .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
        .unwrap_or_else(|| {
            crate::services::provider::tmux_session_fallback_ready_for_input(
                tmux_session_name,
                provider,
                state.runtime_kind,
            )
            .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
        })
}

fn recovery_worktree_path(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .worktree_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
}

fn recovery_worktree_branch(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .worktree_branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
}

fn recovery_dispatch_id(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .dispatch_id
        .as_deref()
        .map(str::trim)
        .filter(|dispatch_id| !dispatch_id.is_empty())
}

pub(super) fn recovery_tmux_session_name(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> Option<String> {
    state
        .tmux_session_name
        .as_deref()
        .or_else(|| state.channel_name.as_deref())
        .map(|name| {
            if name.starts_with(&format!(
                "{}-",
                crate::services::provider::TMUX_SESSION_PREFIX
            )) {
                name.to_string()
            } else {
                provider.build_tmux_session_name(name)
            }
        })
}

fn recovery_requires_worktree_context(state: &inflight::InflightTurnState) -> bool {
    recovery_worktree_branch(state).is_some()
        || state
            .base_commit
            .as_deref()
            .is_some_and(|commit| !commit.trim().is_empty())
}

fn recovery_git_stdout(repo_path: &str, args: &[&str]) -> Option<String> {
    let output = GitCommand::new()
        .repo(repo_path)
        .args(args)
        .run_output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return None;
    }
    Some(stdout)
}

fn recovery_worktree_original_path(worktree_path: &str) -> Option<String> {
    let git_common_dir = recovery_git_stdout(worktree_path, &["rev-parse", "--git-common-dir"])?;
    let common_dir = {
        let candidate = std::path::PathBuf::from(&git_common_dir);
        if candidate.is_absolute() {
            candidate
        } else {
            std::path::Path::new(worktree_path).join(candidate)
        }
    };
    let canonical = std::fs::canonicalize(common_dir).ok()?;
    canonical.parent()?.to_str().map(str::to_string)
}

fn recovery_worktree_info(state: &inflight::InflightTurnState) -> Option<WorktreeInfo> {
    let worktree_path = recovery_worktree_path(state)?;
    if !std::path::Path::new(worktree_path).is_dir() {
        return None;
    }

    let branch_name = recovery_worktree_branch(state)
        .map(str::to_string)
        .or_else(|| recovery_git_stdout(worktree_path, &["branch", "--show-current"]))?;
    let original_path = recovery_worktree_original_path(worktree_path)?;

    Some(WorktreeInfo {
        original_path,
        worktree_path: worktree_path.to_string(),
        branch_name,
    })
}

pub(super) fn restore_recovered_session_worktree(
    session: &mut DiscordSession,
    state: &inflight::InflightTurnState,
) {
    if let Some(worktree) = recovery_worktree_info(state) {
        if session.current_path.is_none() {
            session.current_path = Some(worktree.worktree_path.clone());
        }
        session.worktree = Some(worktree);
    }
}

pub(super) fn recovery_spawn_adk_cwd(
    state: &inflight::InflightTurnState,
    persisted_session_path: Option<String>,
) -> Result<Option<String>, String> {
    if let Some(worktree_path) = recovery_worktree_path(state) {
        if std::path::Path::new(worktree_path).is_dir() {
            return Ok(Some(worktree_path.to_string()));
        }
        return Err(format!(
            "recovery blocked: inflight worktree missing for channel {}: {}",
            state.channel_id, worktree_path
        ));
    }

    if recovery_requires_worktree_context(state) {
        let dispatch_suffix = recovery_dispatch_id(state)
            .map(|dispatch_id| format!(" (dispatch {dispatch_id})"))
            .unwrap_or_default();
        return Err(format!(
            "recovery blocked: inflight worktree state missing for channel {}{}",
            state.channel_id, dispatch_suffix
        ));
    }

    Ok(persisted_session_path)
}
