//! Executes the production adoption seam; caller reachability is lexical only.
use super::inflight::{self, GuardedSaveOutcome, InflightEpisodePin, InflightTurnState};
use super::*;
use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

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

const SEAM: &str = include_str!("coordinate_adoption.rs");
const CALLER: &str = include_str!("mod.rs");
const SEAM_FN: &str = "pub(super) async fn adopt_coordinates(";
const ACQUIRE: &str = "adopt_and_lock_inflight_episode(";
const HOLD: &str = "*locked_episode_from_adoption = Some(guard)";
const SEAM_RETURN: &str = "    (\n        save_outcome,";
const CALLER_FN: &str = "async fn rebind_inflight_for_channel_inner(";
const HOST: &str = "let host = super::tmux::watch_host_of(";
const HOST_LOOKUP: &str = "super::tmux::watch_host_of(";
const ADOPTION: &str = "coordinate_adoption::adopt_coordinates(";
const REJECTED: &str = "if !matches!(save_outcome";
const HANDOFF: &str = "episode_handoff::commit_episode_side_effects(";

// Classification 2 for the three guards below: steps are matched as written inside the
// real seam or caller item, comments excluded. Lexical tripwires, not reachability proofs.

/// Episode authority, then the guard handed to the caller, then the seam's tuple return.
fn seam_order_gaps(seam: &str) -> Vec<String> {
    let lexed = Lexed::new(seam);
    let Some(body) = lexed.block(SEAM_FN) else {
        return vec![format!("missing {SEAM_FN}")];
    };
    let acquire = body.find(ACQUIRE);
    let hold = body.find(HOLD);
    let ret = body.find_all(SEAM_RETURN).last().copied();
    match (lexed.count(ACQUIRE), acquire, hold, ret) {
        (1, Some(acquire), Some(hold), Some(ret)) if acquire < hold && hold < ret => Vec::new(),
        found => vec![format!("authority → hold → return broken: {found:?}")],
    }
}

/// The host lookup is awaited before exact adoption, which precedes the handoff.
fn host_order_gaps(caller: &str) -> Vec<String> {
    let lexed = Lexed::new(caller);
    let Some(body) = lexed.block(CALLER_FN) else {
        return vec![format!("missing {CALLER_FN}")];
    };
    let mut gaps = Vec::new();
    let host_await = body
        .find(HOST)
        .and_then(|at| body.find_after(at, ".await;"));
    match (host_await, body.find(ADOPTION), body.find(HANDOFF)) {
        (Some(host), Some(adoption), Some(handoff)) if host < adoption && adoption < handoff => {}
        found => gaps.push(format!("host await → adoption → handoff broken: {found:?}")),
    }
    if lexed.count(HOST_LOOKUP) != 1 {
        gaps.push(format!("{HOST_LOOKUP} must be called once"));
    }
    gaps
}

/// A refused adoption result is checked before any handoff side effect.
fn rejection_gaps(caller: &str) -> Vec<String> {
    let Some(body) = Lexed::new(caller).block(CALLER_FN) else {
        return vec![format!("missing {CALLER_FN}")];
    };
    let adoption = body.find(ADOPTION);
    let rejected = adoption.and_then(|at| body.find_after(at, REJECTED));
    let handoff = adoption.and_then(|at| body.find_after(at, HANDOFF));
    match (rejected, handoff) {
        (Some(rejected), Some(handoff)) if rejected < handoff => Vec::new(),
        found => vec![format!("adoption refusal → handoff broken: {found:?}")],
    }
}

#[test]
fn actual_seam_orders_episode_authority_before_hold_and_return() {
    assert_eq!(seam_order_gaps(SEAM), Vec::<String>::new());
}

#[test]
fn host_lookup_finishes_before_exact_adoption_can_hold_a_guard() {
    assert_eq!(host_order_gaps(CALLER), Vec::<String>::new());
}

#[test]
fn actual_caller_wires_adoption_before_guarded_handoff() {
    assert_eq!(rejection_gaps(CALLER), Vec::<String>::new());
}

#[test]
fn adoption_order_guards_reject_commented_and_reordered_steps() {
    type Judge = fn(&str) -> Vec<String>;
    let cases: [(Judge, &str, &[&str]); 3] = [
        (
            seam_order_gaps,
            SEAM,
            &[SEAM_FN, ACQUIRE, HOLD, SEAM_RETURN],
        ),
        (
            host_order_gaps,
            CALLER,
            &[CALLER_FN, HOST, ADOPTION, HANDOFF],
        ),
        (
            rejection_gaps,
            CALLER,
            &[CALLER_FN, ADOPTION, REJECTED, HANDOFF],
        ),
    ];
    for (judge, source, needles) in cases {
        for needle in needles {
            for forgery in CommentForgery::ALL {
                let forged = forge(source, needle, forgery);
                assert!(
                    !judge(&forged).is_empty(),
                    "{forgery:?} commented `{needle}` still passes"
                );
            }
        }
    }
    assert!(!seam_order_gaps(&swap(SEAM, ACQUIRE, HOLD)).is_empty());
    assert!(!host_order_gaps(&swap(CALLER, HOST, ADOPTION)).is_empty());
    assert!(!rejection_gaps(&swap(CALLER, REJECTED, HANDOFF)).is_empty());
    let lookup = format!("let _ = {HOST_LOOKUP}shared);");
    let prose = CALLER.replacen(HANDOFF, &format!("// {lookup}\n{HANDOFF}"), 1);
    assert_ne!(prose, CALLER);
    assert_eq!(host_order_gaps(&prose), Vec::<String>::new());
    let coded = CALLER.replacen(HANDOFF, &format!("{lookup}\n{HANDOFF}"), 1);
    assert_eq!(host_order_gaps(&coded).len(), 1);
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
