use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::codex::CodexLaunchOptions;
use crate::services::provider::{CancelToken, ProviderKind, cancel_requested};

use super::host_input::{self, InputTarget};
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

const LAUNCH_OPTIONS_SCHEMA: &str = "agentdesk-codex-tui-launch-v1";

/// Process-sticky launch semantics in fingerprint order. `prompt` changes every
/// turn; `resume_session_id` is pinned by the rollout binding; and resumed turns
/// intentionally omit `developer_instructions` because the original thread
/// already owns them. Those three fields are therefore excluded.
fn launch_option_fields(options: &CodexLaunchOptions) -> Vec<(&'static str, String)> {
    let optional = |value: Option<String>| value.unwrap_or_else(|| "<none>".to_string());
    let mut fields = vec![
        ("model", optional(options.model.clone())),
        (
            "reasoning_effort",
            optional(options.reasoning_effort.clone()),
        ),
        (
            "compact_token_limit",
            optional(options.compact_token_limit.map(|value| value.to_string())),
        ),
        ("readonly_mode", options.readonly_mode.to_string()),
        (
            "fast_mode_enabled",
            optional(options.fast_mode_enabled.map(|value| value.to_string())),
        ),
        (
            "goals_enabled",
            optional(options.goals_enabled.map(|value| value.to_string())),
        ),
        ("cwd", optional(options.cwd.clone())),
    ];
    fields.extend(
        options
            .add_dirs
            .iter()
            .map(|add_dir| ("add_dir", add_dir.clone())),
    );
    let hooks_enabled = crate::services::codex::codex_direct_tui_hook_overrides_enabled();
    fields.push(("direct_tui_hooks", hooks_enabled.to_string()));
    fields
}

/// Stored next to the fingerprint so a later mismatch can name the fields that moved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct LaunchOptionsSnapshot {
    schema: String,
    fingerprint: String,
    fields: BTreeMap<String, Vec<String>>,
}

impl LaunchOptionsSnapshot {
    fn of(options: &CodexLaunchOptions) -> Self {
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, "schema", LAUNCH_OPTIONS_SCHEMA);
        let mut fields = BTreeMap::<String, Vec<String>>::new();
        for (label, value) in launch_option_fields(options) {
            hash_field(&mut hasher, label, &value);
            fields.entry(label.to_string()).or_default().push(value);
        }
        Self {
            schema: LAUNCH_OPTIONS_SCHEMA.to_string(),
            fingerprint: format!("{:x}", hasher.finalize()),
            fields,
        }
    }

    fn changes_from(&self, stored: &Self) -> Vec<LaunchOptionChange> {
        let names: BTreeSet<&String> = self.fields.keys().chain(stored.fields.keys()).collect();
        names
            .into_iter()
            .filter_map(|name| {
                let before = stored.fields.get(name).cloned().unwrap_or_default();
                let after = self.fields.get(name).cloned().unwrap_or_default();
                (before != after).then(|| LaunchOptionChange {
                    name: name.clone(),
                    before,
                    after,
                })
            })
            .collect()
    }
}

pub(crate) fn codex_tui_launch_options_fingerprint(options: &CodexLaunchOptions) -> String {
    LaunchOptionsSnapshot::of(options).fingerprint
}

/// Records the fingerprint the warm gate compares plus its per-field snapshot.
/// The snapshot is diagnostic only, so failing to write it never fails the launch.
pub(crate) fn write_codex_tui_launch_options_evidence(
    tmux_session_name: &str,
    options: &CodexLaunchOptions,
) -> Result<(), String> {
    let snapshot = LaunchOptionsSnapshot::of(options);
    super::session::write_codex_tui_launch_options_fingerprint(
        tmux_session_name,
        &snapshot.fingerprint,
    )?;
    let written = serde_json::to_string(&snapshot)
        .map_err(|error| error.to_string())
        .and_then(|json| {
            super::session::write_codex_tui_launch_options_snapshot(tmux_session_name, &json)
        });
    if let Err(error) = written {
        tracing::warn!(
            tmux_session_name,
            error,
            "Codex TUI launch-options snapshot not recorded; a later mismatch cannot name its fields"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchOptionChange {
    name: String,
    before: Vec<String>,
    after: Vec<String>,
}

/// Why the recorded launch options do not match this turn's.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchOptionsMismatch {
    Missing,
    Unreadable(String),
    /// The fingerprints differ; the error says why the stored snapshot could not name the fields.
    Changed(Result<Vec<LaunchOptionChange>, String>),
}

impl LaunchOptionsMismatch {
    fn kind(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unreadable(_) => "unreadable",
            Self::Changed(_) => "changed",
        }
    }

    /// Changed field names, or `None` when no snapshot describes the stored fingerprint.
    fn changed_field_names(&self) -> Option<Vec<&str>> {
        match self {
            Self::Changed(Ok(changes)) => {
                Some(changes.iter().map(|change| change.name.as_str()).collect())
            }
            _ => None,
        }
    }

    fn changed_fields_log(&self) -> String {
        match (self, self.changed_field_names()) {
            (_, Some(names)) => names.join(","),
            (Self::Changed(Err(_)), None) => "unknown".to_string(),
            _ => String::new(),
        }
    }

    fn detail_log(&self) -> String {
        match self {
            Self::Missing => "fingerprint missing".to_string(),
            Self::Unreadable(error) => format!("fingerprint unreadable: {error}"),
            Self::Changed(Err(why)) => why.clone(),
            Self::Changed(Ok(changes)) => changes
                .iter()
                .map(|change| {
                    format!(
                        "{}: {} -> {}",
                        change.name,
                        change.before.join("|"),
                        change.after.join("|")
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        }
    }
}

fn stored_launch_options_snapshot(
    tmux_session_name: &str,
    stored_fingerprint: &str,
) -> Result<LaunchOptionsSnapshot, String> {
    use super::session::LaunchEvidenceFile;
    let text = match super::session::read_codex_tui_launch_options_snapshot_file(tmux_session_name)
    {
        LaunchEvidenceFile::Missing => return Err("snapshot missing".to_string()),
        LaunchEvidenceFile::Unreadable(error) => {
            return Err(format!("snapshot unreadable: {error}"));
        }
        LaunchEvidenceFile::Present(text) => text,
    };
    let snapshot: LaunchOptionsSnapshot =
        serde_json::from_str(&text).map_err(|error| format!("snapshot unreadable: {error}"))?;
    // A snapshot left by another launch must not name fields for this fingerprint.
    if snapshot.fingerprint != stored_fingerprint {
        return Err("snapshot describes another fingerprint".to_string());
    }
    Ok(snapshot)
}

/// Compares this turn's launch options with the recorded fingerprint. Only the
/// fingerprint decides; the snapshot is read solely to name a mismatch.
fn judge_codex_tui_launch_options(
    tmux_session_name: &str,
    options: &CodexLaunchOptions,
) -> Result<(), LaunchOptionsMismatch> {
    use super::session::LaunchEvidenceFile;
    let current = LaunchOptionsSnapshot::of(options);
    let stored_fingerprint =
        match super::session::read_codex_tui_launch_options_fingerprint_file(tmux_session_name) {
            LaunchEvidenceFile::Missing => return Err(LaunchOptionsMismatch::Missing),
            LaunchEvidenceFile::Unreadable(error) => {
                return Err(LaunchOptionsMismatch::Unreadable(error));
            }
            LaunchEvidenceFile::Present(fingerprint) => fingerprint,
        };
    if stored_fingerprint == current.fingerprint {
        return Ok(());
    }
    Err(LaunchOptionsMismatch::Changed(
        stored_launch_options_snapshot(tmux_session_name, &stored_fingerprint)
            .map(|stored| current.changes_from(&stored)),
    ))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarmInputDecision {
    ReusePane,
    Fallback(CodexWarmFallbackReason),
}

fn decide_warm_input(snapshot: &PromptReadinessSnapshot) -> WarmInputDecision {
    if snapshot.tmux_pane_alive && snapshot.capture_available && snapshot.composer_marker_detected {
        if snapshot.prompt_draft_detected {
            WarmInputDecision::Fallback(CodexWarmFallbackReason::StrandedDraft)
        } else {
            WarmInputDecision::ReusePane
        }
    } else {
        WarmInputDecision::Fallback(CodexWarmFallbackReason::InputReadinessFailed)
    }
}

fn snapshot_has_stranded_draft(snapshot: &PromptReadinessSnapshot) -> bool {
    decide_warm_input(snapshot)
        == WarmInputDecision::Fallback(CodexWarmFallbackReason::StrandedDraft)
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

fn log_launch_options_fallback(tmux_session_name: &str, mismatch: &LaunchOptionsMismatch) {
    let changed_fields = mismatch.changed_fields_log();
    let launch_options_detail = mismatch.detail_log();
    tracing::warn!(
        tmux_session_name,
        fallback_reason = CodexWarmFallbackReason::LaunchOptionsChanged.reason_code(),
        detail = "eligibility gate rejected reuse",
        launch_options_mismatch = mismatch.kind(),
        changed_fields = changed_fields.as_str(),
        launch_options_detail = launch_options_detail.as_str(),
        "Codex TUI warm follow-up falling back to one cold resume launch"
    );
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
    if super::verified_hold::existing_incarnation(tmux_session_name) {
        return CodexWarmFollowupOutcome::Terminal(super::verified_hold::wait_for_cancel(
            tmux_session_name,
            cancel_token.as_ref(),
        ));
    }
    let target = InputTarget::legacy_tmux(tmux_session_name);
    warm_followup_on(
        &target,
        WarmFollowupRequest {
            selection,
            launch_options,
            force_fresh,
            session_exists,
            live_pane,
            prompt,
            sender,
            cancel_token,
            report_channel_id,
        },
    )
}

/// Turn inputs for [`warm_followup_on`], everything except the input target.
#[cfg(unix)]
struct WarmFollowupRequest<'a> {
    selection: &'a CodexTuiSessionSelection,
    launch_options: &'a CodexLaunchOptions,
    force_fresh: bool,
    session_exists: bool,
    live_pane: bool,
    prompt: &'a str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    report_channel_id: Option<u64>,
}

/// Warm follow-up on `target`: only a confirmed tmux session is reused, killed
/// or handed back for a relaunch.
#[cfg(unix)]
fn warm_followup_on(
    target: &InputTarget,
    request: WarmFollowupRequest<'_>,
) -> CodexWarmFollowupOutcome {
    let WarmFollowupRequest {
        selection,
        launch_options,
        force_fresh,
        session_exists,
        live_pane,
        prompt,
        sender,
        cancel_token,
        report_channel_id,
    } = request;
    let tmux_session_name = match target.tmux_session() {
        Ok(session) => session,
        Err(refusal) => {
            let run = host_input::InputRun::Refused(refusal);
            return CodexWarmFollowupOutcome::Terminal(Err(host_input::refusal_error(&run)));
        }
    };
    let marker = super::session::read_codex_tui_rollout_marker(tmux_session_name);
    let launch_options_judgement =
        judge_codex_tui_launch_options(tmux_session_name, launch_options);
    let eligibility = decide_warm_eligibility(WarmEligibilitySignals {
        force_fresh,
        session_exists,
        live_pane,
        resume_selected: selection.resume,
        runtime_kind_matches: crate::services::tmux_common::resolve_tmux_runtime_kind_marker(
            tmux_session_name,
        ) == Some(RuntimeHandoffKind::CodexTui),
        rollout_binding_matches: rollout_binding_matches(selection, marker.as_ref()),
        launch_options_match: launch_options_judgement.is_ok(),
    });
    match eligibility {
        WarmEligibilityDecision::LegacyPath => return CodexWarmFollowupOutcome::LegacyPath,
        WarmEligibilityDecision::Fallback(
            reason @ CodexWarmFallbackReason::LaunchOptionsChanged,
        ) => {
            if let Err(mismatch) = &launch_options_judgement {
                log_launch_options_fallback(tmux_session_name, mismatch);
            }
            return CodexWarmFollowupOutcome::Fallback(reason);
        }
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
        let reason = match decide_warm_input(&snapshot) {
            WarmInputDecision::Fallback(reason) => reason,
            WarmInputDecision::ReusePane => CodexWarmFallbackReason::InputReadinessFailed,
        };
        log_fallback(tmux_session_name, reason, &error);
        return CodexWarmFollowupOutcome::Fallback(reason);
    }
    if cancel_requested(cancel_token.as_deref()) {
        return CodexWarmFollowupOutcome::Terminal(Ok(()));
    }
    let ready_snapshot = super::input::prompt_readiness_snapshot(tmux_session_name);
    if let WarmInputDecision::Fallback(reason) = decide_warm_input(&ready_snapshot) {
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

    let outcome = super::input::submit_codex_followup_prompt(
        tmux_session_name,
        prompt,
        cancel_token.as_deref(),
    );
    if let Some(settled) = settle_submit(
        target,
        tmux_session_name,
        outcome,
        rollout_path,
        rollout_len_before_submit,
        prompt,
    ) {
        return settled;
    }

    let tail_result = super::rollout_tail::tail_warm_followup_rollout_for_tmux(
        rollout_path,
        start_offset,
        session_id,
        sender.clone(),
        cancel_token.clone(),
        || host_input::legacy_pane_alive(tmux_session_name),
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

/// Settles the one submit; `None` goes on to tail the rollout. A draft that stayed
/// in the composer is killed for a relaunch only on a confirmed tmux session.
#[cfg(unix)]
fn settle_submit(
    target: &InputTarget,
    tmux_session_name: &str,
    outcome: CodexFollowupPromptSubmitOutcome,
    rollout_path: &Path,
    rollout_len_before_submit: u64,
    prompt: &str,
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
                super::input::prompt_draft_matches(&first, prompt),
                super::input::prompt_draft_matches(&second, prompt),
                rollout_len_before_submit,
                rollout_len(),
            ) {
                return Some(CodexWarmFollowupOutcome::Terminal(Err(
                    "Codex TUI warm follow-up submit was unconfirmed; refusing replay".to_string(),
                )));
            }
            let reason = CodexWarmFallbackReason::SubmitFailed;
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
            CodexWarmFollowupOutcome::FallbackAfterPaneKill(reason)
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
    fn warm_input_decision_reuses_only_live_empty_composer() {
        let ready = PromptReadinessSnapshot {
            composer_marker_detected: true,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "idle Codex composer".to_string(),
        };
        assert_eq!(decide_warm_input(&ready), WarmInputDecision::ReusePane);

        for mutate in [
            |snapshot: &mut PromptReadinessSnapshot| snapshot.tmux_pane_alive = false,
            |snapshot: &mut PromptReadinessSnapshot| snapshot.capture_available = false,
            |snapshot: &mut PromptReadinessSnapshot| snapshot.composer_marker_detected = false,
        ] {
            let mut snapshot = ready.clone();
            mutate(&mut snapshot);
            assert_eq!(
                decide_warm_input(&snapshot),
                WarmInputDecision::Fallback(CodexWarmFallbackReason::InputReadinessFailed)
            );
        }

        let mut draft = ready;
        draft.prompt_draft_detected = true;
        assert_eq!(
            decide_warm_input(&draft),
            WarmInputDecision::Fallback(CodexWarmFallbackReason::StrandedDraft)
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

    /// Points session temp files at a fresh root with hooks pinned on, under the shared env lock.
    fn isolated_launch_evidence_env() -> (
        std::sync::MutexGuard<'static, ()>,
        tempfile::TempDir,
        [EnvRestore; 3],
    ) {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let restore = [
            EnvRestore::capture("AGENTDESK_ROOT_DIR"),
            EnvRestore::capture("HOSTNAME"),
            EnvRestore::capture("AGENTDESK_CODEX_DIRECT_TUI_HOOKS"),
        ];
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", dir.path());
            std::env::set_var("HOSTNAME", "codex-warm-evidence-host");
            std::env::remove_var("AGENTDESK_CODEX_DIRECT_TUI_HOOKS");
        }
        (lock, dir, restore)
    }

    fn launched_options(prompt: &str) -> CodexLaunchOptions {
        CodexLaunchOptions::new(prompt)
            .with_model(Some("gpt-6-luna"))
            .with_compact_token_limit(Some(217_600))
            .with_cwd(Some("/tmp/work"))
    }

    #[test]
    fn launch_options_mismatch_names_missing_and_each_changed_field() {
        let (_lock, _dir, _restore) = isolated_launch_evidence_env();
        let tmux = "AgentDesk-codex-warm-evidence";

        let missing = judge_codex_tui_launch_options(tmux, &launched_options("turn one"));
        assert_eq!(missing, Err(LaunchOptionsMismatch::Missing));
        super::super::session::write_codex_tui_launch_options_fingerprint(tmux, "").unwrap();
        let unreadable = judge_codex_tui_launch_options(tmux, &launched_options("turn one"));
        assert_eq!(unreadable.unwrap_err().kind(), "unreadable");

        write_codex_tui_launch_options_evidence(tmux, &launched_options("turn one")).unwrap();
        let next_turn = launched_options("turn two").with_resume_session_id(Some("session-one"));
        assert_eq!(judge_codex_tui_launch_options(tmux, &next_turn), Ok(()));

        for (changed, field) in [
            (next_turn.clone().with_model(Some("gpt-6-sol")), "model"),
            (
                next_turn.clone().with_compact_token_limit(Some(272_000)),
                "compact_token_limit",
            ),
        ] {
            let mismatch = judge_codex_tui_launch_options(tmux, &changed).unwrap_err();
            assert_eq!(mismatch.kind(), "changed");
            assert_eq!(mismatch.changed_field_names(), Some(vec![field]));
        }
    }

    #[test]
    fn fingerprint_without_matching_snapshot_still_decides_by_fingerprint_alone() {
        let (_lock, _dir, _restore) = isolated_launch_evidence_env();
        let tmux = "AgentDesk-codex-warm-legacy-evidence";
        let launched = launched_options("turn one");
        // A pane launched by a build that wrote only the fingerprint.
        super::super::session::write_codex_tui_launch_options_fingerprint(
            tmux,
            &codex_tui_launch_options_fingerprint(&launched),
        )
        .unwrap();
        let changed = launched.clone().with_model(Some("gpt-6-sol"));

        assert_eq!(judge_codex_tui_launch_options(tmux, &launched), Ok(()));
        assert_eq!(
            judge_codex_tui_launch_options(tmux, &changed),
            Err(LaunchOptionsMismatch::Changed(Err(
                "snapshot missing".to_string()
            )))
        );

        // A snapshot describing some other launch must not name fields for this one.
        let other = launched.clone().with_cwd(Some("/tmp/other"));
        super::super::session::write_codex_tui_launch_options_snapshot(
            tmux,
            &serde_json::to_string(&LaunchOptionsSnapshot::of(&other)).unwrap(),
        )
        .unwrap();
        assert_eq!(judge_codex_tui_launch_options(tmux, &launched), Ok(()));
        assert_eq!(
            judge_codex_tui_launch_options(tmux, &changed)
                .unwrap_err()
                .changed_field_names(),
            None
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

    #[cfg(unix)]
    #[test]
    fn warm_followup_kills_and_relaunches_only_a_confirmed_tmux_session() {
        use crate::services::codex_tui::host_input::spy::{
            SpyGuard, SpyState, known, non_tmux_targets, resolved,
        };
        use crate::services::codex_tui::host_input::{InputRefusal, InputRun, StopCause};
        use crate::services::session_host::HostKind;

        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        std::fs::write(&rollout, "{}\n").unwrap();
        let len = std::fs::metadata(&rollout).unwrap().len();
        let prompt = "keep this draft";
        let draft = PromptReadinessSnapshot {
            composer_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: format!(
                "old output\n\
                 ╭────────────────────────────────────────╮\n\
                 │ {prompt} ▌                              │\n\
                 ╰────────────────────────────────────────╯\n\
                   Esc to interrupt   Ctrl+J newline   ⏎ send"
            ),
        };
        let stranded = || CodexFollowupPromptSubmitOutcome::RetrySafeDraft {
            first: draft.clone(),
            second: draft.clone(),
        };
        let selection = CodexTuiSessionSelection {
            requested_session_id: Some("session-one".to_string()),
            selected_session_id: Some("session-one".to_string()),
            resume: true,
            reason: "test".to_string(),
            rollout_path: Some(rollout.clone()),
            rollout_start_offset: Some(0),
            candidate_count: 1,
        };
        let options = CodexLaunchOptions::new(prompt);
        let pinned = || SpyState {
            pane_pid: Some(4242),
            ..SpyState::default()
        };
        let session = "AgentDesk-codex-p6b";

        for (target, refusal) in non_tmux_targets() {
            let guard = SpyGuard::install(pinned());
            let (sender, _receiver) = std::sync::mpsc::channel();
            let entry = warm_followup_on(
                &target,
                WarmFollowupRequest {
                    selection: &selection,
                    launch_options: &options,
                    force_fresh: false,
                    session_exists: true,
                    live_pane: true,
                    prompt,
                    sender,
                    cancel_token: None,
                    report_channel_id: None,
                },
            );
            assert!(
                matches!(entry, CodexWarmFollowupOutcome::Terminal(Err(_))),
                "{refusal:?}"
            );
            let settled = settle_submit(&target, session, stranded(), &rollout, len, prompt);
            assert!(
                matches!(settled, Some(CodexWarmFollowupOutcome::Terminal(Err(_)))),
                "{refusal:?}: no relaunch"
            );
            assert!(
                guard.calls().is_empty(),
                "{refusal:?}: no read, key or kill"
            );
        }

        // Positive control: a confirmed tmux draft is killed in the legacy order, then relaunched.
        let tmux = resolved(known(HostKind::Tmux));
        let guard = SpyGuard::install(pinned());
        let settled = settle_submit(&tmux, session, stranded(), &rollout, len, prompt);
        assert!(matches!(
            settled,
            Some(CodexWarmFollowupOutcome::FallbackAfterPaneKill(
                CodexWarmFallbackReason::SubmitFailed
            ))
        ));
        let reason = CodexWarmFallbackReason::SubmitFailed.reason_text();
        let kill_session = format!("kill_session:{reason}");
        assert_eq!(
            guard.calls(),
            ["pane_pid", "kill_tree", kill_session.as_str(), "stopped"]
        );
        drop(guard);

        // A gate refusal ends the attempt on tmux too: no kill and no cold relaunch.
        let guard = SpyGuard::install(pinned());
        let run = InputRun::Indeterminate {
            confirmed: 1,
            cause: StopCause::Refused(InputRefusal::IdentityMismatch),
        };
        let refused = CodexFollowupPromptSubmitOutcome::Refused { run };
        let settled = settle_submit(&tmux, session, refused, &rollout, len, prompt);
        assert!(matches!(
            settled,
            Some(CodexWarmFollowupOutcome::Terminal(Err(_)))
        ));
        assert!(guard.calls().is_empty());
    }
}
