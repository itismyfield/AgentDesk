use super::snapshot_reader_support::*;
use super::*;

#[test]
fn snapshot_facades_keep_published_value_and_preserve_boot_fallback() {
    isolated(|| {
        let fixture = Fixture::new();
        assert!(matches!(
            crate::config::shared_test_env_lock().try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        // A retired top-level `meeting` key still loads and is ignored.
        fixture.write("server: {port: 41}\nmeeting: {channel_name: fixture, available_agents: []}");
        for config in [
            runtime_config(),
            runtime_config_strict().unwrap(),
            runtime_config_optional().unwrap(),
        ] {
            assert_eq!(config.server.port, 41);
            assert_eq!(config.runtime_source.unwrap().path, fixture.path);
        }
        fixture.install("server: {port: 41}", "server: {port: 73}");
        assert_eq!(runtime_config().server.port, 41);
        assert_eq!(runtime_config_strict().unwrap().server.port, 41);
        assert_eq!(runtime_config_optional().unwrap().server.port, 41);
        fixture.reload();
        assert_eq!(runtime_config().server.port, 73);
    });
}

#[cfg(unix)]
#[test]
fn snapshot_facades_do_not_touch_disk_after_publication() {
    use std::os::unix::fs::PermissionsExt;
    isolated(|| {
        let fixture = Fixture::new();
        let unread_data = fixture.root().join("unread-config-data");
        let disk = serde_yaml::to_string(&serde_json::json!({
            "server": {"port": 73, "auth_token": "synthetic-disk-secret"},
            "data": {"dir": unread_data},
        }))
        .unwrap();
        fixture.install("server: {port: 41}", &disk);
        std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        for config in [
            runtime_config(),
            runtime_config_strict().unwrap(),
            runtime_config_optional().unwrap(),
        ] {
            assert_eq!(config.server.port, 41);
        }
        assert!(
            !unread_data.exists(),
            "an eager discarded load must not create the disk config's data directory"
        );
        assert_eq!(
            std::fs::metadata(&fixture.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "an eager discarded optional load must not audit the unread secret file"
        );
    });
}
