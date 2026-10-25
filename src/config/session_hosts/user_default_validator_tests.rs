use super::*;

#[test]
fn user_default_validator_stays_dormant_while_its_approval_projection_decodes() {
    let mut config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {}, "cluster": {"instance_id": "test-node"},
        "agents": [{"id": "fixture", "name": "fixture", "channels": {
            "claude": {"id": "41", "runtime": "tui"}
        }}],
        "session_hosts": {"herdr": {
            "endpoints": {"one": {"execution_node": "test-node",
                "socket_path": "/fixture/herdr.sock", "herdr_home": "/fixture/herdr",
                "herdr_session": "agentdesk"}},
            "channels": {"41": "one"}
        }}
    }))
    .unwrap();
    crate::config::validate_config(&config).expect("complete legacy fixture is supported");
    config
        .session_hosts
        .herdr
        .endpoints
        .get_mut("one")
        .unwrap()
        .mode = EndpointMode::UserDefault;
    let before = crate::services::herdr_launch::admissions_on_this_thread();
    let result = crate::config::validate_config(&config);
    assert!(result.is_err(), "user_default unsupported guard contract");
    assert_eq!(
        crate::services::herdr_launch::admissions_on_this_thread() - before,
        0,
        "validator_refusal_reaches_no_launch_admission_contract"
    );
    let error = result.unwrap_err().to_string();
    for required in [
        "session_hosts.herdr.endpoints.one.mode",
        "user_default",
        "지원 조각 미착지",
        "legacy_dedicated",
        "until support is complete",
    ] {
        assert!(error.contains(required), "missing {required:?}: {error}");
    }
    let approved = projection::Projection {
        hosts: config.session_hosts.clone(),
        local_node: Some("test-node".into()),
    }
    .into_hosts()
    .expect("an approved user-default projection decodes for rollback decisions");
    assert_eq!(approved.channels()[&41].key, "one");
}
