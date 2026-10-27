use super::*;

fn config(node: &str) -> Config {
    serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": { "instance_id": node },
        "session_hosts": {"herdr": {"endpoints": {"user": {
            "mode": "user_default", "execution_node": "worker"
        }}}}
    }))
    .unwrap()
}

#[test]
fn user_default_resolves_only_the_execution_users_socket() {
    let mut gateway = config("gateway");
    let consulted = std::cell::Cell::new(false);
    resolve(&mut gateway, || {
        consulted.set(true);
        Some("/gateway-home".into())
    })
    .unwrap();
    assert!(
        !consulted.get(),
        "remote_default_never_reads_gateway_home_contract"
    );
    assert!(
        gateway.session_hosts.herdr.endpoints["user"]
            .socket_path
            .as_os_str()
            .is_empty(),
        "remote_user_default_socket_stays_logical_contract"
    );
    let mut worker = config("worker");
    // An absolute execution-user HOME takes a drive prefix on Windows.
    let home = if cfg!(windows) {
        "C:/execution-user"
    } else {
        "/execution-user"
    };
    resolve(&mut worker, || Some(home.into())).unwrap();
    assert_eq!(
        worker.session_hosts.herdr.endpoints["user"].socket_path,
        PathBuf::from(home).join(".config/herdr/herdr.sock"),
        "execution_user_home_socket_contract"
    );
    assert_eq!(
        worker.session_hosts.herdr.endpoints["user"].herdr_session,
        "default"
    );
    assert!(resolve(&mut config("worker"), || Some("relative".into())).is_err());
}

#[test]
fn legacy_and_explicit_user_sockets_do_not_read_home() {
    let mut explicit = config("worker");
    explicit
        .session_hosts
        .herdr
        .endpoints
        .get_mut("user")
        .unwrap()
        .socket_path = "/exact/socket".into();
    resolve(&mut explicit, || {
        panic!("explicit socket needs no fallback HOME")
    })
    .unwrap();
    assert_eq!(
        explicit.session_hosts.herdr.endpoints["user"].socket_path,
        PathBuf::from("/exact/socket")
    );
    let mut legacy = config("worker");
    legacy
        .session_hosts
        .herdr
        .endpoints
        .get_mut("user")
        .unwrap()
        .mode = EndpointMode::LegacyDedicated;
    let before = legacy.session_hosts.clone();
    let consulted = std::cell::Cell::new(false);
    resolve(&mut legacy, || {
        consulted.set(true);
        Some("/wrong-fallback".into())
    })
    .unwrap();
    assert!(
        !consulted.get(),
        "legacy_does_not_consult_user_default_home_contract"
    );
    assert_eq!(
        legacy.session_hosts, before,
        "legacy_endpoint_defaults_diff_zero_contract"
    );
}
