use super::RestartMetadata;
use super::proof::{RestartTerminalProof, terminal_proof};
use crate::domain::file_sync::fsync_parent_dir;
use crate::domain::restart_report::*;
use crate::services::durable_publish::atomic_write;
use crate::services::process_generation::process_generation;
use crate::services::provider::ProviderKind;
use crate::services::runtime_paths;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn restart_reports_root() -> Option<PathBuf> {
    runtime_paths::restart_reports_root()
}
fn restart_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}
pub(crate) fn restart_report_path(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    restart_provider_dir(root, provider).join(format!("{channel_id}.json"))
}
pub(crate) fn restart_attempt_report_path(
    root: &Path,
    context: &RestartReportContext,
    attempt: &RestartAttempt,
) -> PathBuf {
    restart_provider_dir(root, &context.provider).join(format!(
        "{}.{}.json",
        context.channel_id,
        attempt.as_str()
    ))
}
pub(crate) fn lock_restart_report_path(
    path: &Path,
) -> Result<crate::utils::record_path_lock::RecordPathLock, String> {
    crate::utils::record_path_lock::lock_record_path(path)
}
pub(crate) fn save_report(path: &Path, report: &RestartCompletionReport) -> Result<(), String> {
    let json = serde_json::to_string_pretty(report).map_err(|error| error.to_string())?;
    atomic_write(path, &json)?;
    fsync_parent_dir(path).map_err(|error| error.to_string())
}
#[derive(Deserialize)]
#[rustfmt::skip]
pub(crate) struct LegacyRestartReport {
    version: u32, provider: String, channel_id: u64, status: String, completed_at: String,
    #[serde(default)] current_msg_id: Option<u64>, #[serde(default)] channel_name: Option<String>, #[serde(default)] user_msg_id: Option<u64>, #[serde(default)] generation: u64, #[serde(default)] doctor_summary: Option<serde_json::Value>,
}
#[rustfmt::skip]
pub(crate) fn load_report(path: &Path) -> Result<Option<RestartCompletionReport>, String> {
    let raw = match fs::read_to_string(path) { Ok(raw) => raw, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(error) => return Err(error.to_string()) };
    if let Ok(report) = serde_json::from_str::<RestartCompletionReport>(&raw) && report.version == RESTART_REPORT_VERSION { return Ok(Some(report)); }
    if let Ok(old) = serde_json::from_str::<LegacyRestartReport>(&raw) && old.version == 1 {
        if old.status == "skipped" { fs::remove_file(path).map_err(|e| e.to_string())?; return Ok(None); }
        let state = match old.status.as_str() { "ok" => RestartReportState::Succeeded, "pending" | "sigterm" => RestartReportState::Awaiting, "rolled_back" => RestartReportState::Failed(RestartFailure::RolledBack), _ => RestartReportState::Failed(RestartFailure::Unverified) };
        let report = RestartCompletionReport { version: RESTART_REPORT_VERSION, provider: old.provider, channel_id: old.channel_id, attempt: RestartAttempt::new(), predecessor: None, state, created_at: old.completed_at, current_msg_id: old.current_msg_id, channel_name: old.channel_name, user_msg_id: old.user_msg_id, generation: old.generation, doctor_summary: old.doctor_summary };
        save_report(path, &report)?; return Ok(Some(report));
    }
    let quarantine = path.with_extension("json.invalid"); let _ = fs::remove_file(&quarantine);
    if fs::rename(path, &quarantine).is_ok() { tracing::warn!("quarantined corrupt or unsupported restart report {}", path.display()); }
    Ok(None)
}
pub(crate) fn announce_restart(
    context: &RestartReportContext,
    metadata: &impl Fn() -> RestartMetadata,
) -> Result<RestartAttempt, RestartReportError> {
    announce_restart_in_root(
        &restart_reports_root().ok_or_else(|| "restart report unavailable".to_string())?,
        context,
        metadata,
    )
}
pub(crate) fn prepare_restart_handoff(
    context: &RestartReportContext,
    claim: Option<&RestartAttempt>,
    metadata: &impl Fn() -> RestartMetadata,
) -> Result<RestartAttempt, RestartReportError> {
    let (Some(reports), Some(runtime)) = (restart_reports_root(), runtime_paths::agentdesk_root())
    else {
        return Err("restart report unavailable".to_string());
    };
    prepare_restart_handoff_in_root(&reports, &runtime, context, claim, metadata)
}
pub(crate) fn prepare_restart_handoff_in_root(
    reports: &Path,
    runtime: &Path,
    context: &RestartReportContext,
    claim: Option<&RestartAttempt>,
    metadata: &impl Fn() -> RestartMetadata,
) -> Result<RestartAttempt, RestartReportError> {
    let attempt = RestartAttempt::new();
    let canonical = restart_report_path(reports, &context.provider, context.channel_id);
    let _lock = lock_restart_report_path(&canonical)?;
    let prior = load_report(&canonical).ok().flatten();
    let adopted = claim.and_then(|claim| {
        prior.as_ref().filter(|report| {
            report.attempt == *claim
                && matches!(report.state, RestartReportState::Awaiting)
                && report.generation == process_generation()
                && restart_terminal_state(runtime, claim).is_none()
        })
    });
    write_restart_report(
        &restart_attempt_report_path(reports, context, &attempt),
        context,
        adopted,
        &attempt,
        adopted.map(|report| report.attempt.clone()),
        metadata,
    )
}
pub(crate) fn clear_restart_handoff(context: &RestartReportContext, attempt: &RestartAttempt) {
    let Some(root) = restart_reports_root() else {
        return;
    };
    let _ = compare_delete(
        &restart_attempt_report_path(&root, context, attempt),
        attempt,
    );
}
pub(crate) fn announce_restart_in_root(
    root: &Path,
    context: &RestartReportContext,
    metadata: &impl Fn() -> RestartMetadata,
) -> Result<RestartAttempt, RestartReportError> {
    let attempt = RestartAttempt::new();
    let path = restart_report_path(root, &context.provider, context.channel_id);
    let _lock = lock_restart_report_path(&path)?;
    let prior = load_report(&path)?;
    if let Some(report) = prior
        .as_ref()
        .filter(|report| !matches!(report.state, RestartReportState::Awaiting))
    {
        let archived = restart_attempt_report_path(root, context, &report.attempt);
        if !archived.exists() {
            save_report(&archived, report)?;
        }
    }
    write_restart_report(&path, context, prior.as_ref(), &attempt, None, metadata)
}
fn write_restart_report(
    path: &Path,
    context: &RestartReportContext,
    prior: Option<&RestartCompletionReport>,
    attempt: &RestartAttempt,
    predecessor: Option<RestartAttempt>,
    metadata: &impl Fn() -> RestartMetadata,
) -> Result<RestartAttempt, RestartReportError> {
    let created_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let metadata = metadata();
    let report = RestartCompletionReport {
        version: RESTART_REPORT_VERSION,
        provider: context.provider.as_str().to_string(),
        channel_id: context.channel_id,
        attempt: attempt.clone(),
        predecessor,
        state: RestartReportState::Awaiting,
        created_at,
        current_msg_id: context
            .current_msg_id
            .or_else(|| prior.and_then(|report| report.current_msg_id)),
        channel_name: context
            .channel_name
            .clone()
            .or_else(|| prior.and_then(|report| report.channel_name.clone())),
        user_msg_id: prior.and_then(|report| report.user_msg_id),
        generation: metadata.generation,
        doctor_summary: Some(metadata.doctor_summary),
    };
    save_report(path, &report)?;
    Ok(attempt.clone())
}
pub(crate) fn restart_claim_for_context(context: &RestartReportContext) -> Option<RestartAttempt> {
    restart_reports_root().and_then(|reports| {
        runtime_paths::agentdesk_root()
            .and_then(|runtime| restart_claim_for_context_in_root(&reports, &runtime, context))
    })
}
pub(crate) fn restart_claim_for_context_in_root(
    root: &Path,
    runtime_root: &Path,
    context: &RestartReportContext,
) -> Option<RestartAttempt> {
    let path = restart_report_path(root, &context.provider, context.channel_id);
    let _lock = lock_restart_report_path(&path).ok()?;
    let report = load_report(&path).ok().flatten()?;
    if matches!(report.state, RestartReportState::Awaiting)
        && report.generation == process_generation()
        && restart_terminal_state(runtime_root, &report.attempt).is_none()
    {
        return Some(report.attempt);
    }
    if matches!(report.state, RestartReportState::Awaiting) {
        let _ = fs::remove_file(path);
    }
    None
}
pub(crate) fn settle_restart_handoff(
    context: &RestartReportContext,
    attempt: &RestartAttempt,
    state: RestartReportState,
) -> Result<SettleOutcome, RestartReportError> {
    let root = restart_reports_root().ok_or_else(|| "restart report unavailable".to_string())?;
    settle_restart_path(
        &restart_attempt_report_path(&root, context, attempt),
        attempt,
        state,
    )
}
#[cfg(test)]
pub(crate) fn settle_restart_in_root(
    root: &Path,
    context: &RestartReportContext,
    attempt: &RestartAttempt,
    state: RestartReportState,
) -> Result<SettleOutcome, RestartReportError> {
    debug_assert!(!matches!(state, RestartReportState::Awaiting));
    settle_restart_path(
        &restart_report_path(root, &context.provider, context.channel_id),
        attempt,
        state,
    )
}
fn settle_restart_path(
    path: &Path,
    attempt: &RestartAttempt,
    state: RestartReportState,
) -> Result<SettleOutcome, RestartReportError> {
    let _lock = lock_restart_report_path(path)?;
    let Some(mut report) = load_report(path)? else {
        return Ok(SettleOutcome::Absent);
    };
    if report.attempt != *attempt {
        return Ok(SettleOutcome::Stale);
    }
    if !matches!(report.state, RestartReportState::Awaiting) {
        return Ok(SettleOutcome::AlreadyTerminal);
    }
    report.state = state;
    save_report(path, &report)?;
    Ok(SettleOutcome::Settled)
}
pub(crate) fn load_restart_report(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<RestartCompletionReport> {
    let root = restart_reports_root()?;
    let path = restart_report_path(&root, provider, channel_id);
    let _lock = lock_restart_report_path(&path).ok()?;
    load_report(&path)
        .ok()
        .flatten()
        .filter(|report| report.provider_kind().as_ref() == Some(provider))
}
pub(crate) fn clear_restart_report(
    provider: &ProviderKind,
    channel_id: u64,
    attempt: &RestartAttempt,
) {
    let Some(root) = restart_reports_root() else {
        return;
    };
    let path = restart_report_path(&root, provider, channel_id);
    let _ = compare_delete(&path, attempt);
}
pub(crate) fn clear_loaded_restart_report(report: Option<&RestartCompletionReport>) {
    let Some(report) = report else {
        return;
    };
    let Some(provider) = report.provider_kind() else {
        return;
    };
    clear_restart_report(&provider, report.channel_id, &report.attempt);
}
pub(crate) fn compare_delete(
    path: &Path,
    attempt: &RestartAttempt,
) -> Result<SettleOutcome, RestartReportError> {
    let _lock = lock_restart_report_path(path)?;
    let Some(report) = load_report(path)? else {
        return Ok(SettleOutcome::Absent);
    };
    if report.attempt != *attempt {
        return Ok(SettleOutcome::Stale);
    }
    fs::remove_file(path).map_err(|error| error.to_string())?;
    Ok(SettleOutcome::Settled)
}
pub(crate) fn load_restart_reports_in_root(
    root: &Path,
    provider: &ProviderKind,
) -> Vec<(PathBuf, RestartCompletionReport)> {
    let Ok(entries) = fs::read_dir(restart_provider_dir(root, provider)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                return None;
            }
            let _lock = lock_restart_report_path(&path).ok()?;
            load_report(&path)
                .ok()
                .flatten()
                .map(|report| (path, report))
        })
        .filter(|(_, report)| report.provider_kind().as_ref() == Some(provider))
        .collect()
}
pub(crate) fn load_restart_reports(
    provider: &ProviderKind,
) -> Vec<(PathBuf, RestartCompletionReport)> {
    restart_reports_root()
        .map(|root| load_restart_reports_in_root(&root, provider))
        .unwrap_or_default()
}
pub(crate) fn restart_terminal_state(
    runtime_root: &Path,
    attempt: &RestartAttempt,
) -> Option<RestartReportState> {
    match terminal_proof(runtime_root, attempt.as_str()) {
        RestartTerminalProof::Persisted => Some(RestartReportState::Succeeded),
        RestartTerminalProof::Cancelled => {
            Some(RestartReportState::Failed(RestartFailure::Cancelled))
        }
        RestartTerminalProof::Pending => None,
    }
}
#[rustfmt::skip]
fn awaiting_report_expired_at(report: &RestartCompletionReport, now: chrono::NaiveDateTime) -> bool {
    chrono::NaiveDateTime::parse_from_str(&report.created_at, "%Y-%m-%d %H:%M:%S").map(|created_at| now.signed_duration_since(created_at) >= AWAITING_REPORT_MAX_AGE).unwrap_or(true)
}
#[rustfmt::skip]
pub(crate) fn settle_awaiting_for_flush(path: &Path, report: &mut RestartCompletionReport, runtime_root: Option<&Path>, now: chrono::NaiveDateTime) -> bool {
    if !matches!(report.state, RestartReportState::Awaiting) { return true; }
    let terminal = runtime_root.and_then(|root| restart_terminal_state(root, &report.attempt)).or_else(|| awaiting_report_expired_at(report, now).then_some(RestartReportState::Failed(RestartFailure::Unverified)));
    let Some(terminal) = terminal else { return false; };
    if !matches!(settle_restart_path(path, &report.attempt, terminal.clone()), Ok(SettleOutcome::Settled)) { return false; }
    report.state = terminal; true
}

pub(crate) fn current_report_matches(path: &Path, expected: &RestartCompletionReport) -> bool {
    let Ok(_lock) = lock_restart_report_path(path) else {
        return false;
    };
    let Some(current) = load_report(path).ok().flatten() else {
        return false;
    };
    current.attempt == expected.attempt && current.state == expected.state
}

#[cfg(test)]
mod tests;
