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
        save_inflight_state_create_new_in_root(&runtime_root, &lower)
            .expect("raw inner real create");
    });
    assert_eq!(
        events,
        vec![invariant_test_capture::CapturedInvariant {
            invariant: "turn_start_offset_monotonic",
            severity: ObsSeverity::Warn,
        }],
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
    let result = save_inflight_state_create_new_in_root(temp.path(), &state);
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
    let result = save_inflight_state_create_new_in_root(temp.path(), &state);
    assert!(matches!(result, Err(CreateNewInflightError::Internal(_))));
    assert_eq!(
        create_monotonic_observer::test_seams::witness_offset(ProviderKind::Codex, channel_id),
        None,
        "failed durability sync must not publish a process witness"
    );
}

#[test]
fn real_if_absent_advances_witness_but_rebind_synthetic_does_not() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let output = temp.path().join("turn.jsonl");
    std::fs::write(&output, vec![b'a'; 512]).expect("write output");
    let witness =
        |id| create_monotonic_observer::test_seams::witness_offset(ProviderKind::Codex, id);
    let real = real_state(54_900_010, &output, 256);
    create_monotonic_observer::test_seams::clear_key(ProviderKind::Codex, real.channel_id);
    assert!(SyntheticInflightCreate::new(&real).is_err());
    assert!(save_inflight_state_if_absent_in_root(temp.path(), &real).unwrap());
    assert_eq!(witness(real.channel_id), Some(256));
    let mut synthetic = real_state(54_900_011, &output, 128);
    synthetic.request_owner_user_id = 0;
    synthetic.user_msg_id = 0;
    synthetic.current_msg_id = 0;
    synthetic.rebind_origin = true;
    synthetic.turn_source = TurnSource::MonitorTriggered;
    create_monotonic_observer::test_seams::clear_key(ProviderKind::Codex, synthetic.channel_id);
    assert!(SyntheticInflightCreate::new(&synthetic).is_ok());
    assert!(save_inflight_state_if_absent_in_root(temp.path(), &synthetic).unwrap());
    assert_eq!(witness(synthetic.channel_id), None);
}
