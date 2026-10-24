use std::process::Command;

#[path = "runtime_root_guard/process_tests.rs"]
mod process_tests;

#[test]
fn guard_keeps_approval_debug_and_children_in_tmp_and_closes_after_drop() {
    use super::session_hosts::approval_store::{ApprovalStore, FileApprovalStore};

    const CELL: &str = "ADK_GUARDED_RUNTIME_ROOT_PROBE";
    const TEST: &str = "config::runtime_root_tests::guard_keeps_approval_debug_and_children_in_tmp_and_closes_after_drop";
    match std::env::var(CELL).as_deref() {
        Ok("inherited") => {
            assert_eq!(
                super::runtime_root().unwrap(),
                std::path::PathBuf::from(std::env::var_os("ADK_EXPECT_RUNTIME_ROOT").unwrap())
            );
            return;
        }
        Ok("guarded") => {
            assert!(std::env::var_os("AGENTDESK_ROOT_DIR").is_none());
            {
                let _root = super::TestRuntimeRootGuard::new();
                let root = super::runtime_root().unwrap();
                let config: super::Config = serde_json::from_value(serde_json::json!({
                    "server": {}, "cluster": {"instance_id": "test-node"},
                    "agents": [{"id": "fixture", "name": "fixture", "channels": {
                        "claude": {"id": "41", "runtime": "tui"}
                    }}], "session_hosts": {"herdr": {
                        "endpoints": {"isolated": {"execution_node": "test-node", "socket_path": root.join("herdr.sock"),
                            "herdr_home": root.join("managed"), "herdr_session": "agentdesk"}},
                        "channels": {"41": "isolated"}
                    }}
                })).unwrap();
                let hosts = super::session_hosts::BootSessionHosts::from_config(&config).unwrap();
                let store = FileApprovalStore::runtime();
                assert_eq!(
                    store.0.as_ref().unwrap(),
                    &root.join("runtime/session_hosts_approval")
                );
                if crate::services::discord::runtime_store::PARENT_DIR_FSYNC_FLUSHES {
                    store.save(&hosts).unwrap();
                    let snapshot: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(root.join("runtime/session_hosts_approval/current.json"))
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        snapshot["projection"]["hosts"]["herdr"]["endpoints"]["isolated"]["herdr_home"],
                        root.join("managed").to_str().unwrap()
                    );
                }
                crate::services::claude::debug_log_to("claude_tui.log", "guarded fixture");
                assert!(
                    std::fs::read_to_string(root.join("debug/claude_tui.log"))
                        .unwrap()
                        .contains("guarded fixture")
                );
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(CELL, "inherited")
                    .env("ADK_EXPECT_RUNTIME_ROOT", &root)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
            }
            assert!(std::panic::catch_unwind(super::runtime_root).is_err());
            return;
        }
        _ => {}
    }
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(CELL, "guarded")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("AGENTDESK_ROOT_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    assert!(!home.path().join(".adk").exists());
}

#[test]
fn missing_or_empty_runtime_root_fails_before_returning_a_path() {
    const CELL: &str = "ADK_RUNTIME_ROOT_PROBE";
    if std::env::var_os(CELL).is_some() {
        let _lock = super::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _marker =
            super::TestEnvVarGuard::capture_after_shared_test_env_lock("AGENTDESK_TEST_RUNTIME");
        // SAFETY: this isolated child holds the shared environment lock.
        unsafe { std::env::remove_var("AGENTDESK_TEST_RUNTIME") };
        let result = std::panic::catch_unwind(super::runtime_root);
        assert!(
            result.is_err(),
            "a test must not resolve the release fallback: {result:?}"
        );
        return;
    }
    let home = tempfile::tempdir().unwrap();
    for value in [None, Some(""), Some(" \t ")] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "config::runtime_root_tests::missing_or_empty_runtime_root_fails_before_returning_a_path", "--nocapture"])
            .env(CELL, "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("AGENTDESK_ROOT_DIR");
        if let Some(value) = value {
            command.env("AGENTDESK_ROOT_DIR", value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "override={value:?}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    }
    assert!(!home.path().join(".adk").exists());
}
