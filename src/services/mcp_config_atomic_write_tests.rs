use super::*;
use crate::domain::atomic_write::{Stage, tests::FaultGuard};

struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || Captured(output.clone()))
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let result = tracing::subscriber::with_default(subscriber, run);
    let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    (result, logs)
}

#[test]
fn opencode_publication_failures_keep_config_state_and_observations() {
    // Isolate HOME changes and the first ENOSPC observation from parallel tests.
    const CHILD: &str = "AGENTDESK_TEST_OPENCODE_PUBLICATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::mcp_config::atomic_write_tests::opencode_publication_failures_keep_config_state_and_observations",
                "--nocapture",
            ])
            .env(CHILD, "1")
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
    assert!(!crate::services::disk_monitor::enospc_recent());
    let env_lock = crate::config::shared_test_env_lock().lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        &dir.path().join("runtime-root"),
    );
    let home_guard =
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("HOME", dir.path());
    let mut config = Config::default();
    config.mcp_servers.insert(
        "fixture".into(),
        McpServerConfig {
            url: "http://fixture.test/mcp".into(),
            auth: None,
        },
    );
    let path = opencode_config_path().unwrap();
    let state = crate::runtime_layout::config_dir(&crate::config::runtime_root().unwrap())
        .join(OPENCODE_SYNC_STATE_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    let old = r#"{"mcp":{},"preserve":"old config"}"#;
    let old_state = r#"{"managed_servers":[]}"#;
    std::fs::write(&path, old).unwrap();
    std::fs::write(&state, old_state).unwrap();
    for stage in [
        Stage::CreateDir,
        Stage::CreateTemp,
        Stage::Write,
        Stage::FileSync,
        Stage::Rename,
    ] {
        for errno in [5, 28] {
            let fault = FaultGuard::new(&path, stage, errno);
            let (result, logs) = capture_logs(|| sync_opencode_mcp_servers(&config));
            if errno == 28 {
                assert!(crate::services::disk_monitor::enospc_recent());
            }
            let (previous, previous_logs) = capture_logs(|| atomic_write(&path, "new"));
            assert_eq!(result, previous, "stage={stage:?} errno={errno}");
            assert!(
                result.is_err(),
                "a failed publication must reach the caller"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
            assert_eq!(std::fs::read_to_string(&state).unwrap(), old_state);
            assert_eq!(logs.contains("ENOSPC"), errno == 28);
            assert_eq!(previous_logs.contains("ENOSPC"), errno == 28);
            assert!(!logs.contains("agentdesk::inflight_write"));
            if errno == 28 {
                assert!(
                    logs.contains("agentdesk::services::discord::runtime_store:   ["),
                    "{logs}"
                );
                let message = format!(
                    "💾 ENOSPC at runtime_store::atomic_write ({}): {}",
                    stage.prefix(),
                    std::io::Error::from_raw_os_error(errno)
                );
                assert!(logs.contains(&message), "{logs}");
                assert!(previous_logs.contains(&message), "{previous_logs}");
                assert!(crate::services::disk_monitor::enospc_recent());
            }
            drop(fault);
        }
    }
    sync_opencode_mcp_servers(&config).unwrap();
    let published: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(published["preserve"], "old config");
    assert_eq!(
        published["mcp"]["fixture"]["url"],
        "http://fixture.test/mcp"
    );
    assert_eq!(
        load_opencode_sync_state_from_path(&state).managed_servers,
        vec!["fixture"]
    );
    drop(home_guard);
    drop(root_guard);
    drop(env_lock);
}
