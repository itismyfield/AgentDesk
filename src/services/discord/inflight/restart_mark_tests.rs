//! Restart marking from async shutdown code marks every row it can, a protected open row
//! included, and reports failed and unseen rows instead of stopping at the first.

use super::super::*;
use super::{RestartMarkReport, mark_restart_mode_blocking};
use crate::services::discord::input_runtime::{self, fence::Gate};

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
async fn c2b_async_marking_marks_protected_open_rows_and_reports_failed_and_unseen_rows() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let provider = ProviderKind::Codex;
    let root = inflight_runtime_root().unwrap();
    let (plain, open, held) = (6_325_940, 6_325_941, 6_325_942);
    let (failed_a, failed_b, unreadable) = (6_325_943, 6_325_944, 6_325_945);
    let path = |channel| inflight_state_path(&root, &provider, channel);
    for channel in [plain, open, held, failed_a, failed_b] {
        save_inflight_state_in_root(&root, &row(channel)).unwrap();
    }
    make_old_format(&path(open));
    make_old_format(&path(held));
    // A directory where the row lock file belongs makes that row's marker write fail.
    for channel in [failed_a, failed_b] {
        std::fs::create_dir_all(path(channel).with_extension("json.lock")).unwrap();
    }
    std::fs::write(path(unreadable), [0xff, 0xfe, 0xfd]).unwrap();
    let held_before = std::fs::read(path(held)).unwrap();
    let open_gate = Gate::protect(provider.clone(), open).unwrap();
    let _open_health = input_runtime::fence::test_health::Clear::new(&open_gate);
    let held_gate = Gate::protect(provider.clone(), held).unwrap();
    let _held_health = input_runtime::fence::test_health::Clear::new(&held_gate);
    let _closing = held_gate.close().unwrap();

    let mut report =
        mark_restart_mode_blocking(provider.clone(), InflightRestartMode::DrainRestart).await;
    report.failed.sort_unstable();

    assert_eq!(
        report,
        RestartMarkReport {
            marked: 2,
            failed: vec![failed_a, failed_b],
            incomplete: true,
        }
    );
    for channel in [plain, open] {
        let marked = read_inflight_state_content(&path(channel)).expect("marked row");
        assert_eq!(
            marked.restart_mode,
            Some(InflightRestartMode::DrainRestart),
            "channel {channel} left unmarked"
        );
    }
    assert_eq!(
        std::fs::read(path(held)).unwrap(),
        held_before,
        "held row untouched"
    );
    let reasons = input_runtime::health_reasons();
    for channel in [open, held] {
        assert!(
            !reasons
                .iter()
                .any(|reason| reason.contains(&format!("channel={channel}"))),
            "channel {channel} saw a refused writer: {reasons:?}"
        );
    }
}
