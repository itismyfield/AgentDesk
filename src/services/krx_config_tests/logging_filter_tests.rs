//! Default tracing filter test that logs on the KE api-caller target, moved out of KRX logging.

use crate::logging::tests::capture_logs_with_default_filter;

#[test]
fn default_agentdesk_filter_keeps_observability_targets_and_drops_policy_target() {
    let logs = capture_logs_with_default_filter(|| {
        tracing::info!(
            target: crate::api_caller_observability::LOG_TARGET,
            "api caller production filter marker"
        );
        tracing::info!(
            target: "policy",
            "policy production filter marker"
        );
    });

    assert!(
        logs.contains("api caller production filter marker"),
        "logs={logs}"
    );
    assert!(
        logs.contains(crate::api_caller_observability::LOG_TARGET),
        "logs={logs}"
    );
    assert!(
        !logs.contains("policy production filter marker"),
        "logs={logs}"
    );
    assert!(!logs.contains("policy"), "logs={logs}");
}
