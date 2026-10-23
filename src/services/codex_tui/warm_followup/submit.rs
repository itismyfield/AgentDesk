use super::*;

/// Settles the one submit; `None` goes on to tail the rollout. A draft that stayed
/// in the composer is killed for a relaunch only on a confirmed tmux session.
#[cfg(unix)]
pub(super) fn settle_submit(
    target: &InputTarget,
    tmux_session_name: &str,
    outcome: CodexFollowupPromptSubmitOutcome,
    rollout_path: &Path,
    rollout_len_before_submit: u64,
    prompt: &str,
    (report_channel_id, cancel_token): (Option<u64>, Option<&std::sync::Arc<CancelToken>>),
) -> Option<CodexWarmFollowupOutcome> {
    let rollout_len = || {
        std::fs::metadata(rollout_path)
            .ok()
            .map(|metadata| metadata.len())
    };
    let settled = match outcome {
        CodexFollowupPromptSubmitOutcome::Submitted => return None,
        CodexFollowupPromptSubmitOutcome::NotSubmitted { error } => {
            if !pre_enter_failure_allows_fallback(rollout_len_before_submit, rollout_len()) {
                return Some(CodexWarmFollowupOutcome::Terminal(Err(format!(
                    "Codex TUI warm follow-up failed before Enter but rollout advanced; refusing replay: {error}"
                ))));
            }
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                ProviderKind::Codex.as_str(),
                tmux_session_name,
                prompt,
            );
            let reason = CodexWarmFallbackReason::SubmitFailed;
            log_fallback(
                tmux_session_name,
                reason,
                &format!("prompt delivery failed before Enter: {error}"),
            );
            CodexWarmFollowupOutcome::Fallback(reason)
        }
        CodexFollowupPromptSubmitOutcome::Cancelled => CodexWarmFollowupOutcome::Terminal(Ok(())),
        CodexFollowupPromptSubmitOutcome::RetrySafeDraft { first, second } => {
            if !submit_failure_allows_fallback(
                crate::services::codex_tui::input::prompt_draft_matches(&first, prompt),
                crate::services::codex_tui::input::prompt_draft_matches(&second, prompt),
                rollout_len_before_submit,
                rollout_len(),
            ) {
                return Some(CodexWarmFollowupOutcome::Terminal(Err(
                    "Codex TUI warm follow-up submit was unconfirmed; refusing replay".to_string(),
                )));
            }
            let reason = CodexWarmFallbackReason::SubmitFailed;
            if let Err(error) = preflight_warm_effect(
                tmux_session_name,
                report_channel_id,
                &CodexLaunchOptions::new(prompt),
            ) {
                return Some(CodexWarmFollowupOutcome::Terminal(Err(error)));
            }
            let replacement =
                match Replacement::begin(report_channel_id, tmux_session_name, cancel_token) {
                    Ok(replacement) => replacement,
                    Err(error) => return Some(CodexWarmFollowupOutcome::Terminal(Err(error))),
                };
            if let Err(error) = host_input::kill_legacy_pane(target, reason.reason_text()) {
                return Some(CodexWarmFollowupOutcome::Terminal(Err(error)));
            }
            if rollout_len() != Some(rollout_len_before_submit) {
                return Some(CodexWarmFollowupOutcome::Terminal(Err(
                    "Codex TUI warm follow-up rollout advanced across the pane-kill barrier; refusing replay"
                        .to_string(),
                )));
            }
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                ProviderKind::Codex.as_str(),
                tmux_session_name,
                prompt,
            );
            log_fallback(
                tmux_session_name,
                reason,
                "draft persisted in two snapshots and rollout stayed unchanged through the pane-kill barrier",
            );
            CodexWarmFollowupOutcome::FallbackAfterPaneKill(reason, replacement)
        }
        CodexFollowupPromptSubmitOutcome::Unconfirmed { error, snapshot } => {
            tracing::error!(
                tmux_session_name,
                error,
                tmux_pane_alive = snapshot.tmux_pane_alive,
                capture_available = snapshot.capture_available,
                composer_marker_detected = snapshot.composer_marker_detected,
                prompt_draft_detected = snapshot.prompt_draft_detected,
                "Codex TUI warm follow-up submit unconfirmed; refusing cold replay"
            );
            CodexWarmFollowupOutcome::Terminal(Err(error))
        }
        CodexFollowupPromptSubmitOutcome::Refused { run } => {
            CodexWarmFollowupOutcome::Terminal(Err(host_input::refusal_error(&run)))
        }
    };
    Some(settled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState, known, resolved};
    use crate::services::discord::codex_absence::replacement::Probe;
    use crate::services::session_host::HostKind;
    use std::sync::Arc;

    #[test]
    fn tracked_retry_safe_draft_carries_replacement_through_its_pane_kill() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE",
            "legacy".as_ref(),
        );
        let channel = 62_841_779;
        let tmux = "AgentDesk-codex-replacement-draft";
        let token = Arc::new(CancelToken::new());
        crate::services::discord::save_actual_episode_for_tests(channel, tmux, 30, &token);
        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(path.path(), "{}\n").unwrap();
        let prompt = "keep this draft";
        let draft = PromptReadinessSnapshot {
            composer_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: format!(
                "old output\n╭────────────────────────────────────────╮\n│ {prompt} ▌ │\n╰────────────────────────────────────────╯\n  ⏎ send"
            ),
        };
        let target = resolved(known(HostKind::Tmux));
        let spy = SpyGuard::install(SpyState {
            pane_pid: Some(4242),
            ..Default::default()
        });
        let probe = Probe::new(channel, tmux);
        let outcome = settle_submit(
            &target,
            tmux,
            CodexFollowupPromptSubmitOutcome::RetrySafeDraft {
                first: draft.clone(),
                second: draft,
            },
            path.path(),
            3,
            prompt,
            (Some(channel), Some(&token)),
        );
        let Some(CodexWarmFollowupOutcome::FallbackAfterPaneKill(reason, Some(replacement))) =
            outcome
        else {
            panic!("tracked pane kill must transfer its active replacement to cold launch");
        };
        assert_eq!(reason, CodexWarmFallbackReason::SubmitFailed);
        assert!(probe.defer_death());
        assert!(probe.cleanup().is_none());
        assert_eq!(
            spy.calls(),
            [
                "pane_pid",
                "kill_tree",
                "kill_session:Codex TUI warm follow-up submit failed with draft preserved",
                "stopped"
            ]
        );
        replacement.complete();
        assert!(!Probe::new(channel, tmux).defer_death());
    }
}
