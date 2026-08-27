use super::*;

fn real_state(channel_id: u64, output: &Path, offset: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        None,
        1,
        2,
        3,
        "turn".to_string(),
        None,
        None,
        Some(output.display().to_string()),
        None,
        offset,
    )
}
#[test]
fn typed_real_create_api_records_successful_turn_births() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let output = temp.path().join("turn.jsonl");
    std::fs::write(&output, vec![b'a'; 512]).expect("write output");
    let channel_id = 54_900_008;
    create_monotonic_observer::test_seams::clear_key(ProviderKind::Codex, channel_id);
    let higher = real_state(channel_id, &output, 256);
    create_real_for_test(&higher).expect("higher real create");
    let runtime_root = inflight_runtime_root().expect("runtime root");
    let sidecar = inflight_state_path(&runtime_root, &ProviderKind::Codex, channel_id);
    std::fs::remove_file(&sidecar).expect("end higher turn");
    let mut lower = higher.clone();
    lower.user_msg_id += 1;
    lower.turn_start_offset = Some(128);
    lower.last_offset = 128;
    let (_, events) = invariant_test_capture::capture(|| {
        create_real_for_test(&lower).expect("lower real create");
    });
    assert_eq!(
        events,
        vec![invariant_test_capture::CapturedInvariant {
            invariant: "turn_start_offset_monotonic",
            severity: ObsSeverity::Warn,
        }],
    );
}
#[test]
fn create_new_inputs_separate_observed_real_from_checked_synthetic_births() {
    let real = real_state(54_900_009, Path::new("/tmp/real.jsonl"), 0);
    assert!(SyntheticInflightCreate::new(&real).is_err());

    let mut synthetic = real.clone();
    synthetic.request_owner_user_id = 0;
    synthetic.user_msg_id = 0;
    synthetic.current_msg_id = 0;
    synthetic.rebind_origin = true;
    synthetic.turn_source = TurnSource::MonitorTriggered;
    assert!(SyntheticInflightCreate::new(&synthetic).is_ok());

    let store = include_str!("../../save_store.rs");
    assert_eq!(
        store
            .matches(concat!("save_inflight_state_create_new_", "in_root("))
            .count(),
        4,
        "only the real, checked-synthetic, test-only wrappers and definition may reach raw O_EXCL",
    );
}
#[cfg(unix)]
#[test]
fn real_create_observes_while_sidecar_flock_is_held() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let output = temp.path().join("turn.jsonl");
    std::fs::write(&output, vec![b'a'; 512]).expect("write output");
    let state = real_state(54_900_007, &output, 256);
    let sidecar_path = inflight_state_path(temp.path(), &ProviderKind::Codex, state.channel_id);
    let hook_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_fired_in_hook = std::sync::Arc::clone(&hook_fired);
    create_monotonic_observer::test_seams::set_anchor_io_hook(move || {
        hook_fired_in_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            create_monotonic_observer::test_seams::sidecar_flock_is_held(&sidecar_path),
            "the observer must run before the create sidecar flock is released"
        );
    });
    let result = save_inflight_state_create_new_in_root(temp.path(), &state, true);
    create_monotonic_observer::test_seams::clear_anchor_io_hook();
    result.expect("real create");
    assert!(
        hook_fired.load(std::sync::atomic::Ordering::SeqCst),
        "the flock assertion hook must execute"
    );
}
#[test]
fn failed_sync_does_not_stamp_real_create_witness() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let output = temp.path().join("turn.jsonl");
    std::fs::write(&output, vec![b'a'; 512]).expect("write output");
    let channel_id = 54_900_006;
    create_monotonic_observer::test_seams::clear_key(ProviderKind::Codex, channel_id);
    let state = real_state(channel_id, &output, 256);
    create_monotonic_observer::test_seams::fail_next_sync(std::io::ErrorKind::Other);
    let result = save_inflight_state_create_new_in_root(temp.path(), &state, true);
    assert!(matches!(result, Err(CreateNewInflightError::Internal(_))));
    assert_eq!(
        create_monotonic_observer::test_seams::witness_offset(ProviderKind::Codex, channel_id),
        None,
        "failed durability sync must not publish a process witness"
    );
}
