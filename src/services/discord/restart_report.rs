use super::settings::{BotChannelRoutingGuardFailure, validate_bot_channel_routing};
use super::{SharedData, mailbox_snapshot};
pub(crate) use crate::domain::restart_report::{
    RestartAttempt, RestartCompletionReport, RestartFailure, RestartReportContext,
    RestartReportState,
};
use crate::services::provider::ProviderKind;
pub(crate) use crate::services::restart_handoff::store::{
    clear_loaded_restart_report, clear_restart_report, load_restart_report,
};
use crate::services::restart_handoff::store::{
    compare_delete, current_report_matches, load_restart_reports, restart_report_path,
    settle_awaiting_for_flush,
};
use crate::services::restart_handoff::{RestartMetadata, sample_metadata};
use poise::serenity_prelude as serenity;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
mod capacity;
#[cfg(test)]
mod model;

fn restart_metadata() -> RestartMetadata {
    sample_metadata(crate::cli::doctor::startup::latest_startup_doctor_health_json)
}

pub(crate) fn announce_restart(context: &RestartReportContext) -> Result<RestartAttempt, String> {
    crate::services::restart_handoff::store::announce_restart(context, &restart_metadata)
}

#[cfg(test)]
use crate::domain::restart_report::SettleOutcome;
#[cfg(test)]
use crate::services::restart_handoff::store::{
    LegacyRestartReport, load_report, load_restart_reports_in_root, restart_attempt_report_path,
    restart_claim_for_context_in_root, restart_terminal_state, save_report, settle_restart_in_root,
};
#[cfg(test)]
use std::fs;
#[cfg(test)]
pub(super) fn announce_restart_in_root(
    root: &Path,
    context: &RestartReportContext,
) -> Result<RestartAttempt, String> {
    crate::services::restart_handoff::store::announce_restart_in_root(
        root,
        context,
        &restart_metadata,
    )
}
#[cfg(test)]
fn prepare_restart_handoff_in_root(
    reports: &Path,
    runtime: &Path,
    context: &RestartReportContext,
    claim: Option<&RestartAttempt>,
) -> Result<RestartAttempt, String> {
    crate::services::restart_handoff::store::prepare_restart_handoff_in_root(
        reports,
        runtime,
        context,
        claim,
        &restart_metadata,
    )
}
#[rustfmt::skip]
fn restart_delivery_nonce(attempt: &RestartAttempt) -> String {
    format!("adkrr{}", super::outbound::outbound_fingerprint(&["restart-report-delivery-v1", attempt.as_str()]))
}
#[rustfmt::skip]
fn current_report_delivery_nonce(path: &Path, expected: &RestartCompletionReport) -> Option<String> {
    current_report_matches(path, expected).then(|| restart_delivery_nonce(&expected.attempt))
}
#[rustfmt::skip]
async fn send_report(http: &Arc<serenity::Http>, shared: &Arc<SharedData>, path: &Path, channel_id: serenity::ChannelId, text: &str, report: &RestartCompletionReport) -> bool {
    let Some(nonce) = current_report_delivery_nonce(path, report) else { return false; };
    // Lock is released before HTTP. A replacement in this residual interval can still make this send stale; the per-attempt nonce prevents only same-attempt duplicates.
    super::gateway::send_outbound_message_with_nonce_classified(Arc::clone(http), Arc::clone(shared), channel_id, text, &nonce).await.is_ok()
}

fn consume_after_send_acceptance(path: &Path, attempt: &RestartAttempt, accepted: bool) {
    if accepted {
        let _ = compare_delete(path, attempt);
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
    let runtime_root = crate::services::runtime_paths::agentdesk_root();
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
        if !settle_awaiting_for_flush(
            &path,
            &mut report,
            runtime_root.as_deref(),
            chrono::Local::now().naive_local(),
        ) {
            continue;
        }
        if let Some(predecessor) = report.predecessor.as_ref() {
            let canonical = restart_report_path(
                path.parent().and_then(Path::parent).unwrap_or(&path),
                provider,
                report.channel_id,
            );
            let _ = compare_delete(&canonical, predecessor);
        }
        if matches!(
            report.state,
            RestartReportState::Failed(RestartFailure::Cancelled)
        ) {
            let _ = compare_delete(&path, &report.attempt);
            continue;
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
            RestartReportState::Failed(RestartFailure::RolledBack) => {
                "⚠️ 재시작 중 롤백이 발생했습니다.".to_string()
            }
            RestartReportState::Failed(RestartFailure::Unverified) => {
                "⚠️ 재시작 결과를 확인하지 못했습니다. 상태를 확인해 주세요.".to_string()
            }
            RestartReportState::Failed(RestartFailure::Cancelled) => continue,
            RestartReportState::Awaiting => continue,
        };
        let accepted = send_report(http, shared, &path, channel_id, &text, &report).await;
        if accepted {
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
            consume_after_send_acceptance(&path, &report.attempt, true);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;
    use tempfile::tempdir;

    fn context() -> RestartReportContext {
        RestartReportContext {
            provider: ProviderKind::Claude,
            channel_id: 42,
            current_msg_id: Some(7),
            channel_name: None,
        }
    }
    #[rustfmt::skip]
    fn report(root: &Path, attempt: &RestartAttempt) -> RestartCompletionReport {
        load_restart_reports_in_root(root, &ProviderKind::Claude).into_iter().map(|(_, report)| report).find(|report| report.attempt == *attempt).unwrap()
    }
    fn proof(root: &Path, stem: &str, attempt: &RestartAttempt) {
        fs::write(
            root.join(format!("{stem}.{}", attempt.as_str())),
            format!("nonce={}\n", attempt.as_str()),
        )
        .unwrap();
    }

    #[rustfmt::skip]
    #[test]
    fn production_path_preserves_attempt_and_fresh_bridge_channel() {
        let root = tempdir().unwrap(); let ctx = context();
        let claim = announce_restart_in_root(root.path(), &ctx).unwrap();
        let execution = prepare_restart_handoff_in_root(root.path(), root.path(), &ctx, Some(&claim)).unwrap();
        let outcome = crate::cli::dcserver::run_quick_restart_with_production_effects_for_test(root.path(), "test", execution.as_str(), |nonce| {
            let attempt = RestartAttempt(nonce.into()); assert!(restart_attempt_report_path(root.path(), &ctx, &attempt).exists()); proof(root.path(), "restart_persisted", &attempt);
        });
        assert_ne!(claim, execution); assert!(outcome.is_persisted()); let successor = announce_restart_in_root(root.path(), &ctx).unwrap();
        assert_eq!(report(root.path(), &execution).predecessor, Some(claim)); assert_eq!(restart_terminal_state(root.path(), &execution), Some(RestartReportState::Succeeded)); assert_eq!(report(root.path(), &successor).attempt, successor);
        settle_restart_in_root(root.path(), &ctx, &successor, RestartReportState::Succeeded).unwrap();
        let old = RestartReportContext::from_bridge(ProviderKind::Claude, 42, Some(8), Some("adk-old".into())); let old_attempt = announce_restart_in_root(root.path(), &old).unwrap(); settle_restart_in_root(root.path(), &old, &old_attempt, RestartReportState::Succeeded).unwrap();
        let fresh = RestartReportContext::from_bridge(ProviderKind::Claude, 42, Some(9), Some("adk-fresh".into())); let fresh_attempt = announce_restart_in_root(root.path(), &fresh).unwrap(); assert_eq!(report(root.path(), &fresh_attempt).channel_name.as_deref(), Some("adk-fresh"));
    }

    #[rustfmt::skip]
    #[test]
    fn exact_fencing_nonce_and_acceptance_survive_a_b_c_overlap() {
        let root = tempdir().unwrap(); let ctx = context(); let path = restart_report_path(root.path(), &ctx.provider, ctx.channel_id);
        let initial = announce_restart_in_root(root.path(), &ctx).unwrap(); let mut wrong_generation = report(root.path(), &initial); wrong_generation.generation = wrong_generation.generation.wrapping_add(1); save_report(&path, &wrong_generation).unwrap();
        assert_eq!(restart_claim_for_context_in_root(root.path(), root.path(), &ctx), None); assert!(!path.exists());
        let proven = announce_restart_in_root(root.path(), &ctx).unwrap(); proof(root.path(), "restart_persisted", &proven); assert_eq!(restart_claim_for_context_in_root(root.path(), root.path(), &ctx), None); assert!(!path.exists());
        let attempt_a = announce_restart_in_root(root.path(), &ctx).unwrap(); let attempt_b = announce_restart_in_root(root.path(), &ctx).unwrap(); assert_eq!(compare_delete(&path, &attempt_a).unwrap(), SettleOutcome::Stale);
        let scanned_b = report(root.path(), &attempt_b); consume_after_send_acceptance(&path, &attempt_b, false); assert!(path.exists());
        let attempt_c = announce_restart_in_root(root.path(), &ctx).unwrap(); assert_eq!(current_report_delivery_nonce(&path, &scanned_b), None); consume_after_send_acceptance(&path, &attempt_b, true);
        let scanned_c = report(root.path(), &attempt_c); let nonce_c = current_report_delivery_nonce(&path, &scanned_c).unwrap(); assert_eq!(nonce_c, restart_delivery_nonce(&attempt_c)); assert_ne!(nonce_c, restart_delivery_nonce(&attempt_b)); assert!(nonce_c.is_ascii() && nonce_c.len() <= 25);
        let mut wrong_state = scanned_c.clone(); wrong_state.state = RestartReportState::Failed(RestartFailure::Unverified); assert_eq!(current_report_delivery_nonce(&path, &wrong_state), None);
        assert_eq!(restart_delivery_nonce(&RestartAttempt("00112233-4455-6677-8899-aabbccddeeff".into())), "adkrre7257951ac8d944a"); consume_after_send_acceptance(&path, &attempt_c, true); assert!(!path.exists());
        let cancelled = RestartAttempt::new(); proof(root.path(), "restart_cancelled", &cancelled); assert_eq!(restart_terminal_state(root.path(), &cancelled), Some(RestartReportState::Failed(RestartFailure::Cancelled)));
    }

    #[rustfmt::skip]
    #[test]
    fn pending_awaiting_and_corrupt_reports_are_conservative() {
        let root = tempdir().unwrap(); let ctx = context(); let path = restart_report_path(root.path(), &ctx.provider, ctx.channel_id); fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"version":1,"provider":"claude","channel_id":42,"status":"pending","completed_at":"2026-08-01 00:00:00"}"#).unwrap(); let mut migrated = load_report(&path).unwrap().unwrap(); assert!(matches!(migrated.state, RestartReportState::Awaiting));
        let now = chrono::NaiveDateTime::parse_from_str("2026-08-01 00:06:00", "%Y-%m-%d %H:%M:%S").unwrap(); assert!(settle_awaiting_for_flush(&path, &mut migrated, Some(root.path()), now)); assert!(matches!(report(root.path(), &migrated.attempt).state, RestartReportState::Failed(RestartFailure::Unverified)));
        fs::write(&path, "{").unwrap(); assert!(load_restart_reports_in_root(root.path(), &ctx.provider).is_empty()); assert!(path.with_extension("json.invalid").exists());
        let fresh = announce_restart_in_root(root.path(), &ctx).unwrap(); consume_report_for_routing_failure(&path, &fresh, BotChannelRoutingGuardFailure::ChannelNotAllowed); assert_eq!(report(root.path(), &fresh).attempt, fresh); settle_restart_in_root(root.path(), &ctx, &fresh, RestartReportState::Succeeded).unwrap(); consume_report_for_routing_failure(&path, &fresh, BotChannelRoutingGuardFailure::ProviderMismatch); assert!(!path.exists());
    }
}
