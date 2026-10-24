use super::*;

#[test]
fn legacy_doctor_fetches_no_checker_evidence_and_user_default_uses_only_doctor_response() {
    let mut cfg = config::Config::default();
    let original = json!({"herdr": {"endpoints": {}}});
    let mut body = original.clone();
    attach_checker_evidence(&cfg, &mut body, || panic!("legacy extra HTTP request"));
    assert_eq!(body, original, "legacy_doctor_has_zero_extra_http_contract");
    cfg.session_hosts.herdr.endpoints.insert(
        "user".into(),
        crate::config::session_hosts::EndpointConfig {
            mode: crate::config::session_hosts::EndpointMode::UserDefault,
            execution_node: "fixture".into(),
            socket_path: "/no-server/doctor-fetch.sock".into(),
            herdr_home: Default::default(),
            herdr_session: "default".into(),
        },
    );
    let evidence = json!({"user": {"stdout": "raw stdout", "stderr": "raw stderr"}});
    attach_checker_evidence(&cfg, &mut body, || Ok(json!({"herdr_checker": evidence})));
    assert_eq!(
        body["herdr_checker"], evidence,
        "doctor_fetch_preserves_only_the_doctor_checker_evidence_contract"
    );
    let mut unavailable = original.clone();
    attach_checker_evidence(&cfg, &mut unavailable, || Err("doctor unavailable".into()));
    assert_eq!(
        unavailable, original,
        "doctor_evidence_failure_does_not_erase_health_contract"
    );
}
