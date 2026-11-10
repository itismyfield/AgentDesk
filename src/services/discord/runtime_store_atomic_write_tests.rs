use super::*;
use std::fs;

#[test]
fn critical_atomic_write_returns_error_for_unwritable_parent_path() {
    let tmp = tempfile::tempdir().unwrap();
    let parent_file = tmp.path().join("not-a-dir");
    fs::write(&parent_file, "blocking-file").unwrap();
    let target = parent_file.join("queue.json");

    let error = critical_publish(
        &target,
        "[]",
        AtomicWriteContext::new("discord_pending_queue")
            .provider("codex")
            .token_hash("discord_deadbeef")
            .channel_id(42),
    )
    .expect_err("critical write must expose persistence failure");

    assert!(
        error.contains("create_dir_all") || error.contains("Not a directory"),
        "unexpected critical write error: {error}"
    );
}

struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend_from_slice(bytes);
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
    let logs = String::from_utf8(
        bytes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone(),
    )
    .unwrap();
    (result, logs)
}

#[test]
fn atomic_write_stage_failures_preserve_bytes_and_observations() {
    use crate::domain::atomic_write::{Stage, tests::FaultGuard};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("discord_inflight/codex/42.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let old = r#"{"user_msg_id":7}"#;
    fs::write(&path, old).unwrap();
    for stage in [
        Stage::CreateDir,
        Stage::CreateTemp,
        Stage::Write,
        Stage::FileSync,
        Stage::Rename,
    ] {
        for errno in [5, 28] {
            let fault = FaultGuard::new(&path, stage, errno);
            let (result, logs) = capture_logs(|| atomic_write(&path, r#"{"user_msg_id":8}"#));
            let error = result.expect_err("a failed syscall must never report success");
            let raw = std::io::Error::from_raw_os_error(errno);
            let prefix = if errno == 28 { "ENOSPC: " } else { "" };
            assert_eq!(error, format!("{prefix}{}: {raw}", stage.prefix()));
            assert_eq!(fs::read_to_string(&path).unwrap(), old);
            assert_eq!(
                logs.contains("agentdesk::inflight_write"),
                stage == Stage::Rename,
                "{stage:?}: {logs}"
            );
            if stage == Stage::Rename {
                assert!(logs.contains("user_msg_id=7"), "{logs}");
                assert!(logs.contains("channel_id=42"), "{logs}");
                assert!(
                    logs.contains("runtime_store_atomic_write_replace"),
                    "{logs}"
                );
            }
            assert_eq!(
                logs.contains("ENOSPC at runtime_store::atomic_write"),
                errno == 28,
                "{logs}"
            );
            if errno == 28 {
                assert!(
                    crate::services::disk_monitor::enospc_recent(),
                    "ENOSPC must reach the disk monitor"
                );
                if stage == Stage::Rename {
                    assert!(
                        logs.find("agentdesk::inflight_write").unwrap()
                            < logs.find("ENOSPC at runtime_store").unwrap(),
                        "{logs}"
                    );
                }
            }
            drop(fault);
        }
    }
    let (result, logs) = capture_logs(|| atomic_write(&path, r#"{"user_msg_id":8}"#));
    result.unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"user_msg_id":8}"#);
    assert!(logs.contains("agentdesk::inflight_write"), "{logs}");
    assert!(
        logs.contains("user_msg_id=7"),
        "diagnostic must read the old file: {logs}"
    );
    assert!(!logs.contains("ENOSPC"), "{logs}");
}

#[test]
fn atomic_write_real_rename_failure_preserves_diagnostic_and_old_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("discord_inflight/codex/42.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, r#"{"user_msg_id":7}"#).unwrap();
    let fault = rename_fault::Armed::new(&path);
    let (result, logs) = capture_logs(|| atomic_write(&path, "new"));
    assert!(result.unwrap_err().starts_with("rename: "));
    assert_eq!(fault.evidence().0, 1);
    assert_eq!(fault.evidence().1.len(), 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"user_msg_id":7}"#);
    assert!(logs.contains("agentdesk::inflight_write"), "{logs}");
    assert!(logs.contains("user_msg_id=7"), "{logs}");
}
