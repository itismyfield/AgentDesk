//! Restart marking backfills old rows and marks every sibling.

use super::*;

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
fn restart_marking_backfills_old_rows_and_marks_the_rest() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::TempDir::new().unwrap();
    let _restore = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let root = inflight_runtime_root().expect("env root must resolve");
    let (old, legacy) = (row(6_325_550), row(6_325_551));
    save_inflight_state_in_root(&root, &old).unwrap();
    save_inflight_state_in_root(&root, &legacy).unwrap();
    let old_path = inflight_state_path(&root, &ProviderKind::Codex, old.channel_id);
    let mut old_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&old_path).unwrap()).unwrap();
    old_json
        .as_object_mut()
        .unwrap()
        .remove("finalizer_turn_id");
    std::fs::write(&old_path, serde_json::to_vec_pretty(&old_json).unwrap()).unwrap();
    // The deferred restart retains the runtime on any marking error.
    let marked = mark_all_inflight_states_restart_mode_checked(
        &ProviderKind::Codex,
        InflightRestartMode::DrainRestart,
    );
    assert_eq!(marked, Ok(2));
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&old_path).unwrap()).unwrap();
    assert_eq!(raw["finalizer_turn_id"].as_u64(), Some(old.user_msg_id));
    assert_eq!(
        read_inflight_state_content(&old_path).unwrap().restart_mode,
        Some(InflightRestartMode::DrainRestart)
    );
    let legacy_path = inflight_state_path(&root, &ProviderKind::Codex, legacy.channel_id);
    let marked_row = read_inflight_state_content(&legacy_path).expect("legacy row");
    assert_eq!(
        marked_row.restart_mode,
        Some(InflightRestartMode::DrainRestart)
    );
}
