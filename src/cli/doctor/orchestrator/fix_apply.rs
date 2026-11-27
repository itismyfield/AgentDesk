use super::{
    DoctorOptions, Duration, FixAction, FixSafety, HealthSnapshot, Value, dcserver,
    stale_mailbox_repair,
};

fn snapshot_is_healthy(snapshot: &HealthSnapshot) -> bool {
    let Some(body) = snapshot.body.as_ref() else {
        return false;
    };

    if let Some(status) = body.get("status").and_then(Value::as_str) {
        return status == "healthy";
    }

    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let db = body.get("db").and_then(Value::as_bool).unwrap_or(false);
    ok && db
}

pub(super) fn apply_service_fix(
    snapshot: &HealthSnapshot,
    options: &DoctorOptions,
) -> Vec<FixAction> {
    const READY_TIMEOUT: Duration = Duration::from_secs(30);

    if snapshot_is_healthy(snapshot) {
        return Vec::new();
    }

    if !options.allow_restart {
        return vec![FixAction::skipped(
            "service_restart",
            "Service Restart",
            "skipped dcserver service restart",
            FixSafety::ExplicitRestartRequired,
            "rerun with --fix --allow-restart to permit service restart",
        )];
    }

    #[cfg(target_os = "macos")]
    {
        let label = dcserver::current_dcserver_launchd_label();
        if dcserver::is_launchd_job_loaded(&label) {
            return vec![
                match dcserver::restart_launchd_dcserver_and_verify(&label, READY_TIMEOUT) {
                    Ok(()) => FixAction::ok(
                        "service_restart",
                        "Service Restart",
                        format!("launchd kickstart succeeded for {label}"),
                    ),
                    Err(e) => FixAction::fail(
                        "service_restart",
                        "Service Restart",
                        format!("launchd kickstart failed for {label}: {e}"),
                    ),
                },
            ];
        }
        return Vec::new();
    }

    #[cfg(target_os = "linux")]
    {
        if dcserver::is_systemd_service_enabled() || dcserver::is_systemd_service_active() {
            return vec![
                match dcserver::restart_systemd_dcserver_and_verify(READY_TIMEOUT) {
                    Ok(()) => FixAction::ok(
                        "service_restart",
                        "Service Restart",
                        "systemd --user restart succeeded for agentdesk-dcserver",
                    ),
                    Err(e) => FixAction::fail(
                        "service_restart",
                        "Service Restart",
                        format!("systemd --user restart failed: {e}"),
                    ),
                },
            ];
        }
        return Vec::new();
    }

    #[cfg(target_os = "windows")]
    {
        if dcserver::is_windows_service_installed() {
            return vec![
                match dcserver::restart_windows_dcserver_and_verify(READY_TIMEOUT) {
                    Ok(()) => FixAction::ok(
                        "service_restart",
                        "Service Restart",
                        "Windows service restart succeeded for AgentDeskDcserver",
                    ),
                    Err(e) => FixAction::fail(
                        "service_restart",
                        "Service Restart",
                        format!("Windows service restart failed: {e}"),
                    ),
                },
            ];
        }
        return Vec::new();
    }

    #[allow(unreachable_code)]
    Vec::new()
}
pub(super) fn apply_stale_mailbox_fixes(
    snapshot: &HealthSnapshot,
    options: &DoctorOptions,
) -> Vec<FixAction> {
    stale_mailbox_repair::apply_stale_mailbox_fixes_with_post(
        snapshot,
        options,
        crate::cli::client::post_json_value,
    )
}

pub(super) fn print_fix_actions(actions: &[FixAction]) {
    if actions.is_empty() {
        return;
    }

    println!("Applying safe fixes");
    for action in actions {
        let label = if action.skipped {
            "SKIPPED"
        } else if action.ok {
            "APPLIED"
        } else {
            "FAILED"
        };
        let icon = if action.skipped {
            "!"
        } else if action.ok {
            "✓"
        } else {
            "✗"
        };
        println!("  {icon} [{label}] {}: {}", action.name, action.detail);
        if let Some(reason) = &action.skipped_reason {
            println!("      → {}", reason);
        }
    }
    println!();
}
