use super::*;
use crate::services::discord::inflight::consume_stale_restart_marker_if_identity_unchanged;
use crate::services::discord::runtime_store::set_process_generation_for_tests;

struct GenerationReset;
impl Drop for GenerationReset {
    fn drop(&mut self) {
        set_process_generation_for_tests(None);
    }
}

fn row(channel: u64) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        42,
        channel + 1,
        channel + 2,
        "restart marker fixture".into(),
        None,
        None,
        None,
        None,
        512,
    );
    state.born_generation = 2;
    state.turn_nonce = Some("original-episode".into());
    state.restart_mode = Some(InflightRestartMode::DrainRestart);
    state.restart_generation = Some(4);
    state.updated_at = "2026-11-05 01:02:03".into();
    state.save_generation = 17;
    state.response_sent_offset = 211;
    state.last_watcher_relayed_offset = Some(377);
    state.full_response = "preserved output frontier ".repeat(20);
    state
}

fn seed(state: &InflightTurnState) -> std::path::PathBuf {
    let path = inflight_state_path(
        &inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        state.channel_id,
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec_pretty(state).unwrap()).unwrap();
    path
}

#[test]
fn refused_readoption_consumes_only_a_previous_unreadopted_marker() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _generation = GenerationReset;
    set_process_generation_for_tests(Some(5));
    type RowMutation = fn(&mut InflightTurnState);
    let cases: [(&str, RowMutation); 9] = [
        ("current generation", |state| {
            state.restart_generation = Some(5)
        }),
        ("future generation", |state| {
            state.restart_generation = Some(6)
        }),
        ("missing marker", |state| state.restart_mode = None),
        ("already readopted", |state| {
            state.readopted_from_inflight = true
        }),
        ("rebind origin", |state| state.rebind_origin = true),
        ("owner zero", |state| state.request_owner_user_id = 0),
        ("synthetic owner", |state| {
            state.request_owner_user_id =
                crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
        }),
        ("zero marker generation", |state| {
            state.restart_generation = Some(0)
        }),
        ("offsetless id zero", |state| {
            state.user_msg_id = 0;
            state.turn_start_offset = None;
        }),
    ];
    for (index, (name, mutate)) in cases.into_iter().enumerate() {
        let mut expected = row(7_807_701 + index as u64);
        mutate(&mut expected);
        let path = seed(&expected);
        let before = fs::read(&path).unwrap();
        consume_stale_restart_marker_if_identity_unchanged(&ProviderKind::Claude, &expected, 5);
        assert_eq!(fs::read(&path).unwrap(), before, "{name}");
    }

    let expected = row(7_807_710);
    let mut successor = expected.clone();
    successor.turn_nonce = Some("successor-episode".into());
    let path = seed(&successor);
    let before = fs::read(&path).unwrap();
    consume_stale_restart_marker_if_identity_unchanged(&ProviderKind::Claude, &expected, 5);
    assert_eq!(fs::read(&path).unwrap(), before, "successor nonce");

    for (channel, marker_generation) in [(7_807_711, Some(4)), (7_807_713, None)] {
        let mut expected = row(channel);
        expected.restart_generation = marker_generation;
        let path = seed(&expected);
        assert_eq!(
            consume_stale_restart_marker_if_identity_unchanged(&ProviderKind::Claude, &expected, 5),
            GuardedSaveOutcome::Saved,
            "an old-format marker uses its nonzero birth generation",
        );
        let actual: InflightTurnState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(actual.save_generation, expected.save_generation + 1);
        expected.clear_restart_mode();
        expected.save_generation = actual.save_generation;
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "only the consumed marker and CAS bookkeeping change; readoption, timestamp and frontiers stay",
        );
        assert!(!actual.readopted_from_inflight);
        assert_eq!(actual.updated_at, "2026-11-05 01:02:03");
        assert_eq!(actual.response_sent_offset, 211);
        assert_eq!(actual.last_offset, 512);
        assert_eq!(actual.last_watcher_relayed_offset, Some(377));
    }

    let absent = row(7_807_712);
    assert_eq!(
        consume_stale_restart_marker_if_identity_unchanged(&ProviderKind::Claude, &absent, 5),
        GuardedSaveOutcome::RowAbsent,
    );
    let missing = inflight_state_path(
        &inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        absent.channel_id,
    );
    assert!(
        !missing.exists(),
        "marker consumption must not recreate an absent row"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn two_shutdowns_refresh_real_and_readopted_synthetic_markers_before_boot() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _generation = GenerationReset;
    set_process_generation_for_tests(Some(4));
    let real = row(7_807_721);
    let mut synthetic = row(7_807_722);
    synthetic.request_owner_user_id =
        crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
    synthetic.set_relay_owner_kind(RelayOwnerKind::Watcher);
    let mut id_zero = row(7_807_723);
    id_zero.user_msg_id = 0;
    id_zero.current_msg_id = 0;
    id_zero.set_relay_owner_kind(RelayOwnerKind::Watcher);
    let mut states = [real, synthetic, id_zero];
    let channels = serde_json::to_string(&[
        (
            states[1].channel_id,
            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        ),
        (
            states[2].channel_id,
            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        ),
    ])
    .unwrap();
    let _channels = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
        std::ffi::OsStr::new(&channels),
    );
    let owned: Vec<_> = tokio::task::spawn_blocking(|| {
        crate::services::tui_o::cutover::boot_ownership()
            .into_iter()
            .filter_map(|(channel, _, owner)| {
                owner
                    .is_some_and(|owner| owner.peek().owned())
                    .then_some(channel)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert_eq!(owned, [states[1].channel_id, states[2].channel_id]);
    assert!(!owned.contains(&states[0].channel_id));
    let paths = states.each_ref().map(|state| {
        assert!(state.input_origin_ids.is_empty());
        seed(state)
    });
    for generation in [5, 6] {
        set_process_generation_for_tests(Some(generation));
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        for (state, path) in states.iter().zip(&paths).skip(1) {
            let before = fs::read(path).unwrap();
            assert!(
                crate::services::discord::recovery_engine::reregister_active_turn_from_inflight(
                    &shared, state,
                )
                .await,
                "synthetic and id0 turns may reattach successfully without a durable readopt marker",
            );
            assert_eq!(fs::read(path).unwrap(), before);
            let restored: InflightTurnState = serde_json::from_slice(&before).unwrap();
            assert!(!restored.readopted_from_inflight);
            assert!(restored.restart_mode.is_some());
        }
        let report =
            mark_restart_mode_blocking(ProviderKind::Claude, InflightRestartMode::DrainRestart)
                .await;
        assert!(!report.incomplete, "shutdown generation {generation}");
        assert!(report.failed.is_empty(), "shutdown generation {generation}");
        assert_eq!(report.marked, states.len());
        for (state, path) in states.iter_mut().zip(&paths) {
            let actual: InflightTurnState =
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert_eq!(actual.restart_generation, Some(generation));
            assert_ne!(actual.updated_at, state.updated_at);
            assert_eq!(actual.save_generation, state.save_generation + 1);
            assert!(!actual.readopted_from_inflight);
            assert_eq!(actual.response_sent_offset, state.response_sent_offset);
            assert_eq!(actual.last_offset, state.last_offset);
            *state = actual;
        }
        if generation == 5 {
            // The persisted clock has second precision; make the next refresh observable.
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
    }
    set_process_generation_for_tests(Some(7));
    let report = reap_inflight_rows_at_boot_with_guard(
        &BootReapOnce::default(),
        &ProviderKind::Claude,
        None,
    )
    .await;
    assert_eq!(report.kept, states.len());
    assert_eq!(report.reaped_stale, 0);
    assert!(paths.iter().all(|path| path.exists()));
}
