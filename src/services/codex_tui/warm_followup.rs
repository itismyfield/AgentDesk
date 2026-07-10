use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::codex::CodexLaunchOptions;
use crate::services::provider::{CancelToken, ProviderKind, cancel_requested};

use super::input::{
    CodexFollowupPromptSubmitOutcome, PromptReadinessKind, PromptReadinessSnapshot,
};
use super::session::{CodexTuiRolloutMarker, CodexTuiSessionSelection};

const WARM_FOLLOWUP_ENV: &str = "AGENTDESK_CODEX_TUI_WARM_FOLLOWUP";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexWarmFallbackReason {
    RuntimeKindMismatch,
    RolloutBindingMismatch,
    LaunchOptionsChanged,
    InputReadinessFailed,
    StrandedDraft,
    SubmitFailed,
}

impl CodexWarmFallbackReason {
    pub(crate) fn reason_code(self) -> &'static str {
        match self {
            Self::RuntimeKindMismatch => "runtime_kind_mismatch",
            Self::RolloutBindingMismatch => "rollout_binding_mismatch",
            Self::LaunchOptionsChanged => "launch_options_changed",
            Self::InputReadinessFailed => "input_readiness_failed",
            Self::StrandedDraft => "stranded_draft",
            Self::SubmitFailed => "submit_failed",
        }
    }

    pub(crate) fn reason_text(self) -> &'static str {
        match self {
            Self::RuntimeKindMismatch => "Codex TUI warm follow-up runtime kind mismatch",
            Self::RolloutBindingMismatch => "Codex TUI warm follow-up rollout binding mismatch",
            Self::LaunchOptionsChanged => "Codex TUI warm follow-up launch options changed",
            Self::InputReadinessFailed => "Codex TUI warm follow-up input readiness failed",
            Self::StrandedDraft => "Codex TUI warm follow-up found a stranded prompt draft",
            Self::SubmitFailed => "Codex TUI warm follow-up submit failed with draft preserved",
        }
    }
}

pub(crate) enum CodexWarmFollowupOutcome {
    Terminal(Result<(), String>),
    Fallback(CodexWarmFallbackReason),
    FallbackAfterPaneKill(CodexWarmFallbackReason),
    LegacyPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WarmEligibilitySignals {
    force_fresh: bool,
    session_exists: bool,
    live_pane: bool,
    resume_selected: bool,
    runtime_kind_matches: bool,
    rollout_binding_matches: bool,
    launch_options_match: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarmEligibilityDecision {
    Eligible,
    Fallback(CodexWarmFallbackReason),
    LegacyPath,
}

fn decide_warm_eligibility(signals: WarmEligibilitySignals) -> WarmEligibilityDecision {
    if signals.force_fresh
        || !signals.session_exists
        || !signals.live_pane
        || !signals.resume_selected
    {
        return WarmEligibilityDecision::LegacyPath;
    }
    if !signals.runtime_kind_matches {
        return WarmEligibilityDecision::Fallback(CodexWarmFallbackReason::RuntimeKindMismatch);
    }
    if !signals.rollout_binding_matches {
        return WarmEligibilityDecision::Fallback(CodexWarmFallbackReason::RolloutBindingMismatch);
    }
    if !signals.launch_options_match {
        return WarmEligibilityDecision::Fallback(CodexWarmFallbackReason::LaunchOptionsChanged);
    }
    WarmEligibilityDecision::Eligible
}

pub(crate) fn codex_tui_warm_followup_enabled() -> bool {
    std::env::var(WARM_FOLLOWUP_ENV)
        .ok()
        .is_none_or(|value| value.trim() != "0")
}

fn hash_field(hasher: &mut Sha256, label: &str, value: &str) {
    hasher.update(label.len().to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update(value.len().to_le_bytes());
    hasher.update(value.as_bytes());
}

fn hash_optional_field(hasher: &mut Sha256, label: &str, value: Option<&str>) {
    hash_field(hasher, label, value.unwrap_or("<none>"));
}

/// Fingerprint process-sticky launch semantics. `prompt` changes every turn;
/// `resume_session_id` is pinned by the rollout binding; and resumed turns
/// intentionally omit `developer_instructions` because the original thread
/// already owns them. Those three fields are therefore excluded.
pub(crate) fn codex_tui_launch_options_fingerprint(options: &CodexLaunchOptions) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "schema", "agentdesk-codex-tui-launch-v1");
    hash_optional_field(&mut hasher, "model", options.model.as_deref());
    hash_optional_field(
        &mut hasher,
        "reasoning_effort",
        options.reasoning_effort.as_deref(),
    );
    hash_optional_field(
        &mut hasher,
        "compact_token_limit",
        options
            .compact_token_limit
            .map(|value| value.to_string())
            .as_deref(),
    );
    hash_field(
        &mut hasher,
        "readonly_mode",
        if options.readonly_mode {
            "true"
        } else {
            "false"
        },
    );
    hash_optional_field(
        &mut hasher,
        "fast_mode_enabled",
        options
            .fast_mode_enabled
            .map(|value| value.to_string())
            .as_deref(),
    );
    hash_optional_field(
        &mut hasher,
        "goals_enabled",
        options
            .goals_enabled
            .map(|value| value.to_string())
            .as_deref(),
    );
    hash_optional_field(&mut hasher, "cwd", options.cwd.as_deref());
    for add_dir in &options.add_dirs {
        hash_field(&mut hasher, "add_dir", add_dir);
    }
    let hooks_enabled = crate::services::codex::codex_direct_tui_hook_overrides_enabled();
    hash_field(
        &mut hasher,
        "direct_tui_hooks",
        if hooks_enabled { "true" } else { "false" },
    );
    format!("{:x}", hasher.finalize())
}

fn paths_match(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn rollout_binding_matches(
    selection: &CodexTuiSessionSelection,
    marker: Option<&CodexTuiRolloutMarker>,
) -> bool {
    let (Some(marker), Some(selected_path), Some(selected_session_id)) = (
        marker,
        selection.rollout_path.as_deref(),
        selection.selected_session_id.as_deref(),
    ) else {
        return false;
    };
    marker.session_id.as_deref() == Some(selected_session_id)
        && paths_match(&marker.rollout_path, selected_path)
}

fn snapshot_is_strictly_input_ready(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.tmux_pane_alive
        && snapshot.capture_available
        && snapshot.composer_marker_detected
        && !snapshot.prompt_draft_detected
}

fn snapshot_has_stranded_draft(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.tmux_pane_alive
        && snapshot.capture_available
        && snapshot.composer_marker_detected
        && snapshot.prompt_draft_detected
}

fn submit_failure_allows_fallback(
    first_draft_matches: bool,
    second_draft_matches: bool,
    rollout_len_before_submit: u64,
    rollout_len_after_submit: Option<u64>,
) -> bool {
    first_draft_matches
        && second_draft_matches
        && rollout_len_after_submit == Some(rollout_len_before_submit)
}

fn pre_enter_failure_allows_fallback(
    rollout_len_before_submit: u64,
    rollout_len_after_submit: Option<u64>,
) -> bool {
    rollout_len_after_submit == Some(rollout_len_before_submit)
}

fn log_fallback(tmux_session_name: &str, reason: CodexWarmFallbackReason, detail: &str) {
    tracing::warn!(
        tmux_session_name,
        fallback_reason = reason.reason_code(),
        detail,
        "Codex TUI warm follow-up falling back to one cold resume launch"
    );
}

#[cfg(unix)]
fn kill_pane_and_confirm_stopped(
    tmux_session_name: &str,
    reason: CodexWarmFallbackReason,
) -> Result<(), String> {
    let pane_pid =
        crate::services::platform::tmux::pane_pid(tmux_session_name).ok_or_else(|| {
            "Codex TUI warm follow-up could not pin the pane PID before kill".to_string()
        })?;
    let pane_identity = crate::services::process::ProcessIdentity::capture(pane_pid);
    let process_tree_kill_started =
        crate::services::process::kill_pid_tree_if_identity_matches(pane_pid, pane_identity);
    let kill_succeeded =
        crate::services::platform::tmux::kill_session(tmux_session_name, reason.reason_text());
    if !process_tree_kill_started || !kill_succeeded {
        tracing::warn!(
            tmux_session_name,
            pane_pid,
            process_tree_kill_started,
            kill_succeeded,
            "Codex TUI warm follow-up kill command was incomplete; requiring independent pane, PID, and process-group death proof"
        );
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let pane_stopped = matches!(
            crate::services::tmux_diagnostics::tmux_session_pane_liveness(tmux_session_name),
            crate::services::platform::tmux::PaneLiveness::DeadOrAbsent
        );
        let process_stopped = matches!(
            pane_identity.probe(pane_pid),
            crate::services::process::ProcessIdentityProbe::GoneOrReused
        );
        let process_group_stopped = matches!(
            crate::services::process::process_group_probe(pane_pid),
            crate::services::process::ProcessGroupProbe::Gone
        );
        if pane_stopped && process_stopped && process_group_stopped {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Err("Codex TUI warm follow-up could not prove pane, process, and process-group termination after fallback kill barrier".to_string())
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_codex_tui_warm_followup(
    selection: &CodexTuiSessionSelection,
    launch_options: &CodexLaunchOptions,
    force_fresh: bool,
    session_exists: bool,
    live_pane: bool,
    prompt: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
) -> CodexWarmFollowupOutcome {
    let marker = super::session::read_codex_tui_rollout_marker(tmux_session_name);
    let fingerprint = codex_tui_launch_options_fingerprint(launch_options);
    let eligibility = decide_warm_eligibility(WarmEligibilitySignals {
        force_fresh,
        session_exists,
        live_pane,
        resume_selected: selection.resume,
        runtime_kind_matches: crate::services::tmux_common::resolve_tmux_runtime_kind_marker(
            tmux_session_name,
        ) == Some(RuntimeHandoffKind::CodexTui),
        rollout_binding_matches: rollout_binding_matches(selection, marker.as_ref()),
        launch_options_match: super::session::read_codex_tui_launch_options_fingerprint(
            tmux_session_name,
        )
        .as_deref()
            == Some(fingerprint.as_str()),
    });
    match eligibility {
        WarmEligibilityDecision::LegacyPath => return CodexWarmFollowupOutcome::LegacyPath,
        WarmEligibilityDecision::Fallback(reason) => {
            log_fallback(tmux_session_name, reason, "eligibility gate rejected reuse");
            return CodexWarmFollowupOutcome::Fallback(reason);
        }
        WarmEligibilityDecision::Eligible => {}
    }

    crate::services::codex::wire_cancel_token_to_tmux_session(
        cancel_token.as_ref(),
        tmux_session_name,
    );
    let initial_snapshot = super::input::prompt_readiness_snapshot(tmux_session_name);
    if snapshot_has_stranded_draft(&initial_snapshot) {
        let reason = CodexWarmFallbackReason::StrandedDraft;
        log_fallback(
            tmux_session_name,
            reason,
            "draft visible before readiness wait",
        );
        return CodexWarmFollowupOutcome::Fallback(reason);
    }
    if let Err(error) = super::input::wait_until_codex_tui_input_ready(
        tmux_session_name,
        PromptReadinessKind::Followup,
        cancel_token.as_ref(),
    ) {
        if super::input::is_prompt_ready_cancelled_error(&error) {
            return CodexWarmFollowupOutcome::Terminal(Ok(()));
        }
        let snapshot = super::input::prompt_readiness_snapshot(tmux_session_name);
        let reason = if snapshot_has_stranded_draft(&snapshot) {
            CodexWarmFallbackReason::StrandedDraft
        } else {
            CodexWarmFallbackReason::InputReadinessFailed
        };
        log_fallback(tmux_session_name, reason, &error);
        return CodexWarmFollowupOutcome::Fallback(reason);
    }
    if cancel_requested(cancel_token.as_deref()) {
        return CodexWarmFollowupOutcome::Terminal(Ok(()));
    }
    let ready_snapshot = super::input::prompt_readiness_snapshot(tmux_session_name);
    if !snapshot_is_strictly_input_ready(&ready_snapshot) {
        let reason = if snapshot_has_stranded_draft(&ready_snapshot) {
            CodexWarmFallbackReason::StrandedDraft
        } else {
            CodexWarmFallbackReason::InputReadinessFailed
        };
        log_fallback(
            tmux_session_name,
            reason,
            "strict post-wait pane snapshot rejected reuse",
        );
        return CodexWarmFollowupOutcome::Fallback(reason);
    }

    let rollout_path = selection
        .rollout_path
        .as_deref()
        .expect("eligible warm follow-up has a rollout path");
    let session_id = selection
        .selected_session_id
        .as_deref()
        .expect("eligible warm follow-up has a session id");
    let rollout_len_before_submit = match std::fs::metadata(rollout_path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            let reason = CodexWarmFallbackReason::RolloutBindingMismatch;
            log_fallback(tmux_session_name, reason, &error.to_string());
            return CodexWarmFollowupOutcome::Fallback(reason);
        }
    };
    let start_offset = rollout_len_before_submit.max(
        marker
            .as_ref()
            .and_then(|marker| marker.rollout_start_offset)
            .unwrap_or(0),
    );
    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        ProviderKind::Codex.as_str(),
        tmux_session_name,
        prompt,
    );
    if let Some(channel_id) = report_channel_id {
        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux_session_name, channel_id);
    }

    match super::input::submit_codex_followup_prompt(
        tmux_session_name,
        prompt,
        cancel_token.as_deref(),
    ) {
        CodexFollowupPromptSubmitOutcome::Submitted => {}
        CodexFollowupPromptSubmitOutcome::NotSubmitted { error } => {
            let rollout_len_after_submit = std::fs::metadata(rollout_path)
                .ok()
                .map(|metadata| metadata.len());
            if pre_enter_failure_allows_fallback(
                rollout_len_before_submit,
                rollout_len_after_submit,
            ) {
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
                return CodexWarmFollowupOutcome::Fallback(reason);
            }
            return CodexWarmFollowupOutcome::Terminal(Err(format!(
                "Codex TUI warm follow-up failed before Enter but rollout advanced; refusing replay: {error}"
            )));
        }
        CodexFollowupPromptSubmitOutcome::Cancelled => {
            return CodexWarmFollowupOutcome::Terminal(Ok(()));
        }
        CodexFollowupPromptSubmitOutcome::RetrySafeDraft { first, second } => {
            let rollout_len_after_submit = std::fs::metadata(rollout_path)
                .ok()
                .map(|metadata| metadata.len());
            if submit_failure_allows_fallback(
                super::input::prompt_draft_matches(&first, prompt),
                super::input::prompt_draft_matches(&second, prompt),
                rollout_len_before_submit,
                rollout_len_after_submit,
            ) {
                let reason = CodexWarmFallbackReason::SubmitFailed;
                if let Err(error) = kill_pane_and_confirm_stopped(tmux_session_name, reason) {
                    return CodexWarmFollowupOutcome::Terminal(Err(error));
                }
                let rollout_len_after_kill = std::fs::metadata(rollout_path)
                    .ok()
                    .map(|metadata| metadata.len());
                if rollout_len_after_kill != Some(rollout_len_before_submit) {
                    return CodexWarmFollowupOutcome::Terminal(Err(
                        "Codex TUI warm follow-up rollout advanced across the pane-kill barrier; refusing replay"
                            .to_string(),
                    ));
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
                return CodexWarmFollowupOutcome::FallbackAfterPaneKill(reason);
            }
            return CodexWarmFollowupOutcome::Terminal(Err(
                "Codex TUI warm follow-up submit was unconfirmed; refusing replay".to_string(),
            ));
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
            return CodexWarmFollowupOutcome::Terminal(Err(error));
        }
    }

    let tail_result = super::rollout_tail::tail_warm_followup_rollout_for_tmux(
        rollout_path,
        start_offset,
        session_id,
        sender.clone(),
        cancel_token.clone(),
        || crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name),
        tmux_session_name,
        prompt,
    );
    let tail_result = match tail_result {
        Ok(result) => result,
        Err(_) if cancel_requested(cancel_token.as_deref()) => {
            return CodexWarmFollowupOutcome::Terminal(Ok(()));
        }
        Err(error) => return CodexWarmFollowupOutcome::Terminal(Err(error)),
    };
    CodexWarmFollowupOutcome::Terminal(crate::services::codex::emit_codex_tui_post_tail_handoff(
        tail_result,
        sender,
        cancel_token,
        tmux_session_name,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvRestore {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                previous: std::env::var_os(key),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn eligible_signals() -> WarmEligibilitySignals {
        WarmEligibilitySignals {
            force_fresh: false,
            session_exists: true,
            live_pane: true,
            resume_selected: true,
            runtime_kind_matches: true,
            rollout_binding_matches: true,
            launch_options_match: true,
        }
    }

    #[test]
    fn eligibility_requires_all_reuse_axes() {
        assert_eq!(
            decide_warm_eligibility(eligible_signals()),
            WarmEligibilityDecision::Eligible
        );
        for mutate in [
            |signals: &mut WarmEligibilitySignals| signals.force_fresh = true,
            |signals: &mut WarmEligibilitySignals| signals.session_exists = false,
            |signals: &mut WarmEligibilitySignals| signals.live_pane = false,
            |signals: &mut WarmEligibilitySignals| signals.resume_selected = false,
        ] {
            let mut signals = eligible_signals();
            mutate(&mut signals);
            assert_eq!(
                decide_warm_eligibility(signals),
                WarmEligibilityDecision::LegacyPath
            );
        }

        let mut runtime = eligible_signals();
        runtime.runtime_kind_matches = false;
        assert_eq!(
            decide_warm_eligibility(runtime),
            WarmEligibilityDecision::Fallback(CodexWarmFallbackReason::RuntimeKindMismatch)
        );
        let mut rollout = eligible_signals();
        rollout.rollout_binding_matches = false;
        assert_eq!(
            decide_warm_eligibility(rollout),
            WarmEligibilityDecision::Fallback(CodexWarmFallbackReason::RolloutBindingMismatch)
        );
        let mut options = eligible_signals();
        options.launch_options_match = false;
        assert_eq!(
            decide_warm_eligibility(options),
            WarmEligibilityDecision::Fallback(CodexWarmFallbackReason::LaunchOptionsChanged)
        );
    }

    #[test]
    fn fallback_reason_codes_are_stable_and_complete() {
        assert_eq!(
            [
                CodexWarmFallbackReason::RuntimeKindMismatch,
                CodexWarmFallbackReason::RolloutBindingMismatch,
                CodexWarmFallbackReason::LaunchOptionsChanged,
                CodexWarmFallbackReason::InputReadinessFailed,
                CodexWarmFallbackReason::StrandedDraft,
                CodexWarmFallbackReason::SubmitFailed,
            ]
            .map(CodexWarmFallbackReason::reason_code),
            [
                "runtime_kind_mismatch",
                "rollout_binding_mismatch",
                "launch_options_changed",
                "input_readiness_failed",
                "stranded_draft",
                "submit_failed",
            ]
        );
    }

    #[test]
    fn rollout_binding_requires_same_canonical_path_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let rollout_path = dir.path().join("rollout.jsonl");
        std::fs::write(&rollout_path, "").unwrap();
        let selection = CodexTuiSessionSelection {
            requested_session_id: Some("session-one".to_string()),
            selected_session_id: Some("session-one".to_string()),
            resume: true,
            reason: "test".to_string(),
            rollout_path: Some(rollout_path.clone()),
            rollout_start_offset: Some(0),
            candidate_count: 1,
        };
        let marker = CodexTuiRolloutMarker {
            rollout_path: rollout_path.clone(),
            session_id: Some("session-one".to_string()),
            rollout_start_offset: Some(0),
        };

        assert!(rollout_binding_matches(&selection, Some(&marker)));

        let mut wrong_session = marker.clone();
        wrong_session.session_id = Some("session-two".to_string());
        assert!(!rollout_binding_matches(&selection, Some(&wrong_session)));

        let mut wrong_path = marker;
        wrong_path.rollout_path = dir.path().join("other.jsonl");
        assert!(!rollout_binding_matches(&selection, Some(&wrong_path)));
        assert!(!rollout_binding_matches(&selection, None));
    }

    #[test]
    fn launch_fingerprint_ignores_per_turn_fields_but_detects_material_change() {
        let base = CodexLaunchOptions::new("turn one")
            .with_resume_session_id(Some("session-one"))
            .with_developer_instructions(Some("already sticky"))
            .with_model(Some("gpt-5.5"))
            .with_cwd(Some("/tmp/work"));
        let next_turn = CodexLaunchOptions::new("turn two")
            .with_resume_session_id(Some("session-two"))
            .with_model(Some("gpt-5.5"))
            .with_cwd(Some("/tmp/work"));
        let changed = next_turn.clone().with_model(Some("gpt-5.6"));

        assert_eq!(
            codex_tui_launch_options_fingerprint(&base),
            codex_tui_launch_options_fingerprint(&next_turn)
        );
        assert_ne!(
            codex_tui_launch_options_fingerprint(&base),
            codex_tui_launch_options_fingerprint(&changed)
        );
    }

    #[test]
    fn submit_fallback_requires_persisted_draft_and_zero_rollout_advance() {
        assert!(submit_failure_allows_fallback(true, true, 100, Some(100)));
        assert!(!submit_failure_allows_fallback(false, true, 100, Some(100)));
        assert!(!submit_failure_allows_fallback(true, false, 100, Some(100)));
        assert!(!submit_failure_allows_fallback(true, true, 100, Some(101)));
        assert!(!submit_failure_allows_fallback(true, true, 100, None));
        assert!(pre_enter_failure_allows_fallback(100, Some(100)));
        assert!(!pre_enter_failure_allows_fallback(100, Some(101)));
        assert!(!pre_enter_failure_allows_fallback(100, None));
    }

    #[test]
    fn kill_switch_defaults_on_and_zero_disables() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _restore = EnvRestore::capture(WARM_FOLLOWUP_ENV);
        unsafe { std::env::remove_var(WARM_FOLLOWUP_ENV) };
        assert!(codex_tui_warm_followup_enabled());
        unsafe { std::env::set_var(WARM_FOLLOWUP_ENV, "0") };
        assert!(!codex_tui_warm_followup_enabled());
        unsafe { std::env::set_var(WARM_FOLLOWUP_ENV, "false") };
        assert!(codex_tui_warm_followup_enabled());
    }
}
