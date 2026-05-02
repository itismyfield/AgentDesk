use serde_json::{Value, json};

use super::contract::{FixSafety, SecurityExposure, Severity};

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
        ["db_unavailable"] => ClassifiedReason {
            raw: raw.to_string(),
            subsystem: "postgres",
            severity: Severity::Error,
            fix_safety: FixSafety::NotFixable,
            security_exposure: SecurityExposure::OperationalMetadata,
            summary: "database is unavailable".to_string(),
            next_step: "check Postgres/SQLite availability and server logs".to_string(),
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::is_loopback_base_url;

    #[test]
    fn loopback_base_url_accepts_http_and_https_local_targets() {
        assert!(is_loopback_base_url("http://127.0.0.1:8791"));
        assert!(is_loopback_base_url("https://127.0.0.1:8791/api"));
        assert!(is_loopback_base_url("http://localhost"));
        assert!(is_loopback_base_url("https://localhost:8791"));
        assert!(is_loopback_base_url("http://[::1]:8791"));
        assert!(is_loopback_base_url("https://[::1]/api/health"));
    }

    #[test]
    fn loopback_base_url_rejects_remote_and_unsupported_targets() {
        assert!(!is_loopback_base_url("http://10.0.0.5:8791"));
        assert!(!is_loopback_base_url("https://example.com"));
        assert!(!is_loopback_base_url("http://localhost.evil.example:8791"));
        assert!(!is_loopback_base_url("http://127.0.0.1.evil.example:8791"));
        assert!(!is_loopback_base_url("ftp://localhost"));
    }
}
