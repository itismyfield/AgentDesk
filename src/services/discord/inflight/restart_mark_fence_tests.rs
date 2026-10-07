//! Restart marking skips a held input channel's row without failing the pass for the others.

use super::*;
use crate::services::discord::input_runtime::fence::{self, Gate};

fn protect_after_scan(channel: u64) {
    AFTER_EXCLUDING_SCAN.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            let gate = Gate::protect(ProviderKind::Codex, channel).unwrap();
            let _closing = gate.close().unwrap();
        }));
    });
}

#[tokio::test]
async fn c2_loop_rechecks_protection_installed_after_loading() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let root = inflight_runtime_root().unwrap();
    for (index, channel) in (6_325_601..6_325_604).enumerate() {
        let mut state = row(channel);
        if index == 1 {
            state.restart_generation = Some(1);
        }
        if index == 2 {
            state.rebind_origin = true;
        }
        save_inflight_state_in_root(&root, &state).unwrap();
        let path = inflight_state_path(&root, &ProviderKind::Codex, channel);
        let before = std::fs::read(&path).unwrap();
        protect_after_scan(channel);
        match index {
            0 => assert_eq!(
                mark_all_inflight_states_restart_mode_checked(
                    &ProviderKind::Codex,
                    InflightRestartMode::DrainRestart,
                ),
                Ok(0),
                "marking must recheck after the snapshot"
            ),
            1 => assert_eq!(invalidate_stale_generation(&ProviderKind::Codex, 2), 0),
            _ => {
                let shared = crate::services::discord::make_shared_data_for_tests();
                crate::services::discord::recovery_engine::restore_inflight_turns(
                    &std::sync::Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
                    &shared,
                    &ProviderKind::Codex,
                )
                .await;
            }
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "late-protected row changed"
        );
        let gate = fence::lookup(&ProviderKind::Codex, channel).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        assert!(
            !crate::services::discord::input_runtime::health_reasons()
                .iter()
                .any(|reason| reason.contains(&format!("channel={channel}"))),
            "loop recheck must avoid even a refused writer attempt"
        );
    }
}

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
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&held_path).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("finalizer_turn_id");
    std::fs::write(&held_path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();
    let before = std::fs::read(&held_path).unwrap();
    let gate = Gate::protect(ProviderKind::Codex, held.channel_id).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();

    // The deferred restart retains the runtime on any marking error.
    let marked = mark_all_inflight_states_restart_mode_checked(
        &ProviderKind::Codex,
        InflightRestartMode::DrainRestart,
    );
    assert!(
        !crate::services::discord::input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={}", held.channel_id))),
        "old-format Held row must not attempt a compatibility writer"
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
