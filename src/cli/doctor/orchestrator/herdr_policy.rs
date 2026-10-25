//! Health observations describe readiness without fixing config or controlling a server.

use super::*;

pub(super) fn build_core_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    let mut checks = vec![
        check_server_running(snapshot),
        check_discord_bot(snapshot),
        check_degraded_reasons(snapshot),
        check_health_db_dashboard(snapshot),
        check_dispatch_outbox(snapshot),
        check_config_audit(snapshot),
        api_exposure::check_server_api_exposure(cfg),
        check_runtime_root(),
        config_dir_checks::check_data_dir(cfg),
        config_dir_checks::check_policies_dir(cfg),
        config_dir_checks::check_routine_script_registration(cfg),
        check_tmux(),
        check_service_manager(),
        check_postgres_connection(cfg),
        check_db_integrity(cfg),
        check_stale_zero_byte_db_files(cfg),
        check_github_repo_registry(cfg),
        check_disk_usage(),
    ];
    #[cfg(target_os = "macos")]
    checks.push(check_file_descriptor_headroom());
    checks.extend(check_mailbox_consistency(snapshot));
    checks.extend(configuration_audit::checks(cfg));
    if let Some(check) = user_default_check(cfg, snapshot) {
        checks.push(check);
    }
    checks
}

fn user_default_check(cfg: &config::Config, snapshot: &HealthSnapshot) -> Option<Check> {
    use crate::config::session_hosts::EndpointMode;
    if !cfg
        .session_hosts
        .herdr
        .endpoints
        .values()
        .any(|e| e.mode == EndpointMode::UserDefault)
    {
        return None;
    }
    let health = snapshot.body.as_ref().and_then(|body| body.get("herdr"));
    let evidence = health
        .cloned()
        .unwrap_or_else(|| json!({"reason": "health_unavailable"}));
    let ready = health.is_some_and(|view| {
        !view
            .get("migration_refused")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && !view
                .get("restart_required")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && cfg
                .session_hosts
                .herdr
                .endpoints
                .iter()
                .filter(|(_, e)| e.mode == EndpointMode::UserDefault)
                .all(|(key, _)| {
                    view.get("endpoints")
                        .and_then(|v| v.get(key))
                        .and_then(|v| v.get("policy"))
                        .and_then(|v| v.get("next_restart_ready"))
                        .and_then(Value::as_bool)
                        == Some(true)
                })
    });
    let detail = format!(
        "UserDefault last observed fresh readiness={ready}; startup proof is independent. {}",
        evidence
    );
    let check = if ready {
        Check::ok(
            "herdr_user_default",
            CheckGroup::Core,
            "Herdr user server policy",
            detail,
        )
    } else {
        Check::warn(
            "herdr_user_default",
            CheckGroup::Core,
            "Herdr user server policy",
            detail,
            "Inspect each endpoint's node, next_restart_ready, startup_proof, reason/action and checker diagnostics. Run bare herdr manually on its execution node if absent; repair its user config and retry. Doctor does not write config, create a daemon, or restart servers.",
        )
    };
    let mut evidence = evidence;
    if let Some(checker) = snapshot
        .body
        .as_ref()
        .and_then(|body| body.get("herdr_checker"))
    {
        evidence["checker_evidence"] = checker.clone();
    }
    Some(
        check
            .with_subsystem("health")
            .with_fix_safety(FixSafety::NotFixable)
            .with_evidence(evidence),
    )
}

#[cfg(test)]
#[path = "herdr_policy_tests.rs"]
mod tests;
