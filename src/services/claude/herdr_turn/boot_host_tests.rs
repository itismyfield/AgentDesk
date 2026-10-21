use super::*;
use crate::config::session_hosts::EndpointMode;
use crate::services::session_host::HerdrEndpoint;

#[test]
fn boot_launch_host_forwards_endpoint_mode() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("unstarted.sock");
    let endpoint = HerdrEndpoint::new("test-node", "user", &socket, "default")
        .unwrap()
        .with_mode(EndpointMode::UserDefault);
    let launch = HerdrLaunchEndpoint {
        execution_node: "test-node".into(),
        config_key: "user".into(),
        socket_addr: socket.to_string_lossy().into_owned(),
        herdr_session: "default".into(),
    };
    let host = SocketHerdrLaunchHost::new(vec![endpoint], Default::default());
    assert_eq!(
        Boot(&host).mode(&launch),
        EndpointMode::UserDefault,
        "boot_endpoint_mode_forwarding_contract"
    );
    assert!(
        !socket.exists(),
        "reading the registered endpoint mode must not start or connect to a server"
    );
}
