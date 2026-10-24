use super::*;

#[test]
fn doctor_separates_fresh_ready_from_startup_restart_and_does_not_fix_servers() {
    let mut cfg = config::Config::default();
    let snapshot = |body| HealthSnapshot {
        base: "http://fixture.invalid".into(),
        body: Some(body),
        error: None,
    };
    assert!(
        user_default_check(&cfg, &snapshot(json!({}))).is_none(),
        "legacy_doctor_has_no_user_server_check_contract"
    );
    cfg.session_hosts.herdr.endpoints.insert(
        "user".into(),
        crate::config::session_hosts::EndpointConfig {
            mode: crate::config::session_hosts::EndpointMode::UserDefault,
            execution_node: "worker".into(),
            socket_path: "/no-server/herdr.sock".into(),
            herdr_home: Default::default(),
            herdr_session: "default".into(),
        },
    );
    let check = user_default_check(
        &cfg,
        &snapshot(json!({"herdr": {"endpoints": {"user": {"policy": {
            "next_restart_ready": true, "startup_proof": "awaiting_server_restart"
        }}}}})),
    )
    .unwrap();
    assert_eq!(
        check.status,
        CheckStatus::Pass,
        "doctor_fresh_ready_does_not_require_historical_startup_contract"
    );
    assert_eq!(check.fix_safety, FixSafety::NotFixable);
    let refused = user_default_check(
        &cfg,
        &snapshot(json!({"herdr": {"migration_refused": true}})),
    )
    .unwrap();
    assert_eq!(
        refused.status,
        CheckStatus::Warn,
        "doctor_refusal_never_promoted_ready_contract"
    );
    assert!(refused.guidance.unwrap().contains("execution node"));
}
