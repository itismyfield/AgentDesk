use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};

use super::formatting::send_long_message_raw;
use super::runtime_store::{atomic_write, discord_restart_reports_root};
use super::settings::{self, BotChannelRoutingGuardFailure};
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
    /// Startup doctor summary/artifact metadata captured after restart.
    #[serde(default)]
    pub doctor_summary: Option<serde_json::Value>,
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
            doctor_summary: latest_startup_doctor_summary(),
        }
    }

    pub(crate) fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}

fn latest_startup_doctor_summary() -> Option<serde_json::Value> {
    Some(crate::cli::doctor::startup::latest_startup_doctor_health_json(true))
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

fn is_unrecoverable_flush_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "unknown channel",
        "missing access",
        "missing permissions",
        "forbidden",
        "not found",
        "403 forbidden",
        "404 not found",
    ]
    .iter()
    .any(|pattern| error.contains(pattern))
        || contains_error_code(&error, "50001")
        || contains_error_code(&error, "50013")
}

fn contains_error_code(error: &str, code: &str) -> bool {
    error.match_indices(code).any(|(index, _)| {
        let before = error[..index].chars().next_back();
        let after = error[index + code.len()..].chars().next();
        !before.is_some_and(|ch| ch.is_ascii_alphanumeric())
            && !after.is_some_and(|ch| ch.is_ascii_alphanumeric())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestartReportRoutingDisposition {
    Flush,
    Preserve,
    ClearGenuineOrphan,
}

fn restart_report_routing_disposition(
    settings_snapshot: &super::DiscordBotSettings,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    is_dm: bool,
    live_child_name: Option<&str>,
    thread_parent: Option<(serenity::ChannelId, Option<&str>)>,
) -> (
    RestartReportRoutingDisposition,
    Option<BotChannelRoutingGuardFailure>,
) {
    // A non-DM without a live child name means the Discord metadata lookup did
    // not resolve the child channel.  Do not let a stale report name, or a
    // second parent-only lookup, manufacture authority and delete a report that
    // a sibling bot may own.
    if !is_dm && live_child_name.is_none() {
        return (RestartReportRoutingDisposition::Preserve, None);
    }

    match settings::validate_bot_channel_routing_with_thread_parent(
        settings_snapshot,
        provider,
        channel_id,
        live_child_name,
        thread_parent,
        is_dm,
    ) {
        Ok(()) => (RestartReportRoutingDisposition::Flush, None),
        Err(reason) if !reason.orphans_inflight_on_restart() => {
            (RestartReportRoutingDisposition::Preserve, Some(reason))
        }
        Err(reason) => (
            RestartReportRoutingDisposition::ClearGenuineOrphan,
            Some(reason),
        ),
    }
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
        let (is_dm, live_child_name, thread_parent) =
            super::session_runtime::resolve_live_channel_routing_metadata(http, channel_id).await;
        let (routing_disposition, routing_failure) = restart_report_routing_disposition(
            &settings_snapshot,
            provider,
            channel_id,
            is_dm,
            live_child_name.as_deref(),
            thread_parent
                .as_ref()
                .map(|(parent_id, parent_name)| (*parent_id, parent_name.as_deref())),
        );
        if routing_disposition != RestartReportRoutingDisposition::Flush {
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(reason) = routing_failure {
                tracing::info!(
                    "  [{ts}] ⏭ restart report skip for channel {} — {reason}",
                    report.channel_id,
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⏳ restart report preserved for channel {} — live child metadata unresolved",
                    report.channel_id,
                );
            }
            if routing_disposition == RestartReportRoutingDisposition::ClearGenuineOrphan {
                clear_restart_report(provider, report.channel_id);
                tracing::info!(
                    "  [{ts}] 🧹 dropped genuinely orphaned restart report for channel {} after routing failure",
                    report.channel_id,
                );
            }
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
                .restart
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
                        super::turn_view_reconciler::note_intake_turn_completed(
                            shared,
                            http,
                            channel_id,
                            user_msg_id,
                            report.generation,
                            "restart_report_complete",
                        )
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
mod routing_tests {
    use super::*;
    use crate::services::discord::DiscordBotSettings;

    const CHILD_ID: u64 = 15_046_124_559_162_459;
    const PARENT_ID: u64 = 14_796_713_013_870_592;

    fn bot_settings(provider: ProviderKind, allowed_channel_ids: Vec<u64>) -> DiscordBotSettings {
        DiscordBotSettings {
            provider,
            allowed_channel_ids,
            agent: Some("project-agentdesk".to_string()),
            ..Default::default()
        }
    }

    fn with_role_map(thread_inherit: bool, include_child: bool, test: impl FnOnce()) {
        let root = tempfile::tempdir().expect("temp AgentDesk root");
        let config = root.path().join("config");
        std::fs::create_dir_all(&config).expect("create config dir");
        let child = if include_child {
            format!(
                r#",
    "{CHILD_ID}": {{
      "roleId": "review-agent",
      "promptFile": "/tmp/review-agent.md",
      "provider": "claude"
    }}"#
            )
        } else {
            String::new()
        };
        std::fs::write(
            config.join("role_map.json"),
            format!(
                r#"{{
  "byChannelId": {{
    "{PARENT_ID}": {{
      "roleId": "project-agentdesk",
      "promptFile": "/tmp/project-agentdesk.md",
      "provider": "codex",
      "threadInherit": {thread_inherit}
    }}{child}
  }}
}}"#
            ),
        )
        .expect("write role map");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        test();
    }

    fn disposition(
        settings: &DiscordBotSettings,
        provider: &ProviderKind,
        child_name: Option<&str>,
        parent_name: Option<&str>,
    ) -> (
        RestartReportRoutingDisposition,
        Option<BotChannelRoutingGuardFailure>,
    ) {
        restart_report_routing_disposition(
            settings,
            provider,
            serenity::ChannelId::new(CHILD_ID),
            false,
            child_name,
            Some((serenity::ChannelId::new(PARENT_ID), parent_name)),
        )
    }

    #[test]
    fn inherited_parent_and_id_only_parent_flush_restart_report() {
        with_role_map(true, false, || {
            let codex = bot_settings(ProviderKind::Codex, vec![PARENT_ID]);
            assert_eq!(
                disposition(
                    &codex,
                    &ProviderKind::Codex,
                    Some("child-thread"),
                    Some("adk-cdx")
                ),
                (RestartReportRoutingDisposition::Flush, None)
            );
            assert_eq!(
                disposition(&codex, &ProviderKind::Codex, Some("child-thread"), None),
                (RestartReportRoutingDisposition::Flush, None),
                "known parent ID remains authoritative when its name fetch fails"
            );
        });
    }

    #[test]
    fn direct_child_precedence_and_parent_opt_out_preserve_possible_sibling_owner() {
        with_role_map(true, true, || {
            let parent_bot = bot_settings(ProviderKind::Codex, vec![PARENT_ID]);
            assert_eq!(
                disposition(
                    &parent_bot,
                    &ProviderKind::Codex,
                    Some("review-thread"),
                    Some("adk-cdx")
                ),
                (
                    RestartReportRoutingDisposition::Preserve,
                    Some(BotChannelRoutingGuardFailure::ChannelNotAllowed)
                )
            );

            let mut child_bot = bot_settings(ProviderKind::Claude, vec![CHILD_ID]);
            child_bot.agent = Some("review-agent".to_string());
            assert_eq!(
                disposition(
                    &child_bot,
                    &ProviderKind::Claude,
                    Some("review-thread"),
                    Some("adk-cdx")
                ),
                (RestartReportRoutingDisposition::Flush, None)
            );
        });

        with_role_map(false, false, || {
            let parent_bot = bot_settings(ProviderKind::Codex, vec![PARENT_ID]);
            assert_eq!(
                disposition(
                    &parent_bot,
                    &ProviderKind::Codex,
                    Some("child-thread"),
                    Some("adk-cdx")
                ),
                (
                    RestartReportRoutingDisposition::Preserve,
                    Some(BotChannelRoutingGuardFailure::ChannelNotAllowed)
                ),
                "threadInherit=false may still have a child-allowlisted sibling owner"
            );
            let mut child_allowlist_sibling = bot_settings(ProviderKind::Codex, vec![CHILD_ID]);
            child_allowlist_sibling.agent = None;
            assert_eq!(
                disposition(
                    &child_allowlist_sibling,
                    &ProviderKind::Codex,
                    Some("adk-cdx"),
                    Some("adk-cdx")
                ),
                (RestartReportRoutingDisposition::Flush, None),
                "a child-only allowlist can own an opted-out thread without a role binding"
            );
        });
    }

    #[test]
    fn unresolved_and_cross_bot_routing_preserve_but_provider_orphan_clears() {
        with_role_map(true, false, || {
            let owner = bot_settings(ProviderKind::Codex, vec![PARENT_ID]);
            assert_eq!(
                restart_report_routing_disposition(
                    &owner,
                    &ProviderKind::Codex,
                    serenity::ChannelId::new(CHILD_ID),
                    false,
                    None,
                    None,
                ),
                (RestartReportRoutingDisposition::Preserve, None),
                "unresolved live child metadata is not authority to delete"
            );

            let sibling_allowlist = bot_settings(ProviderKind::Codex, vec![CHILD_ID]);
            assert_eq!(
                disposition(
                    &sibling_allowlist,
                    &ProviderKind::Codex,
                    Some("child-thread"),
                    Some("adk-cdx")
                ),
                (
                    RestartReportRoutingDisposition::Preserve,
                    Some(BotChannelRoutingGuardFailure::ChannelNotAllowed)
                )
            );

            let mut sibling_agent = bot_settings(ProviderKind::Codex, vec![PARENT_ID]);
            sibling_agent.agent = Some("review-agent".to_string());
            assert_eq!(
                disposition(
                    &sibling_agent,
                    &ProviderKind::Codex,
                    Some("child-thread"),
                    Some("adk-cdx")
                ),
                (
                    RestartReportRoutingDisposition::Preserve,
                    Some(BotChannelRoutingGuardFailure::AgentMismatch)
                )
            );

            let mut wrong_provider = bot_settings(ProviderKind::Claude, vec![PARENT_ID]);
            wrong_provider.agent = Some("project-agentdesk".to_string());
            assert_eq!(
                disposition(
                    &wrong_provider,
                    &ProviderKind::Claude,
                    Some("child-thread"),
                    Some("adk-cdx")
                ),
                (
                    RestartReportRoutingDisposition::ClearGenuineOrphan,
                    Some(BotChannelRoutingGuardFailure::ProviderMismatch)
                )
            );
        });
    }

    #[test]
    fn restart_report_source_uses_one_live_snapshot_and_only_safe_cleanup_paths() {
        let production = include_str!("restart_report.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production restart-report source");
        assert_eq!(
            production
                .matches("resolve_live_channel_routing_metadata(http, channel_id)")
                .count(),
            1
        );
        assert!(production.contains("live_child_name.as_deref(),"));
        assert!(!production.contains("report.channel_name.as_deref()"));
        assert!(!production.contains("validate_bot_channel_routing("));
        assert!(production.contains("!reason.orphans_inflight_on_restart()"));
        assert!(production.contains("is_unrecoverable_flush_error(&e.to_string())"));
    }

    #[test]
    fn recovery_authority_surface_inventory_is_complete() {
        let surfaces = [
            (
                "restart_report",
                include_str!("restart_report.rs"),
                "resolve_live_channel_routing_metadata(http, channel_id)",
            ),
            (
                "catch_up",
                include_str!("catch_up.rs"),
                "resolve_live_bot_channel_routing_status(",
            ),
            (
                "live_helper",
                include_str!("session_runtime/channel_routing.rs"),
                "resolve_live_channel_routing_metadata(&ctx.http, channel_id)",
            ),
            (
                "restore_inflight",
                include_str!("recovery_engine/restore_inflight.rs"),
                "validate_recovery_no_event_routing(",
            ),
            (
                "watcher_restore",
                include_str!("watchers/lifecycle.rs"),
                "classify_live_bot_channel_routing_status(",
            ),
            (
                "manual_rebind",
                include_str!("recovery_engine/manual_rebind/mod.rs"),
                "classify_live_bot_channel_routing_status(",
            ),
            (
                "recovery_flush",
                include_str!("runtime_bootstrap/recovery_flush.rs"),
                "cached_live_bot_routing_status(",
            ),
        ];
        for (name, source, authority_gate) in surfaces {
            assert!(
                source.contains(authority_gate),
                "{name} must remain in the recovery authority inventory"
            );
        }

        let catch_up = include_str!("catch_up.rs");
        assert!(catch_up.contains("if settings::resolve_role_binding("));
        assert!(catch_up.contains(
            "Ok(()) | Err(settings::BotChannelRoutingGuardFailure::ProviderMismatch) => true"
        ));
        assert!(catch_up.contains("Err(_) => false"));
        let no_event = include_str!("recovery_engine/live_routing.rs");
        assert_eq!(
            no_event
                .matches("validate_bot_channel_routing_with_thread_parent(")
                .count(),
            1
        );
        assert!(no_event.contains("if !is_dm && live_child_name.is_none()"));
    }
}
