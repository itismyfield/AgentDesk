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
        CodexFollowupPromptSubmitOutcome::Submitted => {
            crate::services::codex_tui::execution_end::producer::warm_bound(
                report_channel_id,
                tmux_session_name,
            );
            return None;
        }
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

/// Settles one submit for the warm follow-up test mounted in the discord-layer test host.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn settle_submit_for_test(
    target: &InputTarget,
    tmux_session_name: &str,
    outcome: CodexFollowupPromptSubmitOutcome,
    rollout_path: &Path,
    rollout_len_before_submit: u64,
    prompt: &str,
    report: (Option<u64>, Option<&std::sync::Arc<CancelToken>>),
) -> Option<CodexWarmFollowupOutcome> {
    settle_submit(
        target,
        tmux_session_name,
        outcome,
        rollout_path,
        rollout_len_before_submit,
        prompt,
        report,
    )
}

/// The clear end and ticket a warm follow-up test installs (KRX-TS): stored under the same pane authority the
/// production path reads, so upper-crate tests drive the real consumer.
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support {
    use crate::services::codex_tui::execution_end::attempt;
    use crate::services::tmux_common as tc;
    use crate::services::tui_o::store::codex_end::{self, CodexEnd, ContextSnapshot, EndBasis};

    pub(crate) fn install_clear_end(channel: u64, pane: &str) -> CodexEnd {
        use crate::services::tui_o::shadow::SourceId;
        let root = crate::config::runtime_root().unwrap();
        let old_nonce = "a".repeat(32);
        let now = chrono::Utc::now();
        let provider_root = root.join("codex");
        let context = ContextSnapshot {
            schema: 1,
            provider: "codex".into(),
            created_at: now,
            execution_nonce: old_nonce.clone(),
            tmux_session: pane.to_owned(),
            channel_id: Some(channel),
            owner_runtime_root: root.display().to_string(),
            host: Some("warm-test".into()),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(provider_root.clone()),
            first_prompt_digest: None,
            source_policy: Some("shadow".into()),
        };
        let end = CodexEnd {
            end_id: "warm-clear-end".into(),
            channel,
            provider: "codex".into(),
            tmux_session: pane.to_owned(),
            execution_nonce: old_nonce,
            host: "warm-test".into(),
            owner_runtime_root: root.display().to_string(),
            provider_root: provider_root.clone(),
            source: SourceId {
                session_id: "old-source".into(),
                path: provider_root.join("old-source.jsonl"),
                dev: 1,
                ino: 2,
            },
            binding_seq: 1,
            captured_at: now,
            ended_at: now,
            basis: EndBasis::ExplicitClear,
            context,
            successor: None,
        };
        codex_end::append(&root, &end).unwrap();
        tc::with_tmux_source_authority(pane, |authority| {
            let epoch = attempt::generation(channel, authority);
            assert!(attempt::install_ticket(
                channel,
                authority,
                epoch,
                &end.end_id
            ));
        });
        end
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::test_support::install_clear_end;
    use super::*;
    use crate::services::codex_tui::execution_end::attempt::{self, Phase};
    use crate::services::codex_tui::host_input::spy::{known, resolved};
    use crate::services::session_host::HostKind;
    use crate::services::tmux_common as tc;
    use crate::services::tui_o::store::codex_end;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(body: impl FnOnce()) -> String {
        let logs = Logs::default();
        let copy = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .with_writer(move || copy.clone())
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        tracing::subscriber::with_default(subscriber, body);
        String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn submitted_warm_capture_failure_warns_only_for_fresh_shadow_successor() {
        use crate::services::codex_tui::execution_end::producer::LaunchObservation;
        use crate::services::provider_teardown::tests::test_support::FakeTmux;
        use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext};
        let _root = crate::config::TestRuntimeRootGuard::new();
        attempt::reset_for_tests();
        let channel = 62_841_781;
        let pane = format!("a2-warm-capture-{}", uuid::Uuid::new_v4().simple());
        let nonce = "c".repeat(32);
        let fake = FakeTmux::install(&pane);
        fake.run_pane(std::process::id());
        std::fs::write(tc::session_temp_path(&pane, "spawn_nonce"), &nonce).unwrap();
        assert_eq!(
            binding_context::execution_context("codex", &nonce).unwrap_err(),
            "Unreadable"
        );
        let dispatch = attempt::begin(Some(channel), Some(&pane));
        let id = attempt::current(channel, &pane).unwrap();
        assert!(attempt::take_ticket(&id).is_none());
        let target = resolved(known(HostKind::Tmux));
        let rollout = tempfile::NamedTempFile::new().unwrap();
        let mut observation = None;
        let warm_logs = capture_logs(|| {
            observation = Some(LaunchObservation::new(Some(channel), &pane));
            assert!(
                settle_submit(
                    &target,
                    &pane,
                    CodexFollowupPromptSubmitOutcome::Submitted,
                    rollout.path(),
                    0,
                    "warm input",
                    (Some(channel), None),
                )
                .is_none()
            );
        });
        let calls = fake.take_calls();
        assert!(
            calls
                .iter()
                .filter(|call| call.starts_with("has-session"))
                .count()
                >= 2
        );
        assert!(calls.iter().any(|call| call.starts_with("display-message")));
        assert!(calls.iter().all(|call| !call.starts_with("kill-session")));
        assert_eq!(
            warm_logs
                .lines()
                .filter(|line| line.contains("WARN")
                    && line.contains("Codex replacement End not captured"))
                .count(),
            0,
            "Submitted warm resume must not warn about unused replacement evidence: {warm_logs}"
        );
        tc::with_tmux_source_authority(&pane, |authority| {
            assert_eq!(
                attempt::phase_under(&id, authority),
                Some(Phase::Bound { nonce })
            );
        });
        let root = crate::config::runtime_root().unwrap();
        let fresh = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: "d".repeat(32),
            tmux_session: pane.clone(),
            channel_id: Some(channel),
            owner_runtime_root: root.display().to_string(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(root.join("codex")),
            first_prompt_digest: None,
            source_policy: Some("shadow".into()),
            resume_start: None,
        };
        let fresh_logs = capture_logs(|| observation.as_mut().unwrap().prepare(&fresh));
        let warnings = fresh_logs
            .lines()
            .filter(|line| {
                line.contains("WARN") && line.contains("Codex replacement End not captured")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            warnings.len(),
            1,
            "fresh shadow must report its deferred capture failure exactly once: {fresh_logs}"
        );
        for field in [
            "pane=",
            "pid=",
            "pgid=",
            "attempt_state=",
            "reason=",
            "Unreadable",
        ] {
            assert!(warnings[0].contains(field), "missing {field}: {fresh_logs}");
        }
        assert!(codex_end::read(&root, channel).unwrap().is_empty());
        drop(dispatch);
    }

    #[test]
    fn submitted_warm_binds_current_nonce_and_consumes_clear_ticket_without_a_seal() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        attempt::reset_for_tests();
        let root = crate::config::runtime_root().unwrap();
        let channel = 62_841_780;
        let pane = format!("a2-warm-submit-{}", uuid::Uuid::new_v4().simple());
        let nonce = "b".repeat(32);
        let end = install_clear_end(channel, &pane);
        std::fs::write(tc::session_temp_path(&pane, "spawn_nonce"), &nonce).unwrap();
        let dispatch = attempt::begin(Some(channel), Some(&pane));
        let id = attempt::current(channel, &pane).unwrap();
        assert_eq!(attempt::ticket(channel, &pane), None);
        let rollout = tempfile::NamedTempFile::new().unwrap();
        let target = resolved(known(HostKind::Tmux));
        assert!(
            settle_submit(
                &target,
                &pane,
                CodexFollowupPromptSubmitOutcome::Submitted,
                rollout.path(),
                0,
                "warm input",
                (Some(channel), None),
            )
            .is_none()
        );
        tc::with_tmux_source_authority(&pane, |authority| {
            assert_eq!(
                attempt::phase_under(&id, authority),
                Some(Phase::Bound { nonce })
            );
        });
        assert!(attempt::take_ticket(&id).is_none());
        assert_eq!(codex_end::read(&root, channel).unwrap(), [end]);
        drop(dispatch);
        let _later_fresh = attempt::begin(Some(channel), Some(&pane));
        let later_id = attempt::current(channel, &pane).unwrap();
        assert!(attempt::take_ticket(&later_id).is_none());
    }
}

#[cfg(all(test, unix))]
#[path = "verified_warm_tests.rs"]
mod verified_warm_tests;
