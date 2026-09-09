//! Executes the production adoption seam; caller reachability is lexical only.
use super::inflight::{self, GuardedSaveOutcome, InflightEpisodePin, InflightTurnState};
use super::*;

fn raw_episode() -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        5704,
        None,
        7,
        11,
        12,
        "adoption".into(),
        Some("raw-session".into()),
        Some("raw-tmux".into()),
        Some("/raw/rollout.jsonl".into()),
        None,
        0,
    );
    state.turn_source = inflight::TurnSource::ExternalInput;
    state.turn_nonce = Some("A".into());
    state.turn_start_offset = Some(164124174);
    state.last_offset = 165019890;
    state.last_watcher_relayed_offset = Some(164124180);
    state.last_watcher_relayed_generation_mtime_ns = Some(42);
    state
}

fn exercise(successor: bool) {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Both guarded persistence branches, with rebuild and resume coordinates.
    for pinned in [true, false] {
        for rebase in [Some(0), None] {
            let mut original = raw_episode();
            inflight::save_inflight_state(&original).unwrap();
            original = inflight::load_inflight_state(&ProviderKind::Codex, 5704).unwrap();
            let pin = InflightEpisodePin::from_state(&original);
            let mut durable = original.clone();
            if successor {
                // Pinned: nonce alone distinguishes B. Legacy also pins start offset.
                durable.turn_nonce = Some("B".into());
                if !pinned {
                    durable.turn_start_offset = original.turn_start_offset.map(|start| start + 1);
                }
                inflight::save_inflight_state(&durable).unwrap();
                durable = inflight::load_inflight_state(&ProviderKind::Codex, 5704).unwrap();
            }
            let before = serde_json::to_value(&durable).unwrap();
            let mut adopted = original.clone();
            let mut held = None;
            let (outcome, rollback_identity, rollback_start, rollback_frontier) =
                runtime.block_on(coordinate_adoption::adopt_coordinates(
                    &mut adopted,
                    "normalized-tmux",
                    "/normalized/events.jsonl",
                    &Some("/normalized/input".into()),
                    rebase,
                    Some(RuntimeHandoffKind::CodexTui),
                    &Some("normalized-session".into()),
                    pinned.then_some(&pin),
                    &mut held,
                ));
            if successor {
                assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
                assert!(held.is_none());
            } else {
                assert_eq!(outcome, GuardedSaveOutcome::Saved);
                assert_eq!(held.is_some(), pinned);
                if let Some(guard) = held.as_ref() {
                    assert_eq!(
                        serde_json::to_value(guard.state()).unwrap(),
                        serde_json::to_value(&adopted).unwrap()
                    );
                }
                assert_eq!(adopted.turn_nonce, original.turn_nonce);
                assert_eq!(adopted.user_msg_id, original.user_msg_id);
                assert_eq!(adopted.started_at, original.started_at);
                assert_eq!(
                    adopted.output_path.as_deref(),
                    Some("/normalized/events.jsonl")
                );
                assert_eq!(adopted.session_id.as_deref(), Some("normalized-session"));
                assert_eq!(
                    adopted.tmux_session_name.as_deref(),
                    Some("normalized-tmux")
                );
                assert_eq!(
                    adopted.input_fifo_path.as_deref(),
                    Some("/normalized/input")
                );
                assert_eq!(adopted.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
                assert_eq!(
                    adopted.effective_relay_owner_kind(),
                    inflight::RelayOwnerKind::Watcher
                );
                assert_eq!(
                    adopted.turn_start_offset,
                    rebase.or(original.turn_start_offset)
                );
                assert_eq!(adopted.last_offset, rebase.unwrap_or(original.last_offset));
                assert_eq!(
                    adopted.last_watcher_relayed_offset,
                    if rebase.is_some() {
                        None
                    } else {
                        original.last_watcher_relayed_offset
                    }
                );
                assert_eq!(
                    adopted.last_watcher_relayed_generation_mtime_ns,
                    if rebase.is_some() {
                        None
                    } else {
                        original.last_watcher_relayed_generation_mtime_ns
                    }
                );
                assert_eq!(
                    rollback_identity,
                    inflight::InflightTurnIdentity::from_state(&adopted)
                );
                assert_eq!(rollback_start, adopted.turn_start_offset);
                assert_eq!(rollback_frontier, rebase.map(|_| adopted.last_offset));
            }
            drop(held); // Release the actual adoption lock before loading durable state.
            let after = inflight::load_inflight_state(&ProviderKind::Codex, 5704).unwrap();
            if !successor && !pinned {
                // Legacy save returns an outcome, not its persisted timestamp/generation.
                assert_eq!(after.save_generation, original.save_generation + 1);
                assert!(!after.updated_at.is_empty());
                adopted.save_generation = after.save_generation;
                adopted.updated_at = after.updated_at.clone();
            }
            assert_eq!(
                serde_json::to_value(&after).unwrap(),
                if successor {
                    before
                } else {
                    serde_json::to_value(&adopted).unwrap()
                }
            );
        }
    }
}

#[test]
fn actual_seam_preserves_normal_adoption_and_rollback_coordinates() {
    exercise(false);
}

#[test]
fn actual_seam_rejects_same_anchor_successor_without_durable_mutation() {
    exercise(true);
}

#[test]
fn actual_caller_wires_adoption_before_guarded_handoff() {
    let caller = include_str!("mod.rs");
    let adoption = caller
        .find("coordinate_adoption::adopt_coordinates(")
        .expect("actual caller adoption");
    let rejected = caller[adoption..]
        .find("if !matches!(save_outcome")
        .expect("fail-closed adoption result");
    let handoff = caller[adoption..]
        .find("episode_handoff::commit_episode_side_effects")
        .unwrap();
    assert!(rejected < handoff);
}
