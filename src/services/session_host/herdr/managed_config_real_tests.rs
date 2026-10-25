use super::*;
use crate::config::session_hosts::{BootSessionHosts, EndpointConfig, EndpointMode};
use crate::services::session_host::herdr::managed_config::{ManagedHome, path::PersonalPaths};

fn managed(server: &RealServer) -> ManagedHome {
    let user = server.dir.join("isolated-user");
    std::fs::create_dir_all(&user).unwrap();
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some("mac-mini".into());
    config.session_hosts.herdr.endpoints.insert(
        "isolated".into(),
        EndpointConfig {
            mode: EndpointMode::LegacyDedicated,
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
    .remove(0)
    .home
    .unwrap()
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

/// Boot ensures only the snapshot this process installed: a refused migration keeps the approved
/// dedicated homes, so the candidate's home is never written, and a canonical home is not touched.
#[test]
fn boot_ensures_only_the_installed_snapshot_dedicated_homes() {
    const TEST: &str = "services::session_host::herdr::launch_host::real_tests::managed_config::boot_ensures_only_the_installed_snapshot_dedicated_homes";
    let Some(root) = std::env::var_os("ADK_CONFIG_BOOT_PROBE").map(std::path::PathBuf::from) else {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        std::fs::create_dir(&user).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("ADK_CONFIG_BOOT_PROBE", root.path())
            .env("AGENTDESK_ROOT_DIR", root.path().join("runtime"))
            .env("HOME", &user)
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    use crate::config::session_hosts::approval_store::{ApprovalStore, FileApprovalStore};
    use std::os::unix::fs::MetadataExt;
    let home = |name: &str, bytes: &[u8]| {
        let home = root.join(name);
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("config.toml"), bytes).unwrap();
        home
    };
    let approved = home("approved", b"original unsafe config");
    let canonical = home("canonical", CANONICAL_CONFIG);
    let candidate = home("candidate", b"candidate unsafe config");
    let config = |homes: &[(&str, &std::path::Path)], channel: &str| -> crate::config::Config {
        let endpoints: serde_json::Map<_, _> = homes
            .iter()
            .map(|(key, home)| {
                let endpoint = serde_json::json!({"execution_node": "boot-node",
                    "socket_path": home.join("herdr.sock"), "herdr_home": home,
                    "herdr_session": "agentdesk"});
                (key.to_string(), endpoint)
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "server": {}, "data": {"dir": root.join("data")},
            "cluster": {"instance_id": "boot-node"},
            "agents": [{"id": "fixture", "name": "fixture",
                "channels": {"claude": {"id": "41", "runtime": "tui"}}}],
            "session_hosts": {"herdr": {"endpoints": endpoints, "channels": {"41": channel}}}
        }))
        .unwrap()
    };
    let previous = config(
        &[("approved", &approved), ("canonical", &canonical)],
        "approved",
    );
    FileApprovalStore::runtime()
        .save(&BootSessionHosts::from_config(&previous).unwrap())
        .unwrap();
    let canonical_before = std::fs::metadata(canonical.join("config.toml")).unwrap();
    let candidate_before = std::fs::metadata(candidate.join("config.toml")).unwrap();
    let current = config(&[("candidate", &candidate)], "candidate");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(crate::bootstrap::prepare_and_install_boot_snapshots(
            &current, None,
        ))
        .unwrap();

    crate::config::session_hosts::with_boot(|boot| {
        let keys: Vec<_> = boot
            .unwrap()
            .config()
            .herdr
            .endpoints
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            ["approved", "canonical"],
            "the migration was not refused"
        );
    });
    assert_eq!(
        std::fs::read(approved.join("config.toml")).unwrap(),
        CANONICAL_CONFIG
    );
    let identity = |meta: &std::fs::Metadata| (meta.ino(), meta.mtime(), meta.mtime_nsec());
    let canonical_after = std::fs::metadata(canonical.join("config.toml")).unwrap();
    assert_eq!(identity(&canonical_after), identity(&canonical_before));
    assert_eq!(std::fs::read_dir(&canonical).unwrap().count(), 1);
    let candidate_after = std::fs::metadata(candidate.join("config.toml")).unwrap();
    assert_eq!(identity(&candidate_after), identity(&candidate_before));
    assert_eq!(
        std::fs::read(candidate.join("config.toml")).unwrap(),
        b"candidate unsafe config"
    );
    assert_eq!(std::fs::read_dir(&candidate).unwrap().count(), 1);
}
