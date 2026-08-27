use super::formatting::send_long_message_raw;
use super::runtime_store::{atomic_write, discord_restart_reports_root, fsync_parent_dir};
use super::settings::{BotChannelRoutingGuardFailure, validate_bot_channel_routing};
use super::{SharedData, mailbox_snapshot};
use crate::cli::restart_terminal_proof::{RestartTerminalProof, terminal_proof};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
const RESTART_REPORT_VERSION: u32 = 2;
pub(crate) const RESTART_REPORT_CHANNEL_ENV: &str = "AGENTDESK_REPORT_CHANNEL_ID";
pub(crate) const RESTART_REPORT_PROVIDER_ENV: &str = "AGENTDESK_REPORT_PROVIDER";
#[derive(Debug, Clone)]
pub(crate) struct RestartReportContext {
    pub provider: ProviderKind,
    pub channel_id: u64,
    pub current_msg_id: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct RestartAttempt(String);
impl RestartAttempt {
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestartFailure {
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub(crate) enum RestartReportState {
    Awaiting,
    Succeeded,
    Failed(RestartFailure),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestartCompletionReport {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub attempt: RestartAttempt,
    #[serde(default)]
    pub predecessor: Option<RestartAttempt>,
    pub state: RestartReportState,
    pub created_at: String,
    #[serde(default)]
    pub current_msg_id: Option<u64>,
    #[serde(default)]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub user_msg_id: Option<u64>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub doctor_summary: Option<serde_json::Value>,
}
impl RestartCompletionReport {
    fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}
pub(crate) type RestartReportError = String;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettleOutcome {
    Settled,
    Absent,
    Stale,
    AlreadyTerminal,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SendVerdict {
    Delivered,
    DefinitelyNotDelivered,
    PossiblyDelivered,
}
pub(crate) fn restart_report_context_from_env() -> Option<RestartReportContext> {
    let provider = ProviderKind::from_str(&std::env::var(RESTART_REPORT_PROVIDER_ENV).ok()?)?;
    let channel_id = std::env::var(RESTART_REPORT_CHANNEL_ENV)
        .ok()?
        .parse()
        .ok()?;
    Some(RestartReportContext {
        provider,
        channel_id,
        current_msg_id: None,
    })
}
fn restart_reports_root() -> Option<PathBuf> {
    discord_restart_reports_root()
}
fn restart_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}
pub(super) fn restart_report_path(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    restart_provider_dir(root, provider).join(format!("{channel_id}.json"))
}
pub(super) fn restart_attempt_report_path(
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
fn lock_restart_report_path(
    path: &Path,
) -> Result<super::outbound::delivery_record::DeliveryRecordLock, String> {
    super::outbound::delivery_record::lock_record_path(path)
}
fn save_report(path: &Path, report: &RestartCompletionReport) -> Result<(), String> {
    let json = serde_json::to_string_pretty(report).map_err(|error| error.to_string())?;
    atomic_write(path, &json)?;
    fsync_parent_dir(path).map_err(|error| error.to_string())
}
#[derive(Deserialize)]
#[rustfmt::skip]
struct LegacyRestartReport {
    version: u32, provider: String, channel_id: u64, status: String, completed_at: String,
    #[serde(default)] current_msg_id: Option<u64>, #[serde(default)] channel_name: Option<String>, #[serde(default)] user_msg_id: Option<u64>, #[serde(default)] generation: u64, #[serde(default)] doctor_summary: Option<serde_json::Value>,
}
#[rustfmt::skip]
fn load_report(path: &Path) -> Result<Option<RestartCompletionReport>, String> {
    let raw = match fs::read_to_string(path) { Ok(raw) => raw, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(error) => return Err(error.to_string()) };
    if let Ok(report) = serde_json::from_str::<RestartCompletionReport>(&raw) && report.version == RESTART_REPORT_VERSION { return Ok(Some(report)); }
    if let Ok(old) = serde_json::from_str::<LegacyRestartReport>(&raw) && old.version == 1 {
        if old.status == "skipped" { fs::remove_file(path).map_err(|e| e.to_string())?; return Ok(None); }
        let state = if matches!(old.status.as_str(), "ok" | "pending" | "sigterm") { RestartReportState::Succeeded } else { RestartReportState::Failed(RestartFailure::Cancelled) };
        let report = RestartCompletionReport { version: RESTART_REPORT_VERSION, provider: old.provider, channel_id: old.channel_id, attempt: RestartAttempt::new(), predecessor: None, state, created_at: old.completed_at, current_msg_id: old.current_msg_id, channel_name: old.channel_name, user_msg_id: old.user_msg_id, generation: old.generation, doctor_summary: old.doctor_summary };
        save_report(path, &report)?; return Ok(Some(report));
    }
    let quarantine = path.with_extension("json.invalid"); let _ = fs::remove_file(&quarantine);
    if fs::rename(path, &quarantine).is_ok() { tracing::warn!("quarantined corrupt or unsupported restart report {}", path.display()); }
    Ok(None)
}
pub(crate) fn announce_restart(
    context: &RestartReportContext,
) -> Result<RestartAttempt, RestartReportError> {
    announce_restart_in_root(
        &restart_reports_root().ok_or_else(|| "restart report unavailable".to_string())?,
        context,
    )
}
pub(crate) fn prepare_restart_handoff(
    context: &RestartReportContext,
    claim: Option<&RestartAttempt>,
) -> Result<RestartAttempt, RestartReportError> {
    let (Some(reports), Some(runtime)) = (
        restart_reports_root(),
        super::runtime_store::agentdesk_root(),
    ) else {
        return Err("restart report unavailable".to_string());
    };
    prepare_restart_handoff_in_root(&reports, &runtime, context, claim)
}
pub(super) fn prepare_restart_handoff_in_root(
    reports: &Path,
    runtime: &Path,
    context: &RestartReportContext,
    claim: Option<&RestartAttempt>,
) -> Result<RestartAttempt, RestartReportError> {
    let attempt = RestartAttempt::new();
    let canonical = restart_report_path(reports, &context.provider, context.channel_id);
    let _lock = lock_restart_report_path(&canonical)?;
    let prior = load_report(&canonical).ok().flatten();
    let adopted = claim.and_then(|claim| {
        prior.as_ref().filter(|report| {
            report.attempt == *claim
                && matches!(report.state, RestartReportState::Awaiting)
                && report.generation == super::runtime_store::process_generation()
                && restart_terminal_state(runtime, claim).is_none()
        })
    });
    write_restart_report(
        &restart_attempt_report_path(reports, context, &attempt),
        context,
        adopted,
        &attempt,
        adopted.map(|report| report.attempt.clone()),
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
pub(super) fn announce_restart_in_root(
    root: &Path,
    context: &RestartReportContext,
) -> Result<RestartAttempt, RestartReportError> {
    let attempt = RestartAttempt::new();
    let path = restart_report_path(root, &context.provider, context.channel_id);
    let _lock = lock_restart_report_path(&path)?;
    let prior = load_report(&path)?;
    if let Some(report) = prior
        .as_ref()
        .filter(|report| !matches!(report.state, RestartReportState::Awaiting))
    {
        if ACTIVE_SENDS.contains(report.attempt.as_str()) {
            return Err("restart completion send is in progress".to_string());
        }
        let archived = restart_attempt_report_path(root, context, &report.attempt);
        if !archived.exists() {
            save_report(&archived, report)?;
        }
    }
    write_restart_report(&path, context, prior.as_ref(), &attempt, None)
}
fn write_restart_report(
    path: &Path,
    context: &RestartReportContext,
    prior: Option<&RestartCompletionReport>,
    attempt: &RestartAttempt,
    predecessor: Option<RestartAttempt>,
) -> Result<RestartAttempt, RestartReportError> {
    let report = RestartCompletionReport {
        version: RESTART_REPORT_VERSION,
        provider: context.provider.as_str().to_string(),
        channel_id: context.channel_id,
        attempt: attempt.clone(),
        predecessor,
        state: RestartReportState::Awaiting,
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        current_msg_id: context
            .current_msg_id
            .or_else(|| prior.and_then(|report| report.current_msg_id)),
        channel_name: prior.and_then(|report| report.channel_name.clone()),
        user_msg_id: prior.and_then(|report| report.user_msg_id),
        generation: super::runtime_store::process_generation(),
        doctor_summary: Some(crate::cli::doctor::startup::latest_startup_doctor_health_json(true)),
    };
    save_report(path, &report)?;
    Ok(attempt.clone())
}
pub(crate) fn restart_claim_for_context(context: &RestartReportContext) -> Option<RestartAttempt> {
    restart_reports_root().and_then(|reports| {
        super::runtime_store::agentdesk_root()
            .and_then(|runtime| restart_claim_for_context_in_root(&reports, &runtime, context))
    })
}
pub(super) fn restart_claim_for_context_in_root(
    root: &Path,
    runtime_root: &Path,
    context: &RestartReportContext,
) -> Option<RestartAttempt> {
    let path = restart_report_path(root, &context.provider, context.channel_id);
    let _lock = lock_restart_report_path(&path).ok()?;
    let report = load_report(&path).ok().flatten()?;
    if matches!(report.state, RestartReportState::Awaiting)
        && report.generation == super::runtime_store::process_generation()
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
pub(super) fn settle_restart_in_root(
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
pub(super) fn compare_delete(
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
pub(super) fn load_restart_reports_in_root(
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
fn load_restart_reports(provider: &ProviderKind) -> Vec<(PathBuf, RestartCompletionReport)> {
    restart_reports_root()
        .map(|root| load_restart_reports_in_root(&root, provider))
        .unwrap_or_default()
}
pub(super) fn restart_terminal_state(
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
static ACTIVE_SENDS: LazyLock<dashmap::DashSet<String>> = LazyLock::new(dashmap::DashSet::new);
struct SendClaim(String);
impl SendClaim {
    fn acquire(path: &Path, attempt: &RestartAttempt) -> Option<Self> {
        let key = attempt.as_str().to_owned();
        let claim = ACTIVE_SENDS.insert(key.clone()).then_some(Self(key))?;
        let _lock = lock_restart_report_path(path).ok()?;
        load_report(path)
            .ok()
            .flatten()
            .is_some_and(|report| report.attempt == *attempt)
            .then_some(claim)
    }
}
impl Drop for SendClaim {
    fn drop(&mut self) {
        ACTIVE_SENDS.remove(&self.0);
    }
}
pub(super) fn classify_send_status(status: Option<u16>) -> SendVerdict {
    match status {
        Some(429) => SendVerdict::DefinitelyNotDelivered,
        _ => SendVerdict::PossiblyDelivered,
    }
}
fn classify_send_error(error: &(dyn std::error::Error + 'static)) -> SendVerdict {
    let mut current = Some(error);
    while let Some(candidate) = current {
        if let Some(serenity::Error::Http(http_error)) = candidate.downcast_ref::<serenity::Error>()
        {
            return classify_send_status(http_error.status_code().map(|status| status.as_u16()));
        }
        current = candidate.source();
    }
    SendVerdict::PossiblyDelivered
}
async fn send_report(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    text: &str,
) -> SendVerdict {
    match send_long_message_raw(http, channel_id, text, shared).await {
        Ok(()) => SendVerdict::Delivered,
        Err(error) => classify_send_error(error.as_ref()),
    }
}
#[rustfmt::skip]
fn consume_report_for_routing_failure(path: &Path, attempt: &RestartAttempt, reason: BotChannelRoutingGuardFailure) {
    if !reason.is_expected_cross_bot_skip() { let _ = compare_delete(path, attempt); }
}
pub(super) async fn flush_restart_reports(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let runtime_root = super::runtime_store::agentdesk_root();
    let mut reports = load_restart_reports(provider);
    let predecessors: HashSet<_> = reports
        .iter()
        .filter_map(|(_, report)| report.predecessor.clone())
        .collect();
    reports.retain(|(path, report)| {
        if predecessors.contains(&report.attempt) {
            let _ = compare_delete(path, &report.attempt);
            false
        } else {
            true
        }
    });
    for (path, mut report) in reports {
        if matches!(report.state, RestartReportState::Awaiting) {
            let Some(terminal) = runtime_root
                .as_deref()
                .and_then(|root| restart_terminal_state(root, &report.attempt))
            else {
                continue;
            };
            match settle_restart_path(&path, &report.attempt, terminal.clone()) {
                Ok(SettleOutcome::Settled) => report.state = terminal,
                _ => continue,
            }
        }
        if let Some(predecessor) = report.predecessor.as_ref() {
            let canonical = restart_report_path(
                path.parent().and_then(Path::parent).unwrap_or(&path),
                provider,
                report.channel_id,
            );
            let _ = compare_delete(&canonical, predecessor);
        }
        let channel_id = serenity::ChannelId::new(report.channel_id);
        let settings_snapshot = shared.settings.read().await.clone();
        let is_dm = matches!(
            channel_id.to_channel(http.as_ref()).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        if let Err(reason) = validate_bot_channel_routing(
            &settings_snapshot,
            provider,
            channel_id,
            report.channel_name.as_deref(),
            is_dm,
        ) {
            consume_report_for_routing_failure(&path, &report.attempt, reason);
            continue;
        }
        let Some(_claim) = SendClaim::acquire(&path, &report.attempt) else {
            continue;
        };
        let text = match report.state {
            RestartReportState::Succeeded => {
                let queued = mailbox_snapshot(shared, channel_id)
                    .await
                    .intervention_queue
                    .len();
                if queued == 0 {
                    "✅ 재시작 완료. 이어서 진행합니다.".to_string()
                } else {
                    format!("✅ 재시작 완료. 대기 메시지 {queued}건이 있습니다.")
                }
            }
            RestartReportState::Failed(_) => "❌ 재시작 실패. 관리자에게 문의하세요.".to_string(),
            RestartReportState::Awaiting => continue,
        };
        match send_report(http, shared, channel_id, &text).await {
            SendVerdict::Delivered => {
                if let Some(user_msg_id) = report.user_msg_id {
                    super::turn_view_reconciler::note_intake_turn_completed(
                        shared,
                        http,
                        channel_id,
                        serenity::MessageId::new(user_msg_id),
                        report.generation,
                        "restart_report_complete",
                    )
                    .await;
                }
                let _ = compare_delete(&path, &report.attempt);
            }
            SendVerdict::DefinitelyNotDelivered => {}
            SendVerdict::PossiblyDelivered => {
                let _ = compare_delete(&path, &report.attempt);
            }
        }
    }
}
#[cfg(test)]
#[path = "restart_report_tests.rs"]
mod tests;
