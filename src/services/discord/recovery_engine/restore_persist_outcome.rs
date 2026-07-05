//! Persist-outcome handling for restart-path inflight recovery.
//!
//! Behavior-preserving extraction from `restore_inflight.rs`: resolve Codex TUI
//! rollout fallbacks and handle guarded persist outcomes before watcher spawn.

use super::*;

pub(super) enum RestorePersistOutcome {
    UseOutputPath(String),
    SkipWatcher,
}

pub(super) fn restore_codex_rollout_output_path(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    output_path: String,
) -> RestorePersistOutcome {
    // #2795 — codex_tui writes its rollout transcript directly to
    // `~/.codex/sessions/...`; the inflight's stored `output_path` is
    // the AgentDesk-side relay JSONL which may not exist on disk yet
    // when dcserver quick-exits mid-turn (e.g. agent ran deploy from
    // inside its own turn). Without a falling-back lookup the
    // `metadata` check below silently fails and recovery never spawns
    // a watcher, leaving the live codex pane permanently un-relayed.
    // Resolve the actual rollout via the inflight `session_id` and
    // persist the corrected path so subsequent restarts also find it.
    let mut output_path = output_path;
    if std::fs::metadata(&output_path).is_err()
        && matches!(
            state.runtime_kind,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
        )
    {
        if let Some(session_id) = state.session_id.as_deref() {
            if let Some(rollout) =
                crate::services::codex_tui::rollout_tail::find_rollout_by_session_id(session_id)
            {
                let rollout_str = rollout.display().to_string();
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ recovery: codex rollout fallback for channel {} — {} → {}",
                    state.channel_id,
                    output_path,
                    rollout_str
                );
                let persist_outcome =
                    inflight::persist_recovery_output_path_if_matches_identity_locked(
                        provider,
                        state.channel_id,
                        &inflight::InflightTurnIdentity::from_state(state),
                        rollout_str.clone(),
                    );
                match persist_outcome {
                    inflight::GuardedSaveOutcome::Saved => {
                        output_path = rollout_str;
                    }
                    inflight::GuardedSaveOutcome::IdentityMismatch => {
                        tracing::warn!(
                            provider = %provider.as_str(),
                            channel_id = state.channel_id,
                            stale_user_msg_id = state.user_msg_id,
                            stale_output_path = %output_path,
                            recovered_output_path = %rollout_str,
                            persist_outcome = ?persist_outcome,
                            "recovery restore skipped stale Codex rollout watcher because the inflight row was replaced during the restore scan"
                        );
                        return RestorePersistOutcome::SkipWatcher;
                    }
                    inflight::GuardedSaveOutcome::Missing => {
                        tracing::info!(
                            provider = %provider.as_str(),
                            channel_id = state.channel_id,
                            stale_user_msg_id = state.user_msg_id,
                            stale_output_path = %output_path,
                            recovered_output_path = %rollout_str,
                            persist_outcome = ?persist_outcome,
                            "recovery restore skipped stale Codex rollout watcher because the inflight row disappeared during the restore scan"
                        );
                        return RestorePersistOutcome::SkipWatcher;
                    }
                    inflight::GuardedSaveOutcome::IoError => {
                        // Durable state is unknown after an I/O error. Keep the
                        // previous best-effort restore behavior so a live rollout
                        // can still regain relay coverage from memory.
                        tracing::warn!(
                            provider = %provider.as_str(),
                            channel_id = state.channel_id,
                            stale_user_msg_id = state.user_msg_id,
                            stale_output_path = %output_path,
                            recovered_output_path = %rollout_str,
                            persist_outcome = ?persist_outcome,
                            "recovery restore could not persist Codex rollout fallback; proceeding best-effort with in-memory output path"
                        );
                        output_path = rollout_str;
                    }
                }
            }
        }
    }

    RestorePersistOutcome::UseOutputPath(output_path)
}
