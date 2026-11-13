use super::*;
use std::os::unix::fs::PermissionsExt;

const PROBE: &str = "ADK_TEST_DECISION_STAGE";
const TEST: &str = "cli::dcserver::decision_boot::stage_tests::unstaged_boot_skips_lookup_and_staged_missing_key_is_consumed_once";

#[test]
fn unstaged_boot_skips_lookup_and_staged_missing_key_is_consumed_once() {
    if let Some(marker) = std::env::var_os(PROBE) {
        let marker = std::path::PathBuf::from(marker);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            initialize_staged().await.unwrap();
            assert!(!marker.exists());
            assert!(decision_gate::installed().is_none());
            stage(None);
            initialize_staged().await.unwrap();
            assert_eq!(std::fs::read(&marker).unwrap(), b"x");
            assert!(decision_gate::installed().is_some());
            initialize_staged().await.unwrap();
            assert_eq!(std::fs::read(&marker).unwrap(), b"x");
        });
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("calls");
    let stub = dir.path().join("agent-op");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\nprintf x >> '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env(PROBE, &marker)
        .env("PATH", dir.path())
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(marker).unwrap(), b"x");
}
