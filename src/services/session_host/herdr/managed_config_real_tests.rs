use super::*;
use crate::config::session_hosts::{BootSessionHosts, EndpointConfig};
use crate::services::session_host::herdr::provenance::managed_config::{
    ManagedHome, path::PersonalPaths,
};

fn managed(server: &RealServer) -> ManagedHome {
    let user = server.dir.join("isolated-user");
    std::fs::create_dir_all(&user).unwrap();
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some("mac-mini".into());
    config.session_hosts.herdr.endpoints.insert(
        "isolated".into(),
        EndpointConfig {
            execution_node: "mac-mini".into(),
            socket_path: server.dir.join("h.sock"),
            herdr_home: server.dir.join("home"),
            herdr_session: "adk".into(),
        },
    );
    ManagedHome::local(
        &BootSessionHosts::from_config(&config).unwrap(),
        &PersonalPaths {
            home: user,
            xdg: None,
        },
    )
    .unwrap()
    .remove(0)
}

fn reading(server: &RealServer) -> RestoreResume {
    let endpoint = server.endpoint();
    let transport =
        HerdrSocketTransport::new(&endpoint, HerdrSocketConfig::default(), LineJsonFraming);
    observe::read_restore_resume(&transport, &endpoint)
}

fn stop(server: &RealServer) {
    let child = server.child.lock().unwrap().take().unwrap();
    shut_down_server(&server.bin, &server.dir, child);
}

fn start_after_publication(server: &RealServer) {
    std::thread::sleep(Duration::from_millis(2_200));
    *server.child.lock().unwrap() = Some(spawn_server(&server.bin, &server.dir));
}

pub(super) fn missing_config_writer_then_later_real_server_is_off() {
    let server = RealServer::start(
        "managed-missing",
        b"[session]\nresume_agents_on_restore = true\n",
    );
    stop(&server);
    std::fs::remove_file(server.dir.join("home/config.toml")).unwrap();
    managed(&server).ensure().unwrap();
    start_after_publication(&server);
    assert!(matches!(reading(&server), RestoreResume::Off { .. }));
}

pub(super) fn running_real_server_stays_unverified_until_a_later_start() {
    let server = RealServer::start(
        "managed-running",
        b"[session]\nresume_agents_on_restore = true\n",
    );
    let pid = server.pid();
    assert!(matches!(reading(&server), RestoreResume::Unverified(_)));
    managed(&server).ensure().unwrap();
    for _ in 0..2 {
        assert_eq!(server.pid(), pid);
        assert_eq!(
            reading(&server),
            RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart)
        );
        std::thread::sleep(Duration::from_millis(1_100));
    }
    stop(&server);
    start_after_publication(&server);
    assert!(matches!(reading(&server), RestoreResume::Off { .. }));
}

#[test]
fn boot_snapshot_installation_keeps_managed_config_untouched() {
    const TEST: &str = "services::session_host::herdr::launch_host::real_tests::managed_config::boot_snapshot_installation_keeps_managed_config_untouched";
    if std::env::var_os("ADK_CONFIG_BOOT_PROBE").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("ADK_CONFIG_BOOT_PROBE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("managed");
    std::fs::create_dir(&home).unwrap();
    let path = home.join("config.toml");
    std::fs::write(&path, b"original unsafe config").unwrap();
    let before = std::fs::metadata(&path).unwrap();
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some("local".into());
    config.session_hosts.herdr.endpoints.insert(
        "isolated".into(),
        EndpointConfig {
            execution_node: "local".into(),
            socket_path: home.join("isolated.sock"),
            herdr_home: home.clone(),
            herdr_session: "adk".into(),
        },
    );
    crate::bootstrap::install_boot_snapshots(&config).unwrap();
    use std::os::unix::fs::MetadataExt;
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(
        (before.ino(), before.modified().unwrap()),
        (after.ino(), after.modified().unwrap())
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"original unsafe config");
    assert_eq!(std::fs::read_dir(home).unwrap().count(), 1);
}
