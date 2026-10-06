//! Restart marking skips a held input channel's row without failing the pass for the others.

use super::*;
use crate::services::discord::input_runtime::fence::{self, Gate};

fn row(channel_id: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        Some("c2-mark".to_string()),
        7,
        channel_id + 1,
        channel_id + 2,
        "hello".to_string(),
        None,
        Some(format!("AgentDesk-codex-c2-mark-{channel_id}")),
        Some("/tmp/out.jsonl".to_string()),
        None,
        0,
    )
}

#[test]
fn c2_restart_marking_skips_a_held_row_and_marks_the_rest() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::TempDir::new().unwrap();
    let _restore = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let root = inflight_runtime_root().expect("env root must resolve");
    let (held, legacy) = (row(6_325_550), row(6_325_551));
    save_inflight_state_in_root(&root, &held).unwrap();
    save_inflight_state_in_root(&root, &legacy).unwrap();
    let held_path = inflight_state_path(&root, &ProviderKind::Codex, held.channel_id);
    let before = std::fs::read(&held_path).unwrap();
    let gate = Gate::protect(ProviderKind::Codex, held.channel_id).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();

    // The deferred restart retains the runtime on any marking error.
    let marked = mark_all_inflight_states_restart_mode_checked(
        &ProviderKind::Codex,
        InflightRestartMode::DrainRestart,
    );
    assert_eq!(marked, Ok(1));
    assert_eq!(
        std::fs::read(&held_path).unwrap(),
        before,
        "held row untouched"
    );
    let legacy_path = inflight_state_path(&root, &ProviderKind::Codex, legacy.channel_id);
    let marked_row = read_inflight_state_content(&legacy_path).expect("legacy row");
    assert_eq!(
        marked_row.restart_mode,
        Some(InflightRestartMode::DrainRestart)
    );
}
