//! Restart marking reports failed and unseen rows while marking every readable sibling.

use super::super::*;
use super::{
    RestartMarkReport, WriteFaultForTest, mark_restart_mode_blocking, short_pass_event_for_test,
    short_passes,
};

fn row(channel_id: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        Some("c2b-mark".to_string()),
        7,
        channel_id + 1,
        channel_id + 2,
        "hello".to_string(),
        None,
        None,
        None,
        None,
        0,
    )
}

/// Rewrites a row the way a pre-finalizer-id build left it, so the scan must backfill it.
fn make_old_format(path: &std::path::Path) {
    let mut old: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("finalizer_turn_id");
    std::fs::write(path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();
}

#[tokio::test]
async fn async_marking_marks_readable_rows_and_reports_failed_and_unseen_rows() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let provider = ProviderKind::Codex;
    let root = inflight_runtime_root().unwrap();
    let (plain, old_a, old_b) = (6_325_940, 6_325_941, 6_325_942);
    let (failed_a, failed_b, unreadable) = (6_325_943, 6_325_944, 6_325_945);
    let path = |channel| inflight_state_path(&root, &provider, channel);
    for channel in [plain, old_a, old_b, failed_a, failed_b] {
        save_inflight_state_in_root(&root, &row(channel)).unwrap();
    }
    make_old_format(&path(old_a));
    make_old_format(&path(old_b));
    // A directory where the row lock file belongs makes that row's marker write fail.
    for channel in [failed_a, failed_b] {
        let lock = path(channel).with_extension("json.lock");
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
    }
    std::fs::write(path(unreadable), [0xff, 0xfe, 0xfd]).unwrap();
    let mut report =
        mark_restart_mode_blocking(provider.clone(), InflightRestartMode::DrainRestart).await;
    report.failed.sort_unstable();

    assert_eq!(
        report,
        RestartMarkReport {
            marked: 3,
            failed: vec![failed_a, failed_b],
            skipped_receipt: Vec::new(),
            incomplete: true,
        }
    );
    for channel in [plain, old_a, old_b] {
        let marked = read_inflight_state_content(&path(channel)).expect("marked row");
        assert_eq!(
            marked.restart_mode,
            Some(InflightRestartMode::DrainRestart),
            "channel {channel} left unmarked"
        );
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path(channel)).unwrap()).unwrap();
        assert_eq!(raw["finalizer_turn_id"].as_u64(), Some(channel + 1));
    }
    let event = short_pass_event_for_test().expect("short pass reported as an error event");
    let mut event_failed: Vec<u64> = serde_json::from_value(event["failed"].clone()).unwrap();
    event_failed.sort_unstable();
    assert_eq!(event_failed, vec![failed_a, failed_b]);
    assert_eq!(event["incomplete"], true);
}

/// A failed marker leaves siblings marked; the next complete pass clears the short pass.
#[tokio::test]
async fn failed_marker_write_reports_short_pass_until_a_complete_pass() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let provider = ProviderKind::Codex;
    let root = inflight_runtime_root().unwrap();
    let (plain, open) = (6_325_972, 6_325_973);
    let path = |channel| inflight_state_path(&root, &provider, channel);
    for channel in [plain, open] {
        save_inflight_state_in_root(&root, &row(channel)).unwrap();
    }
    let codex_pass = || {
        short_passes()
            .into_iter()
            .map(|pass| serde_json::to_value(pass).unwrap())
            .find(|pass| pass["provider"] == "codex")
    };

    let fault = WriteFaultForTest::new(path(open));
    let report =
        mark_restart_mode_blocking(provider.clone(), InflightRestartMode::DrainRestart).await;

    assert_eq!(
        report,
        RestartMarkReport {
            marked: 1,
            failed: vec![open],
            skipped_receipt: Vec::new(),
            incomplete: false,
        }
    );
    assert_eq!(
        read_inflight_state_content(&path(plain)).and_then(|row| row.restart_mode),
        Some(InflightRestartMode::DrainRestart),
        "the sibling row left unmarked"
    );
    assert_eq!(
        codex_pass(),
        Some(serde_json::json!({
            "provider": "codex",
            "marked": 1,
            "failed": [open],
            "incomplete": false,
        }))
    );

    drop(fault);
    let report =
        mark_restart_mode_blocking(provider.clone(), InflightRestartMode::DrainRestart).await;

    assert_eq!(
        report,
        RestartMarkReport {
            marked: 2,
            failed: Vec::new(),
            skipped_receipt: Vec::new(),
            incomplete: false,
        }
    );
    assert_eq!(codex_pass(), None, "a complete pass keeps the short pass");
}

/// Restart marking leaves receipt rows byte-identical and reports them apart from marked and failed
/// rows, so the pass stays complete; a malformed receipt row still leaves the scan incomplete.
#[tokio::test]
async fn receipt_rows_are_skipped_byte_identical_and_never_counted_as_failed() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let provider = ProviderKind::Codex;
    let root = inflight_runtime_root().unwrap();
    let (plain, supported, unknown, null) = (6_534_901, 6_534_902, 6_534_903, 6_534_904);
    let malformed = 6_534_905;
    let path = |channel| inflight_state_path(&root, &provider, channel);
    let receipt = serde_json::json!({"protocol": 1, "kind": "unsubmitted_before_start_stop",
        "turn_nonce": "c2b-mark", "admitted_generation": 2, "basis": "no_attempt",
        "notice": "pending"});
    let mut before = std::collections::BTreeMap::new();
    for (channel, shape) in [
        (supported, Some(receipt.clone())),
        (unknown, Some(serde_json::json!({"protocol": 9}))),
        (null, Some(serde_json::Value::Null)),
        (plain, None),
    ] {
        save_inflight_state_in_root(&root, &row(channel)).unwrap();
        if let Some(shape) = shape {
            make_old_format(&path(channel));
            let mut raw: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path(channel)).unwrap()).unwrap();
            raw["herdr_before_start_stop"] = shape;
            std::fs::write(path(channel), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
            before.insert(channel, std::fs::read(path(channel)).unwrap());
        }
    }
    let codex_pass = || {
        short_passes()
            .into_iter()
            .any(|pass| serde_json::to_value(pass).unwrap()["provider"] == "codex")
    };

    let mut report =
        mark_restart_mode_blocking(provider.clone(), InflightRestartMode::DrainRestart).await;
    report.skipped_receipt.sort_unstable();

    assert_eq!(
        report,
        RestartMarkReport {
            marked: 1,
            failed: Vec::new(),
            skipped_receipt: vec![supported, unknown, null],
            incomplete: false,
        }
    );
    assert!(!codex_pass(), "a skipped receipt row is no short pass");
    assert_eq!(
        read_inflight_state_content(&path(plain)).and_then(|row| row.restart_mode),
        Some(InflightRestartMode::DrainRestart),
        "the receipt-free sibling is marked"
    );
    for (channel, bytes) in &before {
        assert_eq!(&std::fs::read(path(*channel)).unwrap(), bytes, "{channel}");
    }

    let mut raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path(supported)).unwrap()).unwrap();
    raw["channel_id"] = malformed.into();
    let mut text = serde_json::to_string_pretty(&raw).unwrap();
    text.truncate(text.len() - 3);
    std::fs::write(path(malformed), &text).unwrap();

    let mut report =
        mark_restart_mode_blocking(provider.clone(), InflightRestartMode::DrainRestart).await;
    report.skipped_receipt.sort_unstable();

    assert_eq!(report.failed, Vec::<u64>::new());
    assert_eq!(report.skipped_receipt, vec![supported, unknown, null]);
    assert!(report.incomplete, "the malformed row stays unseen");
    assert_eq!(std::fs::read_to_string(path(malformed)).unwrap(), text);
    for (channel, bytes) in &before {
        assert_eq!(&std::fs::read(path(*channel)).unwrap(), bytes, "{channel}");
    }
}
