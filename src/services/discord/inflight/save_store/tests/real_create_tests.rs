use super::*;
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
    let higher = InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        None,
        1,
        2,
        3,
        "higher".to_string(),
        None,
        None,
        Some(output.display().to_string()),
        None,
        256,
    );
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
fn both_real_turn_call_sites_use_the_typed_create_api() {
    fn collect_typed_api_files(dir: &Path, root: &Path, found: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read source directory") {
            let entry = entry.expect("source entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("source entry type");
            if file_type.is_dir() {
                collect_typed_api_files(&path, root, found);
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(&path).expect("Rust source");
                if source.contains("save_real_inflight_state_create_new(")
                    || source.contains("RealInflightCreate::new(")
                {
                    found.push(
                        path.strip_prefix(root)
                            .expect("source under root")
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut typed_api_files = Vec::new();
    collect_typed_api_files(&root.join("src"), root, &mut typed_api_files);
    typed_api_files.sort();
    assert_eq!(
        typed_api_files,
        vec![
            "src/services/discord/inflight/save_store.rs",
            "src/services/discord/router/message_handler/headless_turn.rs",
            "src/services/discord/router/message_handler/intake_turn.rs",
        ],
        "the real-create observer capability must have exactly two production callers",
    );

    let headless = include_str!("../../../router/message_handler/headless_turn.rs");
    let intake = include_str!("../../../router/message_handler/intake_turn.rs");
    for (name, source) in [("headless", headless), ("intake", intake)] {
        assert_eq!(
            source
                .matches("save_real_inflight_state_create_new(")
                .count(),
            1,
            "{name} real-turn birth must use the observer-typed create API exactly once",
        );
        assert_eq!(
            source
                .matches("RealInflightCreate::new(&inflight_state)")
                .count(),
            1,
            "{name} must construct the typed real-turn input at its birth site",
        );
    }
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
    let state = InflightTurnState::new(
        ProviderKind::Codex,
        54_900_007,
        Some("adk-test".to_string()),
        1,
        2,
        3,
        "turn".to_string(),
        None,
        Some("AgentDesk-codex-flock-5490".to_string()),
        Some(output.display().to_string()),
        None,
        256,
    );
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
    let state = InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        Some("adk-test".to_string()),
        1,
        2,
        3,
        "turn".to_string(),
        None,
        Some("AgentDesk-codex-sync-5490".to_string()),
        Some(output.display().to_string()),
        None,
        256,
    );
    create_monotonic_observer::test_seams::fail_next_sync(std::io::ErrorKind::Other);
    let result = save_inflight_state_create_new_in_root(temp.path(), &state, true);
    assert!(matches!(result, Err(CreateNewInflightError::Internal(_))));
    assert_eq!(
        create_monotonic_observer::test_seams::witness_offset(ProviderKind::Codex, channel_id),
        None,
        "failed durability sync must not publish a process witness"
    );
}
