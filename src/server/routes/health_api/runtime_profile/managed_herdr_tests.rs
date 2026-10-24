use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::config::session_hosts::{
    BootSessionHosts, EndpointConfig, EndpointMode, force_for_test, with_boot,
};
use crate::services::session_host::herdr_socket_rig_tests::{NativeHerdrRig, native_server_child};
use crate::services::session_host::managed_config::{maintenance, path::PersonalPaths};

type Snapshot = BTreeMap<PathBuf, (Vec<u8>, u64, i64, i64)>;

fn snapshot(dir: &Path) -> Snapshot {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let bytes = fs::read(&path).unwrap_or_default();
            (path, (bytes, meta.ino(), meta.mtime(), meta.mtime_nsec()))
        })
        .collect()
}

/// Health shows the upkeep's restart-required cause for a server still on the old config, read
/// from memory: the GET neither touches the managed home nor dials the server.
#[test]
fn health_shows_restart_required_without_touching_the_home_or_the_server() {
    const TEST: &str = "server::routes::health_api::runtime_profile::managed_herdr::tests::health_shows_restart_required_without_touching_the_home_or_the_server";
    if native_server_child() {
        return;
    }
    let rig = NativeHerdrRig::start(TEST);
    fs::write(
        rig.home().join("config.toml"),
        b"[session]\nresume_agents_on_restore = true\n",
    )
    .unwrap();
    let user = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some("health-node".into());
    config.session_hosts.herdr.endpoints.insert(
        "health-node-endpoint".into(),
        EndpointConfig {
            mode: EndpointMode::LegacyDedicated,
            execution_node: "health-node".into(),
            socket_path: rig.socket(),
            herdr_home: rig.home(),
            herdr_session: "agentdesk".into(),
        },
    );
    let boot = BootSessionHosts::from_config(&config).unwrap();
    let personal = PersonalPaths {
        home: user.path().to_path_buf(),
        xdg: None,
    };
    maintenance::pass(&boot, &personal, true);
    let _hosts = force_for_test(Some("health-node"), &[(41, "health-node")]);
    let mut live = crate::config::Config::default();
    live.session_hosts = with_boot(|boot| boot.unwrap().config().clone());
    let home = snapshot(&rig.home());
    let requests = rig.requests().len();

    let view = super::super::herdr_health(&live);

    let endpoint = &view["endpoints"]["health-node-endpoint"];
    assert_eq!(endpoint["last_e7"], "unverified:ConfigChangedSinceStart");
    assert_eq!(endpoint["managed_config"]["config"], "prepared");
    assert_eq!(
        endpoint["managed_config"]["reason"],
        "herdr_config_restart_required"
    );
    assert!(endpoint["managed_config"]["earliest_start_after"].is_string());
    assert_eq!(
        view["managed_config_reasons"],
        json!(["herdr_config_restart_required"])
    );
    assert_eq!(
        view["restart_required"], false,
        "boot drift keeps its meaning"
    );
    assert_eq!(
        snapshot(&rig.home()),
        home,
        "health wrote to the managed home"
    );
    assert_eq!(rig.requests().len(), requests, "health dialled the server");
}
