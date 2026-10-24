use super::*;

#[test]
fn startup_health_strips_raw_checker_evidence_without_changing_the_doctor_artifact() {
    let raw = "private checker output".repeat(200);
    let report = json!({"checks": [{
        "id": "herdr_user_default", "status": "warn", "detail": "bounded health excerpt",
        "evidence": {"endpoints": {"user": {"policy": {"checker": "bounded health excerpt"}}},
            "checker_evidence": {"user": {"stderr": raw}}}
    }, {"id": "other_check", "status": "fail", "evidence": {"raw": "unrelated"}}]});
    let before = report.clone();
    let health =
        startup_doctor_summary_json(std::path::Path::new("/fixture/doctor.json"), &report, true);
    assert!(
        !health["warned_checks"][0]["evidence"]
            .as_object()
            .unwrap()
            .contains_key("checker_evidence"),
        "startup_health_never_reexports_raw_herdr_checker_evidence_contract"
    );
    assert!(!health.to_string().contains("private checker output"));
    assert_eq!(
        health["warned_checks"][0]["detail"],
        "bounded health excerpt"
    );
    assert_eq!(health["failed_checks"][0]["evidence"]["raw"], "unrelated");
    assert_eq!(
        report, before,
        "doctor_artifact_keeps_raw_checker_evidence_contract"
    );
}

#[test]
fn protected_doctor_response_includes_current_checker_evidence_when_no_artifact_exists() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut config = crate::config::Config::default();
    config.session_hosts.herdr.endpoints.insert(
        "user".into(),
        crate::config::session_hosts::EndpointConfig {
            mode: crate::config::session_hosts::EndpointMode::UserDefault,
            execution_node: "fixture".into(),
            socket_path: "/no-server/protected-doctor.sock".into(),
            herdr_home: Default::default(),
            herdr_session: "default".into(),
        },
    );
    crate::services::session_host::policy::diagnostics::checker(
        std::path::Path::new("/no-server/protected-doctor.sock"),
        b"checker stdout",
        b"checker stderr",
    );
    let response = doctor_json(&config);
    assert_eq!(
        response["herdr_checker"]["user"]["stderr"], "checker stderr",
        "protected_doctor_carries_current_raw_checker_evidence_contract"
    );
    assert!(!response["available"].as_bool().unwrap());
    config.session_hosts.herdr.endpoints.clear();
    assert!(
        doctor_json(&config).get("herdr_checker").is_none(),
        "legacy_doctor_response_adds_no_checker_evidence_contract"
    );
}
