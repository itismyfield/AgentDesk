use serde_json::{Value, json};

use super::contract::{FixSafety, SecurityExposure, Severity};
use super::startup::LATEST_STARTUP_DOCTOR_ENDPOINT;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClassifiedReason {
    pub(crate) raw: String,
    pub(crate) subsystem: &'static str,
    pub(crate) severity: Severity,
    pub(crate) fix_safety: FixSafety,
    pub(crate) security_exposure: SecurityExposure,
    pub(crate) summary: String,
    pub(crate) next_step: String,
}

fn startup_doctor_report_next_step() -> String {
    format!("inspect the startup doctor report via {LATEST_STARTUP_DOCTOR_ENDPOINT}")
}

fn format_bytes(bytes_str: &str) -> String {
    bytes_str
        .parse::<u64>()
        .ok()
        .map(|b| {
            const KB: u64 = 1024;
            const MB: u64 = KB * 1024;
            const GB: u64 = MB * 1024;
            if b >= GB {
                format!("{:.1} GB", b as f64 / GB as f64)
            } else if b >= MB {
                format!("{:.1} MB", b as f64 / MB as f64)
            } else if b >= KB {
                format!("{:.1} KB", b as f64 / KB as f64)
            } else {
                format!("{b} B")
            }
        })
        .unwrap_or_else(|| bytes_str.to_string())
}

pub(crate) fn classify_degraded_reason(raw: &str) -> ClassifiedReason {
    let parts: Vec<&str> = raw.split(':').collect();
    match parts.as_slice() {
        ["provider", provider, "pending_queue_depth", depth] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_runtime",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("provider {provider} has pending queue depth {depth}"),
            next_step: format!("inspect provider {provider} active turn and queue state"),
        },
        ["provider", provider, "disconnected"] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_runtime",
            severity: Severity::Error,
            fix_safety: FixSafety::ExplicitRestartRequired,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("provider {provider} is disconnected"),
            next_step: format!("check {provider} Discord token, gateway status, and dcserver logs"),
        },
        ["provider", provider, "restart_pending"] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_runtime",
            severity: Severity::Error,
            fix_safety: FixSafety::ExplicitRestartRequired,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("provider {provider} restart is pending"),
            next_step: "wait for restart completion or run explicit operator restart".to_string(),
        },
        ["provider", provider, "reconcile_in_progress"] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_runtime",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("provider {provider} reconcile is still in progress"),
            next_step: "wait for reconcile completion before dispatching new work".to_string(),
        },
        ["provider", provider, "deferred_hooks_backlog", count] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_runtime",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("provider {provider} has deferred hook backlog {count}"),
            next_step: format!("inspect deferred hook backlog for provider {provider}"),
        },
        ["provider", provider, "recovering_channels", count] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_runtime",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("provider {provider} is recovering {count} channel(s)"),
            next_step: "wait for recovery or inspect stale recovery markers".to_string(),
        },
        ["opencode_warm_server", "suspicious_active_leak", count] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_runtime",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!(
                "{count} resident OpenCode warm server(s) show suspicious active-session evidence"
            ),
            next_step: "inspect opencode.warm_servers[] in /api/health/detail; this is evidence-only, no repair runs in P0".to_string(),
        },
        ["opencode_warm_server", "stopped_resident", count] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_runtime",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!(
                "{count} resident OpenCode warm server(s) have a stopped process and will be evicted on next reuse"
            ),
            next_step: "inspect opencode.warm_servers[] running/pid fields in /api/health/detail".to_string(),
        },
        ["dispatch_outbox_oldest_pending_age", age] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "dispatch_outbox",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("dispatch outbox oldest pending age is {age}s"),
            next_step: "inspect dispatch outbox retry worker and delivery failures".to_string(),
        },
        ["pipeline_override_warnings", count] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "config_audit",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("pipeline override warnings count is {count}"),
            next_step: "run config audit and inspect override report".to_string(),
        },
        [
            "global_active_counter_out_of_bounds",
            raw_val,
            prov_val,
            fin_val,
        ] => {
            let raw_clean = raw_val.strip_prefix("raw=").unwrap_or(raw_val);
            let prov_clean = prov_val
                .strip_prefix("provider_active_turns=")
                .unwrap_or(prov_val);
            let fin_clean = fin_val
                .strip_prefix("global_finalizing=")
                .unwrap_or(fin_val);
            ClassifiedReason {
                raw: raw.to_string(),
                subsystem: "health",
                severity: Severity::Warning,
                fix_safety: FixSafety::ReadOnly,
                security_exposure: SecurityExposure::OperationalMetadata,
                summary: format!(
                    "global active counter out of bounds (raw: {raw_clean}, provider: {prov_clean}, finalizing: {fin_clean})"
                ),
                next_step: "inspect global active counter tracking in dcserver logs".to_string(),
            }
        }
        ["no_providers_registered"] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "provider_registry",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: "no providers are currently registered".to_string(),
            next_step: "register a provider via the dashboard or check agentdesk.yaml".to_string(),
        },
        ["startup_doctor_failed", count] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "startup_doctor",
            severity: Severity::Error,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("startup doctor reported {count} failure(s)"),
            next_step: startup_doctor_report_next_step(),
        },
        ["startup_doctor_warned", count] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "startup_doctor",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("startup doctor reported {count} warning(s)"),
            next_step: startup_doctor_report_next_step(),
        },
        ["disk_low_free_bytes", bytes] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "disk",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!("disk has low free bytes: {}", format_bytes(bytes)),
            next_step: "free up disk space or increase disk capacity".to_string(),
        },
        ["db_unavailable"] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "postgres",
            severity: Severity::Error,
            fix_safety: FixSafety::NotFixable,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: "database is unavailable".to_string(),
            next_step: "check Postgres/SQLite availability and server logs".to_string(),
        },
        // #4515 PR2: worker-local recovery circuit reasons.
        ["worker_local_restart_budget_exhausted", worker] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "worker_recovery",
            severity: Severity::Error,
            fix_safety: FixSafety::ExplicitRestartRequired,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!(
                "worker-local worker {worker} exhausted its restart budget and is permanently stopped"
            ),
            next_step: format!(
                "inspect dcserver logs for {worker} crash cause; the process exits for launchd KeepAlive restart unless the cross-process crash-loop guard held it"
            ),
        },
        ["worker_local_loop_owned_terminated", worker] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "worker_recovery",
            severity: Severity::Warning,
            fix_safety: FixSafety::ExplicitRestartRequired,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!(
                "un-migrated LoopOwned worker {worker} terminated unexpectedly and is not auto-restarted"
            ),
            next_step: format!(
                "inspect dcserver logs for {worker}; a dcserver restart is required to recover it"
            ),
        },
        ["worker_local_restart_flapping", worker, count] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "worker_recovery",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: format!(
                "worker-local worker {worker} restarted {count} time(s) within the budget window"
            ),
            next_step: format!("inspect dcserver logs for repeated {worker} exits"),
        },
        _ => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "health",
            severity: Severity::Warning,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: raw.to_string(),
            next_step: "inspect detailed health payload".to_string(),
        },
    }
}

pub(crate) fn degraded_reasons(body: &Value) -> Vec<ClassifiedReason> {
    body.get("degraded_reasons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(classify_degraded_reason)
        .collect()
}

pub(crate) fn reasons_evidence(reasons: &[ClassifiedReason]) -> Value {
    json!({
        "degraded_reasons": reasons
            .iter()
            .map(|reason| {
                json!({
                    "raw": reason.raw,
                    "subsystem": reason.subsystem,
                    "severity": reason.severity.as_str(),
                    "next_step": reason.next_step,
                })
            })
            .collect::<Vec<_>>()
    })
}

pub(crate) fn is_loopback_base_url(base: &str) -> bool {
    crate::utils::loopback_url::is_loopback_url(base, None)
}

#[cfg(test)]
mod health_classification_tests {
    use super::super::contract::{FixSafety, Severity};
    use super::{LATEST_STARTUP_DOCTOR_ENDPOINT, classify_degraded_reason};

    #[test]
    fn startup_doctor_reasons_point_to_latest_report_endpoint() {
        let expected_next_step =
            format!("inspect the startup doctor report via {LATEST_STARTUP_DOCTOR_ENDPOINT}");

        let failed = classify_degraded_reason("startup_doctor_failed:2");
        assert_eq!(failed.subsystem, "startup_doctor");
        assert_eq!(failed.summary, "startup doctor reported 2 failure(s)");
        assert_eq!(failed.next_step.as_str(), expected_next_step.as_str());

        let warned = classify_degraded_reason("startup_doctor_warned:3");
        assert_eq!(warned.subsystem, "startup_doctor");
        assert_eq!(warned.summary, "startup doctor reported 3 warning(s)");
        assert_eq!(warned.next_step.as_str(), expected_next_step.as_str());
    }

    #[test]
    fn disk_low_free_bytes_reason_formats_bytes() {
        let reason = classify_degraded_reason("disk_low_free_bytes:104857600");
        assert_eq!(reason.subsystem, "disk");
        assert_eq!(reason.summary, "disk has low free bytes: 100.0 MB");

        let reason_invalid = classify_degraded_reason("disk_low_free_bytes:invalid");
        assert_eq!(reason_invalid.summary, "disk has low free bytes: invalid");
    }

    /// [TEST-004] resident warm-pool reason codes classify to provider_runtime
    /// (Warning/ReadOnly) and are NOT the generic catch-all, and are distinct
    /// from the fresh-serve probe check.
    #[test]
    fn opencode_warm_server_reason_codes_classify_without_serve_overlap() {
        let leak = classify_degraded_reason("opencode_warm_server:suspicious_active_leak:2");
        assert_eq!(leak.subsystem, "provider_runtime");
        assert_eq!(leak.severity, Severity::Warning);
        assert_eq!(leak.fix_safety, FixSafety::ReadOnly);
        assert!(leak.summary.contains('2'));
        // Not the generic catch-all (which echoes the raw string as summary).
        assert_ne!(leak.summary, leak.raw);

        let stopped = classify_degraded_reason("opencode_warm_server:stopped_resident:1");
        assert_eq!(stopped.subsystem, "provider_runtime");
        assert_eq!(stopped.severity, Severity::Warning);
        assert_eq!(stopped.fix_safety, FixSafety::ReadOnly);
        assert_ne!(stopped.summary, stopped.raw);
    }

    #[test]
    fn worker_recovery_reason_codes_classify() {
        // #4515 PR2: budget exhaustion is a fatal, restart-required error.
        let exhausted =
            classify_degraded_reason("worker_local_restart_budget_exhausted:dispatch_outbox");
        assert_eq!(exhausted.subsystem, "worker_recovery");
        assert_eq!(exhausted.severity, Severity::Error);
        assert_eq!(exhausted.fix_safety, FixSafety::ExplicitRestartRequired);
        assert!(exhausted.summary.contains("dispatch_outbox"));
        assert_ne!(exhausted.summary, exhausted.raw);

        // An un-migrated LoopOwned worker death is a warning needing a restart.
        let terminated =
            classify_degraded_reason("worker_local_loop_owned_terminated:watcher_supervisor");
        assert_eq!(terminated.subsystem, "worker_recovery");
        assert_eq!(terminated.severity, Severity::Warning);
        assert_eq!(terminated.fix_safety, FixSafety::ExplicitRestartRequired);
        assert!(terminated.summary.contains("watcher_supervisor"));
        assert_ne!(terminated.summary, terminated.raw);

        // Flapping is read-only informational.
        let flapping =
            classify_degraded_reason("worker_local_restart_flapping:session_discovery:3");
        assert_eq!(flapping.subsystem, "worker_recovery");
        assert_eq!(flapping.severity, Severity::Warning);
        assert_eq!(flapping.fix_safety, FixSafety::ReadOnly);
        assert!(flapping.summary.contains("session_discovery"));
        assert!(flapping.summary.contains('3'));
        assert_ne!(flapping.summary, flapping.raw);
    }

    #[test]
    fn global_active_counter_reason_is_actionable() {
        let reason = classify_degraded_reason(
            "global_active_counter_out_of_bounds:raw=4:provider_active_turns=2:global_finalizing=1",
        );

        assert_eq!(reason.subsystem, "health");
        assert_eq!(
            reason.summary,
            "global active counter out of bounds (raw: 4, provider: 2, finalizing: 1)"
        );
        assert_eq!(
            reason.next_step,
            "inspect global active counter tracking in dcserver logs"
        );
    }
}
