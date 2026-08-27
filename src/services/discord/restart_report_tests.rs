use super::{
    RestartAttempt, RestartFailure, RestartReportContext, RestartReportState, SendVerdict,
    SettleOutcome, announce_restart_in_root, classify_send_status, compare_delete,
    load_restart_reports_in_root, prepare_restart_handoff_in_root, restart_attempt_report_path,
    restart_claim_for_context_in_root, restart_report_path, restart_terminal_state,
    settle_restart_in_root,
};
use crate::services::provider::ProviderKind;
use tempfile::tempdir;
fn context() -> RestartReportContext {
    RestartReportContext {
        provider: ProviderKind::Claude,
        channel_id: 42,
        current_msg_id: Some(7),
    }
}
fn write_proof(root: &std::path::Path, stem: &str, attempt: &RestartAttempt) {
    std::fs::write(
        root.join(format!("{stem}.{}", attempt.as_str())),
        format!("nonce={}\n", attempt.as_str()),
    )
    .unwrap();
}
#[rustfmt::skip]
#[test]
fn production_path_joins_claim_to_fresh_durable_marker_attempt() {
    let root = tempdir().unwrap(); let ctx = context();
    let claim = announce_restart_in_root(root.path(), &ctx).unwrap();
    let execution = prepare_restart_handoff_in_root(root.path(), root.path(), &ctx, Some(&claim)).unwrap();
    let outcome = crate::cli::dcserver::run_quick_restart_with_production_effects_for_test(root.path(), "test", execution.as_str(), |nonce| {
        let attempt = RestartAttempt(nonce.into()); assert!(restart_attempt_report_path(root.path(), &ctx, &attempt).exists()); write_proof(root.path(), "restart_persisted", &attempt);
    });
    assert_ne!(claim, execution); assert!(outcome.is_persisted()); let successor = announce_restart_in_root(root.path(), &ctx).unwrap();
    assert_eq!(only_report_with_attempt(root.path(), &execution).predecessor, Some(claim)); assert_eq!(restart_terminal_state(root.path(), &execution), Some(RestartReportState::Succeeded)); assert_eq!(only_report_with_attempt(root.path(), &successor).attempt, successor);
}
#[rustfmt::skip]
fn only_report_with_attempt(root: &std::path::Path, attempt: &RestartAttempt) -> super::RestartCompletionReport {
    load_restart_reports_in_root(root, &ProviderKind::Claude).into_iter().map(|(_, report)| report).find(|report| report.attempt == *attempt).unwrap()
}
#[rustfmt::skip]
#[test]
fn exact_report_fences_proofs_and_retry_contracts() {
    let root = tempdir().unwrap(); let ctx = context();
    let initial = announce_restart_in_root(root.path(), &ctx).unwrap();
    let path = restart_report_path(root.path(), &ctx.provider, ctx.channel_id);
    let mut report = only_report_with_attempt(root.path(), &initial); report.generation = report.generation.wrapping_add(1); super::save_report(&path, &report).unwrap();
    assert_eq!(restart_claim_for_context_in_root(root.path(), root.path(), &ctx), None); assert!(!path.exists());
    let proven = announce_restart_in_root(root.path(), &ctx).unwrap(); write_proof(root.path(), "restart_persisted", &proven);
    assert_eq!(restart_claim_for_context_in_root(root.path(), root.path(), &ctx), None); assert!(!path.exists());
    let attempt = announce_restart_in_root(root.path(), &ctx).unwrap(); let stale = RestartAttempt::new();
    assert_eq!(settle_restart_in_root(root.path(), &ctx, &stale, RestartReportState::Succeeded).unwrap(), SettleOutcome::Stale);
    assert_eq!(compare_delete(&path, &stale).unwrap(), SettleOutcome::Stale);
    assert_eq!(settle_restart_in_root(root.path(), &ctx, &attempt, RestartReportState::Succeeded).unwrap(), SettleOutcome::Settled);
    assert_eq!(settle_restart_in_root(root.path(), &ctx, &attempt, RestartReportState::Failed(RestartFailure::Cancelled)).unwrap(), SettleOutcome::AlreadyTerminal);
    assert_eq!(compare_delete(&path, &attempt).unwrap(), SettleOutcome::Settled);
    let pending = RestartAttempt::new(); assert_eq!(restart_terminal_state(root.path(), &pending), None);
    let cancelled = RestartAttempt::new(); write_proof(root.path(), "restart_cancelled", &cancelled); assert_eq!(restart_terminal_state(root.path(), &cancelled), Some(RestartReportState::Failed(RestartFailure::Cancelled)));
    assert_eq!(classify_send_status(Some(429)), SendVerdict::DefinitelyNotDelivered);
    for status in [Some(503), Some(403), None] { assert_eq!(classify_send_status(status), SendVerdict::PossiblyDelivered); }
}

#[rustfmt::skip]
#[test]
fn legacy_corrupt_announce_and_send_claim_are_non_wedging() {
    let root=tempdir().unwrap(); let ctx=context(); let path=restart_report_path(root.path(), &ctx.provider, ctx.channel_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap(); std::fs::write(&path, r#"{"version":1,"provider":"claude","channel_id":42,"status":"pending","summary":"x","completed_at":"2026-08-01 00:00:00"}"#).unwrap(); let migrated=super::load_report(&path).unwrap().unwrap(); assert!(matches!(migrated.state, RestartReportState::Succeeded));
    std::fs::write(&path, "{").unwrap(); assert!(super::load_restart_reports_in_root(root.path(), &ctx.provider).is_empty()); assert!(path.with_extension("json.invalid").exists()); let fresh=announce_restart_in_root(root.path(), &ctx).unwrap(); assert_eq!(only_report_with_attempt(root.path(), &fresh).attempt, fresh);
    super::consume_report_for_routing_failure(&path, &fresh, crate::services::discord::settings::BotChannelRoutingGuardFailure::ChannelNotAllowed); assert_eq!(only_report_with_attempt(root.path(), &fresh).attempt, fresh);
    settle_restart_in_root(root.path(), &ctx, &fresh, RestartReportState::Succeeded).unwrap(); let first=super::SendClaim::acquire(&path, &fresh).unwrap(); assert!(announce_restart_in_root(root.path(), &ctx).is_err()); assert!(super::SendClaim::acquire(&path, &fresh).is_none()); drop(first);
    super::consume_report_for_routing_failure(&path, &fresh, crate::services::discord::settings::BotChannelRoutingGuardFailure::ProviderMismatch); assert!(!path.exists()); assert!(super::SendClaim::acquire(&path, &fresh).is_none());
}
