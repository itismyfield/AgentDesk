use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};

use super::formatting::send_long_message_raw;
use super::runtime_store::{atomic_write, discord_restart_reports_root};
use super::settings::{BotChannelRoutingGuardFailure, validate_bot_channel_routing};
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
    /// Channel name for log context.
    #[serde(default)]
    pub channel_name: Option<String>,
    /// User message ID for reaction management (⏳ → ✅).
    #[serde(default)]
    pub user_msg_id: Option<u64>,
    /// Restart generation at time of report creation.
    #[serde(default)]
    pub generation: u64,
}

impl RestartCompletionReport {
    pub(crate) fn new(
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
            generation: super::runtime_store::load_generation(),
        }
    }

    pub(crate) fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}

pub(crate) fn restart_report_context_from_env() -> Option<RestartReportContext> {
    let provider = std::env::var(RESTART_REPORT_PROVIDER_ENV).ok()?;
    let provider = ProviderKind::from_str(&provider)?;
    let channel_id = std::env::var(RESTART_REPORT_CHANNEL_ENV).ok()?;
    let channel_id = channel_id.parse::<u64>().ok()?;
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

fn restart_report_path(root: &Path, provider: &ProviderKind, channel_id: u64) -> PathBuf {
    restart_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

pub(crate) fn save_restart_report(report: &RestartCompletionReport) -> Result<(), String> {
    let Some(root) = restart_reports_root() else {
        return Err("Home directory not found".to_string());
    };
    save_restart_report_in_root(&root, report)?;
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 📝 Saved restart follow-up report for provider {} channel {}",
        report.provider,
        report.channel_id
    );
    Ok(())
}

fn save_restart_report_in_root(
    root: &Path,
    report: &RestartCompletionReport,
) -> Result<(), String> {
    let Some(provider) = report.provider_kind() else {
        return Err(format!("Unknown provider '{}'", report.provider));
    };
    let path = restart_report_path(root, &provider, report.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

pub(crate) fn clear_restart_report(provider: &ProviderKind, channel_id: u64) {
    let Some(root) = restart_reports_root() else {
        return;
    };
    let path = restart_report_path(&root, provider, channel_id);
    let _ = fs::remove_file(path);
}

pub(crate) fn load_restart_reports(provider: &ProviderKind) -> Vec<RestartCompletionReport> {
    let Some(root) = restart_reports_root() else {
        return Vec::new();
    };
    load_restart_reports_in_root(&root, provider)
}

pub(crate) fn load_restart_report(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<RestartCompletionReport> {
    let root = restart_reports_root()?;
    let path = restart_report_path(&root, provider, channel_id);
    let content = fs::read_to_string(path).ok()?;
    let report = serde_json::from_str::<RestartCompletionReport>(&content).ok()?;
    (report.provider_kind().as_ref() == Some(provider)).then_some(report)
}

fn load_restart_reports_in_root(
    root: &Path,
    provider: &ProviderKind,
) -> Vec<RestartCompletionReport> {
    let dir = restart_provider_dir(&root, provider);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ restart report dir unreadable for provider {}: {} ({})",
                provider.as_str(),
                dir.display(),
                err
            );
            return Vec::new();
        }
    };

    let mut reports = Vec::new();
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to read restart report file: {}",
                path.display()
            );
            continue;
        };
        let Ok(report) = serde_json::from_str::<RestartCompletionReport>(&content) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to parse restart report file: {}",
                path.display()
            );
            continue;
        };
        if report.provider_kind().as_ref() != Some(provider) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ restart report provider mismatch in {}: expected {}, found {}",
                path.display(),
                provider.as_str(),
                report.provider
            );
            continue;
        }
        reports.push(report);
    }
    reports
}

fn report_age(report: &RestartCompletionReport) -> Option<Duration> {
    let created_at =
        chrono::NaiveDateTime::parse_from_str(&report.completed_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let now = chrono::Local::now().naive_local();
    let delta = now.signed_duration_since(created_at);
    delta.to_std().ok()
}

fn should_clear_restart_report_on_routing_failure(reason: BotChannelRoutingGuardFailure) -> bool {
    !reason.is_expected_cross_bot_skip()
}

fn is_unrecoverable_flush_error(error: &str) -> bool {
    error.contains("Unknown Channel")
}

pub(super) async fn flush_restart_reports(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let reports = load_restart_reports(provider);
    if reports.is_empty() {
        return;
    }

    for report in reports {
        let channel_id = serenity::ChannelId::new(report.channel_id);
        let settings_snapshot = { shared.settings.read().await.clone() };
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
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ restart report skip for channel {} — {reason}",
                report.channel_id,
            );
            clear_restart_report(provider, report.channel_id);
            tracing::info!(
                "  [{ts}] 🧹 dropped restart report for channel {} after routing failure",
                report.channel_id,
            );
            continue;
        }

        // "skipped" reports don't need Discord follow-up — just clean up
        if report.status == "skipped" {
            clear_restart_report(provider, report.channel_id);
            continue;
        }

        if report.status == "pending" {
            // Skip pending reports if the turn that created them is still active.
            // The turn will clear the report on normal completion.
            let age = report_age(&report).unwrap_or_default();
            let has_active_turn = mailbox_has_active_turn(shared, channel_id).await;
            let has_finalizing = shared
                .finalizing_turns
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0;
            // If the report is old enough (>30s), the original turn that created
            // it is gone (dcserver restarted). Force flush even if a new turn is
            // active — otherwise the report is stuck forever.
            if (has_active_turn || has_finalizing) && age < Duration::from_secs(30) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏳ pending restart report for channel {} deferred (age={:.0}s, active={}, finalizing={})",
                    report.channel_id,
                    age.as_secs_f64(),
                    has_active_turn,
                    has_finalizing
                );
                continue;
            }
        }

        // Notify via Discord — human-friendly message (no internal details)
        let text = match report.status.as_str() {
            "rolled_back" => "⚠️ 재시작 중 롤백이 발생했습니다.".to_string(),
            s if s == "ok" || s == "pending" || s == "sigterm" => {
                // Build queue preview (skip "진행 중인 턴" — silently handled)
                let queue_preview = {
                    let snapshot = mailbox_snapshot(shared, channel_id).await;
                    let queue = &snapshot.intervention_queue;
                    if !queue.is_empty() {
                        let items: Vec<String> = queue
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
                                // Escape mentions to prevent re-triggering @everyone/@here/role/user mentions
                                let preview = raw.replace('@', "@\u{200B}");
                                format!("• <@{}>: {}", item.author_id, preview)
                            })
                            .collect();
                        if items.is_empty() {
                            None
                        } else {
                            let overflow = if queue.len() > 5 {
                                format!("\n... +{}건", queue.len() - 5)
                            } else {
                                String::new()
                            };
                            Some(format!(
                                "대기 메시지 {}건:\n{}{}",
                                queue.len(),
                                items.join("\n"),
                                overflow
                            ))
                        }
                    } else {
                        None
                    }
                };

                match queue_preview {
                    Some(preview) => format!("✅ 재시작 완료. {preview}"),
                    None => "✅ 재시작 완료. 이어서 진행합니다.".to_string(),
                }
            }
            _ => "❌ 재시작 실패. 관리자에게 문의하세요.".to_string(),
        };
        // Log internal details (summary, status) for debugging
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📝 restart report detail: status={}, summary={}",
                report.status,
                report.summary
            );
        }

        for attempt in 1..=5 {
            match send_long_message_raw(http, channel_id, &text, shared).await {
                Ok(()) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ✓ Flushed restart follow-up report for channel {} on attempt {}",
                        report.channel_id,
                        attempt
                    );
                    // Mark user message as completed: ⏳ → ✅
                    if let Some(umid) = report.user_msg_id {
                        let user_msg_id = serenity::model::id::MessageId::new(umid);
                        super::formatting::remove_reaction_raw(http, channel_id, user_msg_id, '⏳')
                            .await;
                        super::formatting::add_reaction_raw(http, channel_id, user_msg_id, '✅')
                            .await;
                    }
                    clear_restart_report(provider, report.channel_id);
                    break;
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    if attempt < 5 {
                        tracing::info!(
                            "  [{ts}] ⚠ failed to flush restart report for channel {} on attempt {}: {}",
                            report.channel_id,
                            attempt,
                            e
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    } else {
                        tracing::info!(
                            "  [{ts}] ❌ keeping restart report for channel {} after {} failed attempts: {}",
                            report.channel_id,
                            attempt,
                            e
                        );
                        if is_unrecoverable_flush_error(&e.to_string()) {
                            clear_restart_report(provider, report.channel_id);
                            tracing::info!(
                                "  [{ts}] 🧹 dropped unrecoverable restart report for channel {}",
                                report.channel_id
                            );
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RESTART_REPORT_VERSION, RestartCompletionReport, is_unrecoverable_flush_error,
        load_restart_reports_in_root, save_restart_report_in_root,
        should_clear_restart_report_on_routing_failure,
    };
    use crate::services::discord::settings::BotChannelRoutingGuardFailure;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load_restart_report() {
        let temp = TempDir::new().unwrap();
        let report = RestartCompletionReport {
            version: RESTART_REPORT_VERSION,
            provider: "codex".to_string(),
            channel_id: 123,
            current_msg_id: Some(999),
            status: "ok".to_string(),
            summary: "ready".to_string(),
            completed_at: "2026-03-08 18:00:00".to_string(),
            channel_name: None,
            user_msg_id: None,
            generation: 0,
        };

        save_restart_report_in_root(temp.path(), &report).unwrap();
        let content = std::fs::read_to_string(temp.path().join("codex").join("123.json")).unwrap();
        let loaded: RestartCompletionReport = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.channel_id, 123);
        assert_eq!(loaded.status, "ok");
    }

    #[test]
    fn test_load_restart_reports_filters_provider() {
        let temp = TempDir::new().unwrap();

        save_restart_report_in_root(
            temp.path(),
            &RestartCompletionReport {
                version: RESTART_REPORT_VERSION,
                provider: "codex".to_string(),
                channel_id: 123,
                current_msg_id: Some(111),
                status: "ok".to_string(),
                summary: "codex-ready".to_string(),
                completed_at: "2026-03-08 19:00:00".to_string(),
                channel_name: None,
                user_msg_id: None,
                generation: 0,
            },
        )
        .unwrap();

        save_restart_report_in_root(
            temp.path(),
            &RestartCompletionReport {
                version: RESTART_REPORT_VERSION,
                provider: "claude".to_string(),
                channel_id: 456,
                current_msg_id: Some(222),
                status: "ok".to_string(),
                summary: "claude-ready".to_string(),
                completed_at: "2026-03-08 19:00:01".to_string(),
                channel_name: None,
                user_msg_id: None,
                generation: 0,
            },
        )
        .unwrap();

        let codex_reports = load_restart_reports_in_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(codex_reports.len(), 1);
        assert_eq!(codex_reports[0].channel_id, 123);

        let claude_reports = load_restart_reports_in_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(claude_reports.len(), 1);
        assert_eq!(claude_reports[0].channel_id, 456);
    }

    #[test]
    fn test_unknown_channel_is_unrecoverable() {
        assert!(is_unrecoverable_flush_error("Unknown Channel"));
        assert!(!is_unrecoverable_flush_error("temporary network error"));
    }

    #[test]
    fn test_expected_cross_bot_skip_preserves_restart_report() {
        assert!(!should_clear_restart_report_on_routing_failure(
            BotChannelRoutingGuardFailure::ChannelNotAllowed
        ));
        assert!(!should_clear_restart_report_on_routing_failure(
            BotChannelRoutingGuardFailure::AgentMismatch
        ));
        assert!(should_clear_restart_report_on_routing_failure(
            BotChannelRoutingGuardFailure::ProviderMismatch
        ));
    }
}
