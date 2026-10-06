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
                    coordinate_adoption::AdoptionCoordinates {
                        tmux_session_name: "normalized-tmux",
                        output_path: "/normalized/events.jsonl",
                        input_fifo_for_state: &Some("/normalized/input".into()),
                        existing_offset_rebase_to_output: rebase,
                        runtime_kind_for_state: Some(RuntimeHandoffKind::CodexTui),
                        session_id_for_state: &Some("normalized-session".into()),
                    },
                    pinned.then_some(&pin),
                    &mut held,
                ));
            if successor {
                assert!(outcome.is_identity_mismatch_legacy());
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
fn actual_seam_orders_episode_authority_before_hold_and_return() {
    let seam = include_str!("coordinate_adoption.rs");
    assert_eq!(seam.matches("adopt_and_lock_inflight_episode(").count(), 1);
    let acquire = seam
        .find("adopt_and_lock_inflight_episode(")
        .expect("authority");
    let hold = seam
        .find("*locked_episode_from_adoption = Some(guard)")
        .expect("live authority handed to the caller");
    let ret = seam
        .rfind("    (\n        save_outcome,")
        .expect("seam tuple return");
    assert!(acquire < hold && hold < ret);
}

#[test]
fn host_lookup_finishes_before_exact_adoption_can_hold_a_guard() {
    let caller = include_str!("mod.rs");
    let host = caller
        .find("let host = super::tmux::watch_host_of(")
        .expect("host lookup");
    let host_await = host + caller[host..].find(".await;").expect("host awaited");
    let adoption = caller
        .find("coordinate_adoption::adopt_coordinates(")
        .expect("exact adoption");
    let handoff = caller
        .find("episode_handoff::commit_episode_side_effects(")
        .expect("handoff");
    assert!(
        host_await < adoption && adoption < handoff,
        "host PG await must finish before exact adoption acquires the guard"
    );
    assert_eq!(caller.matches("super::tmux::watch_host_of(").count(), 1);
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

/// Stops one channel's blocking adoption before it takes the episode lock.
pub(super) mod adoption_gap {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender, channel};

    type Gap = (Sender<()>, Receiver<()>);
    static GAPS: Mutex<Option<HashMap<u64, Gap>>> = Mutex::new(None);

    /// Returns (reached, resume) for the channel's next blocking adoption.
    pub(super) fn arm(channel_id: u64) -> (Receiver<()>, Sender<()>) {
        let (reached_tx, reached_rx) = channel();
        let (resume_tx, resume_rx) = channel();
        let mut gaps = GAPS.lock().unwrap_or_else(|e| e.into_inner());
        gaps.get_or_insert_with(HashMap::new)
            .insert(channel_id, (reached_tx, resume_rx));
        (reached_rx, resume_tx)
    }

    pub(in super::super) fn pause(channel_id: u64) {
        let gap = {
            let mut gaps = GAPS.lock().unwrap_or_else(|e| e.into_inner());
            gaps.as_mut().and_then(|gaps| gaps.remove(&channel_id))
        };
        if let Some((reached, resume)) = gap {
            let _ = reached.send(());
            let _ = resume.recv();
        }
    }
}

#[test]
fn a_cancelled_respawn_keeps_the_slot_until_its_blocking_adoption_returns() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let provider = ProviderKind::Claude;
    let mut original = raw_episode();
    original.provider = provider.as_str().to_string();
    original.channel_id = 5_707_301;
    inflight::save_inflight_state(&original).unwrap();
    let original = inflight::load_inflight_state(&provider, 5_707_301).unwrap();
    let pin = InflightEpisodePin::from_state(&original);
    let (reached, resume) = adoption_gap::arm(5_707_301);
    let original_could_start =
        || super::super::live_bridge::try_respawn_recovery(&provider, 5_707_301).is_ok();

    let recovery = super::super::live_bridge::try_respawn_recovery(&provider, 5_707_301).unwrap();
    assert!(recovery.is_guarded());
    let mut adopted = original.clone();
    let task = runtime.spawn(async move {
        recovery
            .run(async move {
                let mut held = None;
                coordinate_adoption::adopt_coordinates(
                    &mut adopted,
                    coordinate_adoption::AdoptionCoordinates {
                        tmux_session_name: "normalized-tmux",
                        output_path: "/normalized/transcript.jsonl",
                        input_fifo_for_state: &None,
                        existing_offset_rebase_to_output: None,
                        runtime_kind_for_state: Some(RuntimeHandoffKind::ClaudeTui),
                        session_id_for_state: &None,
                    },
                    Some(&pin),
                    &mut held,
                )
                .await
                .0
            })
            .await
    });
    reached
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the adoption reaches its blocking section");
    task.abort();
    assert!(runtime.block_on(task).unwrap_err().is_cancelled());
    assert!(
        !original_could_start(),
        "the slot must stay closed while the blocking adoption runs"
    );

    resume.send(()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !original_could_start() {
        assert!(std::time::Instant::now() < deadline, "the slot must reopen");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let durable = inflight::load_inflight_state(&provider, 5_707_301).unwrap();
    assert_eq!(
        durable.effective_relay_owner_kind(),
        inflight::RelayOwnerKind::Watcher,
        "the adoption committed inside the closed slot"
    );
}
