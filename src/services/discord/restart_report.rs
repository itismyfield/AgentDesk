use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};

use super::formatting::send_long_message_raw;
use super::runtime_store::{atomic_write, discord_restart_reports_root, fsync_parent_dir};
use super::settings::validate_bot_channel_routing;
use super::{SharedData, mailbox_has_active_turn, mailbox_snapshot};
use crate::services::provider::ProviderKind;

const RESTART_REPORT_VERSION: u32 = 1;
pub(crate) const RESTART_REPORT_CHANNEL_ENV: &str = "AGENTDESK_REPORT_CHANNEL_ID";
pub(crate) const RESTART_REPORT_PROVIDER_ENV: &str = "AGENTDESK_REPORT_PROVIDER";

#[derive(Debug, Clone)]
pub(crate) struct RestartReportContext {
    pub provider: ProviderKind,
    pub channel_id: u64,
    pub current_msg_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestartCompletionReport {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    #[serde(default)]
    pub current_msg_id: Option<u64>,
    pub status: String,
    pub summary: String,
    pub completed_at: String,
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
    fn new(
        provider: ProviderKind,
        channel_id: u64,
        status: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            version: RESTART_REPORT_VERSION,
            provider: provider.as_str().to_string(),
            channel_id,
            current_msg_id: None,
            status: status.into(),
            summary: summary.into(),
            completed_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            channel_name: None,
            user_msg_id: None,
            generation: super::runtime_store::process_generation(),
            doctor_summary: Some(
                crate::cli::doctor::startup::latest_startup_doctor_health_json(true),
            ),
        }
    }

    pub(crate) fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}

pub(crate) fn restart_report_context_from_env() -> Option<RestartReportContext> {
    let provider = ProviderKind::from_str(&std::env::var(RESTART_REPORT_PROVIDER_ENV).ok()?)?;
    let channel_id = std::env::var(RESTART_REPORT_CHANNEL_ENV)
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(RestartReportContext {
        provider,
        channel_id,
        current_msg_id: None,
    })
}

fn restart_report_path(root: &Path, provider: &ProviderKind, channel_id: u64) -> PathBuf {
    root.join(provider.as_str())
        .join(format!("{channel_id}.json"))
}
fn claims_dir(path: &Path) -> PathBuf {
    path.parent().expect("report provider dir").join("claims")
}
fn claim_path(path: &Path, state: &str) -> PathBuf {
    let channel = path
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("unknown");
    claims_dir(path).join(format!("{channel}.{state}.json"))
}
fn quarantine_path(path: &Path, label: &str) -> PathBuf {
    let channel = path
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("unknown");
    claims_dir(path).join(format!(
        "{channel}.quarantined.{label}.{}.json",
        uuid::Uuid::new_v4().simple()
    ))
}

#[derive(Debug)]
enum RawSlot {
    Absent,
    Legacy(RestartCompletionReport),
    MalformedLegacy,
    Owned,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RestartReportError {
    #[error("restart report store unavailable")]
    Unavailable,
    #[error("restart report publication unsupported on this platform")]
    Unsupported,
    #[error("restart report slot is unsafe: {0}")]
    Unsafe(&'static str),
    #[error("restart report slot is unreadable: {0}")]
    Unreadable(String),
    #[error("restart report persistence failed: {0}")]
    Persist(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartReportWriteOutcome {
    Durable,
    PointOfNoReturn,
}

#[derive(Debug, Serialize, Deserialize)]
struct QueuedClaim(String, RestartCompletionReport);

fn read_raw_slot(path: &Path) -> Result<RawSlot, RestartReportError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(RawSlot::Absent),
        Err(error) => return Err(RestartReportError::Unreadable(error.to_string())),
    };
    if serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .and_then(|object| object.get("nonce").cloned())
        .is_some_and(|nonce| !nonce.is_null())
    {
        return Ok(RawSlot::Owned);
    }
    Ok(match serde_json::from_str(&raw) {
        Ok(report) => RawSlot::Legacy(report),
        Err(_) => RawSlot::MalformedLegacy,
    })
}

#[cfg(unix)]
fn lock_slot(
    path: &Path,
) -> Result<super::outbound::delivery_record::DeliveryRecordLock, RestartReportError> {
    super::outbound::delivery_record::lock_record_path(path).map_err(RestartReportError::Persist)
}
#[cfg(not(unix))]
fn lock_slot(_path: &Path) -> Result<(), RestartReportError> {
    Err(RestartReportError::Unsupported)
}

fn sync_parent(path: &Path) -> Result<(), RestartReportError> {
    fsync_parent_dir(path)
        .map_err(|error| RestartReportError::Persist(format!("sync parent: {error}")))
}
fn path_exists(path: &Path) -> Result<bool, RestartReportError> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(RestartReportError::Unreadable(error.to_string())),
    }
}
fn durable_rename(from: &Path, to: &Path) -> Result<(), RestartReportError> {
    fs::create_dir_all(to.parent().expect("claim parent"))
        .map_err(|error| RestartReportError::Persist(error.to_string()))?;
    fs::rename(from, to).map_err(|error| RestartReportError::Persist(error.to_string()))?;
    sync_parent(from)?;
    sync_parent(to)
}
fn quarantine(path: &Path, label: &str) -> Result<(), RestartReportError> {
    if path_exists(path)? {
        durable_rename(path, &quarantine_path(path, label))?;
    }
    Ok(())
}
fn queued_report(path: &Path) -> Result<RestartCompletionReport, RestartReportError> {
    let raw = fs::read_to_string(path)
        .map_err(|error| RestartReportError::Unreadable(error.to_string()))?;
    serde_json::from_str::<QueuedClaim>(&raw)
        .map(|claim| claim.1)
        .or_else(|_| serde_json::from_str(&raw))
        .map_err(|error| RestartReportError::Persist(format!("malformed claim: {error}")))
}

fn write_pending_at_with_hooks(
    path: &Path,
    report: RestartCompletionReport,
    mut sync: impl FnMut(&Path) -> Result<(), RestartReportError>,
    mut unlink: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<RestartReportWriteOutcome, RestartReportError> {
    let _lock = lock_slot(path)?;
    let queued = claim_path(path, "queued");
    let inflight = claim_path(path, "inflight");
    let poison = claim_path(path, "prepare");
    if path_exists(&poison)? {
        quarantine(&queued, "poisoned")?;
        return Err(RestartReportError::Unsafe("poisoned-or-ambiguous"));
    }
    if path_exists(&inflight)? {
        quarantine(&inflight, "crash-residue")?;
        return Err(RestartReportError::Unsafe("poisoned-or-ambiguous"));
    }
    match read_raw_slot(path)? {
        RawSlot::Owned => return Err(RestartReportError::Unsafe("nonce-owned")),
        RawSlot::Legacy(_) | RawSlot::MalformedLegacy => {
            quarantine(path, "writer-superseded-canonical")?
        }
        RawSlot::Absent => {}
    }
    quarantine(&queued, "writer-superseded-queued")?;
    let transaction_id = uuid::Uuid::new_v4().to_string();
    atomic_write(
        &poison,
        &serde_json::json!({"transaction_id": transaction_id}).to_string(),
    )
    .map_err(RestartReportError::Persist)?;
    sync(&poison)?;
    let body = serde_json::to_string(&QueuedClaim(transaction_id, report))
        .map_err(|error| RestartReportError::Persist(error.to_string()))?;
    atomic_write(&queued, &body).map_err(RestartReportError::Persist)?;
    sync(&queued)?;
    unlink(&poison)
        .map_err(|error| RestartReportError::Persist(format!("unlink poison: {error}")))?;
    if sync(&poison).is_err() {
        return Ok(RestartReportWriteOutcome::PointOfNoReturn);
    }
    Ok(RestartReportWriteOutcome::Durable)
}

fn write_pending_at(
    path: &Path,
    report: RestartCompletionReport,
) -> Result<RestartReportWriteOutcome, RestartReportError> {
    write_pending_at_with_hooks(path, report, sync_parent, |path| fs::remove_file(path))
}

pub(crate) fn write_cli_pending_restart_report(
    context: &RestartReportContext,
) -> Result<RestartReportWriteOutcome, RestartReportError> {
    let root = discord_restart_reports_root().ok_or(RestartReportError::Unavailable)?;
    let mut report = RestartCompletionReport::new(
        context.provider.clone(),
        context.channel_id,
        "pending",
        "dcserver restart requested; 새 프로세스가 completion follow-up을 이어받는 중입니다.",
    );
    report.current_msg_id = context.current_msg_id;
    write_pending_at(
        &restart_report_path(&root, &context.provider, context.channel_id),
        report,
    )
}

pub(crate) fn write_tool_arm_pending_restart_report(
    provider: &ProviderKind,
    channel_id: u64,
    current_msg_id: Option<u64>,
    channel_name: Option<String>,
    request_owner_name: &str,
) -> Result<RestartReportWriteOutcome, RestartReportError> {
    let root = discord_restart_reports_root().ok_or(RestartReportError::Unavailable)?;
    let mut report = RestartCompletionReport::new(
        provider.clone(),
        channel_id,
        "pending",
        format!(
            "dcserver restart requested by `{request_owner_name}`; 새 프로세스가 후속 보고를 이어받을 예정입니다."
        ),
    );
    report.current_msg_id = current_msg_id;
    report.channel_name = channel_name;
    write_pending_at(&restart_report_path(&root, provider, channel_id), report)
}

fn clear_at(path: &Path) -> Result<(), RestartReportError> {
    let _lock = lock_slot(path)?;
    if path_exists(&claim_path(path, "prepare"))? {
        return Err(RestartReportError::Unsafe("poisoned-or-ambiguous"));
    }
    quarantine(&claim_path(path, "inflight"), "clear-ambiguous")?;
    let queued = claim_path(path, "queued");
    if path_exists(&queued)? {
        fs::remove_file(&queued).map_err(|error| RestartReportError::Persist(error.to_string()))?;
        sync_parent(&queued)?;
    }
    match read_raw_slot(path)? {
        RawSlot::Absent => Ok(()),
        RawSlot::Owned => Err(RestartReportError::Unsafe("nonce-owned")),
        RawSlot::Legacy(_) | RawSlot::MalformedLegacy => {
            fs::remove_file(path)
                .map_err(|error| RestartReportError::Persist(error.to_string()))?;
            sync_parent(path)
        }
    }
}

pub(crate) fn clear_restart_report(provider: &ProviderKind, channel_id: u64) {
    let Some(root) = discord_restart_reports_root() else {
        return;
    };
    if let Err(error) = clear_at(&restart_report_path(&root, provider, channel_id)) {
        tracing::warn!(%error, provider = provider.as_str(), channel_id, "restart report clear refused");
    }
}

pub(crate) fn cancel_pending_restart_report(
    context: &RestartReportContext,
) -> Result<(), RestartReportError> {
    let root = discord_restart_reports_root().ok_or(RestartReportError::Unavailable)?;
    clear_at(&restart_report_path(
        &root,
        &context.provider,
        context.channel_id,
    ))
}

fn peek_report(path: &Path) -> Option<RestartCompletionReport> {
    match read_raw_slot(path).ok()? {
        RawSlot::Legacy(report) => Some(report),
        RawSlot::Absent => queued_report(&claim_path(path, "queued")).ok(),
        RawSlot::MalformedLegacy | RawSlot::Owned => None,
    }
}
pub(crate) fn load_restart_report(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<RestartCompletionReport> {
    let root = discord_restart_reports_root()?;
    peek_report(&restart_report_path(&root, provider, channel_id))
        .filter(|report| report.provider_kind().as_ref() == Some(provider))
}

fn provider_channels(root: &Path, provider: &ProviderKind) -> Vec<u64> {
    let dir = root.join(provider.as_str());
    let mut channels = std::collections::BTreeSet::new();
    for scan in [dir.clone(), dir.join("claims")] {
        if let Ok(entries) = fs::read_dir(scan) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(channel) = name.split('.').next().and_then(|v| v.parse().ok()) {
                    channels.insert(channel);
                }
            }
        }
    }
    channels.into_iter().collect()
}

fn acquire_claim(
    path: &Path,
    sync: &mut impl FnMut(&Path) -> Result<(), RestartReportError>,
) -> Result<Option<RestartCompletionReport>, RestartReportError> {
    let queued = claim_path(path, "queued");
    let inflight = claim_path(path, "inflight");
    if path_exists(&inflight)? {
        quarantine(&inflight, "crash-residue")?;
        return Ok(None);
    }
    if path_exists(&claim_path(path, "prepare"))? {
        quarantine(&queued, "poisoned")?;
        return Ok(None);
    }
    match read_raw_slot(path)? {
        RawSlot::Owned => {
            quarantine(&queued, "owned-successor")?;
            Ok(None)
        }
        RawSlot::MalformedLegacy => {
            quarantine(path, "malformed-legacy")?;
            quarantine(&queued, "malformed-canonical")?;
            Ok(None)
        }
        RawSlot::Legacy(report) => {
            quarantine(&queued, "canonical-supersedes-queued")?;
            fs::create_dir_all(claims_dir(path))
                .map_err(|error| RestartReportError::Persist(error.to_string()))?;
            fs::rename(path, &inflight)
                .map_err(|error| RestartReportError::Persist(error.to_string()))?;
            sync(path)?;
            sync(&inflight)?;
            Ok(Some(report))
        }
        RawSlot::Absent if path_exists(&queued)? => match queued_report(&queued) {
            Ok(report) => {
                fs::rename(&queued, &inflight)
                    .map_err(|error| RestartReportError::Persist(error.to_string()))?;
                sync(&queued)?;
                sync(&inflight)?;
                Ok(Some(report))
            }
            Err(_) => {
                quarantine(&queued, "malformed-queued")?;
                Ok(None)
            }
        },
        RawSlot::Absent => Ok(None),
    }
}

async fn flush_slot_at_with_sync<S, F>(
    path: &Path,
    mut sync: impl FnMut(&Path) -> Result<(), RestartReportError>,
    sender: S,
) -> Result<bool, RestartReportError>
where
    S: FnOnce(RestartCompletionReport) -> F,
    F: Future<Output = Result<(), String>>,
{
    let _lock = lock_slot(path)?;
    let Some(report) = acquire_claim(path, &mut sync)? else {
        return Ok(false);
    };
    let inflight = claim_path(path, "inflight");
    match sender(report).await {
        Ok(()) => {
            fs::remove_file(&inflight)
                .map_err(|error| RestartReportError::Persist(error.to_string()))?;
            sync(&inflight)?;
            Ok(true)
        }
        Err(_) => {
            quarantine(&inflight, "ambiguous-send")?;
            Ok(false)
        }
    }
}

fn report_age(report: &RestartCompletionReport) -> Option<Duration> {
    let created_at =
        chrono::NaiveDateTime::parse_from_str(&report.completed_at, "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local::now()
        .naive_local()
        .signed_duration_since(created_at)
        .to_std()
        .ok()
}

async fn report_text(
    report: &RestartCompletionReport,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> String {
    match report.status.as_str() {
        "rolled_back" => "⚠️ 재시작 중 롤백이 발생했습니다.".into(),
        "ok" | "pending" | "sigterm" => {
            let snapshot = mailbox_snapshot(shared, channel_id).await;
            if snapshot.intervention_queue.is_empty() {
                "✅ 재시작 완료. 이어서 진행합니다.".into()
            } else {
                let items = snapshot
                    .intervention_queue
                    .iter()
                    .take(5)
                    .map(|item| {
                        let raw: String = item
                            .text
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(50)
                            .collect();
                        format!("• <@{}>: {}", item.author_id, raw.replace('@', "@\u{200B}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "✅ 재시작 완료. 대기 메시지 {}건:\n{items}",
                    snapshot.intervention_queue.len()
                )
            }
        }
        _ => "❌ 재시작 실패. 관리자에게 문의하세요.".into(),
    }
}

pub(super) async fn flush_restart_reports(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let Some(root) = discord_restart_reports_root() else {
        return;
    };
    for channel in provider_channels(&root, provider) {
        let path = restart_report_path(&root, provider, channel);
        let channel_id = serenity::ChannelId::new(channel);
        if let Some(report) = peek_report(&path) {
            if report.status == "pending" {
                let active = mailbox_has_active_turn(shared, channel_id).await;
                let finalizing = shared
                    .restart
                    .finalizing_turns
                    .load(std::sync::atomic::Ordering::Relaxed)
                    > 0;
                if (active || finalizing)
                    && report_age(&report).unwrap_or_default() < Duration::from_secs(30)
                {
                    continue;
                }
            }
            let settings = { shared.settings.read().await.clone() };
            let is_dm = matches!(
                channel_id.to_channel(http.as_ref()).await,
                Ok(serenity::model::channel::Channel::Private(_))
            );
            if validate_bot_channel_routing(
                &settings,
                provider,
                channel_id,
                report.channel_name.as_deref(),
                is_dm,
            )
            .is_err()
                || report.status == "skipped"
            {
                let _ = clear_at(&path);
                continue;
            }
        }
        let result = flush_slot_at_with_sync(&path, sync_parent, |report| async move {
            let text = report_text(&report, shared, channel_id).await;
            send_long_message_raw(http, channel_id, &text, shared)
                .await
                .map_err(|error| error.to_string())
        })
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, provider = provider.as_str(), channel, "restart report flush failed closed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn report(channel: u64) -> RestartCompletionReport {
        RestartCompletionReport::new(ProviderKind::Claude, channel, "pending", "test")
    }
    fn path(root: &tempfile::TempDir, channel: u64) -> PathBuf {
        restart_report_path(root.path(), &ProviderKind::Claude, channel)
    }
    fn raw(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn nonce_classification_covers_every_json_type_and_legacy_shapes() {
        let root = tempfile::tempdir().unwrap();
        for (index, nonce) in ["\"x\"", "1", "true", "{}", "[]"].iter().enumerate() {
            let path = path(&root, index as u64);
            raw(&path, &format!("{{\"nonce\":{nonce}}}"));
            assert!(matches!(read_raw_slot(&path), Ok(RawSlot::Owned)));
        }
        for (index, body) in ["{\"nonce\":null}", "{}", "[]", "null", "broken"]
            .iter()
            .enumerate()
        {
            let path = path(&root, 20 + index as u64);
            raw(&path, body);
            assert!(!matches!(read_raw_slot(&path), Ok(RawSlot::Owned)));
        }
        let path = path(&root, 99);
        fs::create_dir_all(&path).unwrap();
        assert!(matches!(
            read_raw_slot(&path),
            Err(RestartReportError::Unreadable(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn claims_fail_closed_and_preserve_successors() {
        let root = tempfile::tempdir().unwrap();
        let path = path(&root, 41);
        let mut calls = 0;
        assert!(
            write_pending_at_with_hooks(
                &path,
                report(41),
                |_| {
                    calls += 1;
                    if calls == 2 {
                        Err(RestartReportError::Persist("injected".into()))
                    } else {
                        Ok(())
                    }
                },
                |p: &Path| fs::remove_file(p)
            )
            .is_err()
        );
        assert!(claim_path(&path, "prepare").exists());
        let sent = AtomicUsize::new(0);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(
            !runtime
                .block_on(flush_slot_at_with_sync(&path, sync_parent, |_| async {
                    sent.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }))
                .unwrap()
        );
        assert_eq!(sent.load(Ordering::SeqCst), 0);
        fs::remove_file(claim_path(&path, "prepare")).unwrap();
        raw(
            &claim_path(&path, "inflight"),
            &serde_json::to_string(&report(41)).unwrap(),
        );
        assert!(
            !runtime
                .block_on(flush_slot_at_with_sync(&path, sync_parent, |_| async {
                    sent.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }))
                .unwrap()
        );
        assert_eq!(sent.load(Ordering::SeqCst), 0);
        assert!(!claim_path(&path, "queued").exists());
    }

    #[cfg(unix)]
    #[test]
    fn fsync_failure_sends_zero_and_two_flushers_send_once() {
        let root = tempfile::tempdir().unwrap();
        let path = path(&root, 42);
        raw(&path, &serde_json::to_string(&report(42)).unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(
            runtime
                .block_on(flush_slot_at_with_sync(
                    &path,
                    |_| Err(RestartReportError::Persist("fsync".into())),
                    |_| async {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                ))
                .is_err()
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        quarantine(&claim_path(&path, "inflight"), "reset").unwrap();
        raw(&path, &serde_json::to_string(&report(42)).unwrap());
        let threads = (0..2)
            .map(|_| {
                let path = path.clone();
                let sender_path = path.clone();
                let count = count.clone();
                std::thread::spawn(move || {
                    tokio::runtime::Runtime::new()
                        .unwrap()
                        .block_on(flush_slot_at_with_sync(&path, sync_parent, |_| async {
                            raw(&sender_path, r#"{"nonce":17}"#);
                            count.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(30));
                            Ok(())
                        }))
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read_to_string(path).unwrap(), r#"{"nonce":17}"#);
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_is_explicitly_unsupported() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            write_pending_at(&path(&root, 45), report(45)),
            Err(RestartReportError::Unsupported)
        ));
    }
}
