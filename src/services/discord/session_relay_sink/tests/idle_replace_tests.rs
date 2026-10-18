//! A rebind's atomic relay replacement as the actual idle scanner sees it.

use super::*;

const CHILD: &str = "ADK_IDLE_REPLACE_CHILD";

/// After a rebind replaces the relay, the idle scanner follows it to byte 0, so
/// the rebuilt head an open row deferred is recovered once the row ends, and the
/// body delivered before the rebind is not sent again.
#[tokio::test(start_paused = true)]
async fn idle_cursor_restarts_a_rebind_replaced_relay_at_zero() {
    if std::env::var_os(CHILD).is_none() {
        let name = "services::discord::session_relay_sink::tests::idle_replace_tests::idle_cursor_restarts_a_rebind_replaced_relay_at_zero";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .expect("isolated scanner child must execute");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
        return;
    }
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        dir.path(),
    );
    let path = dir.path().join("relay.jsonl");
    let delivered = "{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"delivered before the rebind\"}]}}\n";
    let rebuilt = "{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"rebuilt\"}]}}\n";
    assert!(rebuilt.len() < delivered.len());
    let mut f = Dc1Fixture::start(&path, delivered.as_bytes(), true).await;
    let name = f.binding.expected_session_name.clone();
    let cursor = dc1_tick().await[&name];
    assert_eq!(
        cursor,
        (delivered.len() as u64, 0),
        "delivered bytes sit behind the cursor"
    );

    let mut row = inflight_for(&name, RelayOwnerKind::Watcher, false);
    row.channel_id = 58080001;
    inflight::save_inflight_state(&row).unwrap();
    let staged = format!("{}.rebuilt", path.display());
    std::fs::write(&staged, rebuilt).unwrap();
    note_relay_output_replaced(path.to_str().unwrap());
    std::fs::rename(&staged, &path).unwrap();
    assert_eq!(
        dc1_tick().await[&name],
        (0, 1),
        "the open row defers the rebuilt relay from byte 0"
    );

    inflight::clear_inflight_state(&ProviderKind::Claude, 58080001);
    // Production grace is the monotonic OS clock. Tokio advance alone cannot expire it.
    std::thread::sleep(Duration::from_millis(10_100));
    f.producer(true);
    dc1_tick().await;
    f.frame(0, rebuilt).await;
    assert!(
        f.rx.try_recv().is_err(),
        "the delivered body is never resent"
    );
    f.stop().await;
}
