use super::*;

fn fixture(instance_id: Option<&str>, execution_node: &str) -> crate::config::Config {
    serde_json::from_value(serde_json::json!({
        "server": {}, "cluster": {"instance_id": instance_id},
        "agents": [{"id": "fixture", "name": "fixture", "channels": {
            "claude": {"id": "41", "runtime": "tui"}
        }}],
        "session_hosts": {"herdr": {
            "endpoints": {"one": {"mode": "user_default", "execution_node": execution_node,
                "socket_path": "/fixture/herdr.sock", "herdr_session": "agentdesk"}},
            "channels": {"41": "one"}
        }}
    }))
    .unwrap()
}

#[test]
fn user_default_validator_admits_endpoints_where_this_platform_has_a_loaded_image_adapter() {
    let adapter = crate::services::session_host::policy::peer::SUPPORTED;
    for (node, label) in [("test-node", "local"), ("other-node", "remote")] {
        let config = fixture(Some("test-node"), node);
        let before = crate::services::herdr_launch::admissions_on_this_thread();
        let result = crate::config::validate_config(&config);
        assert_eq!(
            crate::services::herdr_launch::admissions_on_this_thread() - before,
            0,
            "validator_reaches_no_launch_admission_contract"
        );
        let refused = label == "local" && !adapter;
        assert_eq!(result.is_err(), refused, "{label} endpoint: {result:?}");
        if let Err(error) = result {
            let error = error.to_string();
            for required in [
                "endpoints.one.mode",
                "loaded-image adapter",
                "legacy_dedicated",
            ] {
                assert!(error.contains(required), "missing {required:?}: {error}");
            }
        }
        let approved = projection::Projection {
            hosts: config.session_hosts.clone(),
            local_node: Some("test-node".into()),
        }
        .into_hosts()
        .expect("an approved user-default projection decodes for rollback decisions");
        assert_eq!(approved.channels()[&41].key, "one");
    }
}

#[test]
fn user_default_without_an_adapter_is_refused_unless_its_node_is_known_to_be_remote() {
    for (instance_id, refused) in [
        (Some("test-node"), true),
        (Some(" test-node "), true),
        (None, true),
        (Some("  "), true),
        (Some("other-node"), false),
    ] {
        let config = fixture(instance_id, "test-node");
        assert_eq!(
            user_default_platform(&config, false).is_err(),
            refused,
            "instance_id {instance_id:?}"
        );
        assert!(user_default_platform(&config, true).is_ok());
    }
    let mut legacy = fixture(Some("test-node"), "test-node");
    let endpoint = legacy.session_hosts.herdr.endpoints.get_mut("one").unwrap();
    endpoint.mode = EndpointMode::LegacyDedicated;
    assert!(user_default_platform(&legacy, false).is_ok());
}
