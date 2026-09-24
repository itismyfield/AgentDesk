//! #5951 S1 — the `GuardedSaveOutcome` truth table and its behaviour proof.
//!
//! Design §3.3 names six causes that the pre-split `IdentityMismatch` collapsed
//! into one value. The split only earns its keep if every one of them is
//! REACHABLE through a real guarded-write entry point and lands on the variant
//! its repair / finalize semantics demand. Nothing below injects an outcome
//! value into a helper: each case seeds a real durable row in a temporary
//! runtime root and drives the shipped writer against it.

use super::*;
use crate::services::provider::ProviderKind;

const CALLER: &str = "test::5951_s1_outcome_decomposition";
const PROVIDER: ProviderKind = ProviderKind::Codex;

fn turn(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        PROVIDER,
        channel_id,
        Some("adk-5951-s1".to_string()),
        343_742_347_365_974_026,
        user_msg_id,
        18,
        "user prompt".to_string(),
        Some("session".to_string()),
        Some(format!("AgentDesk-codex-5951-{channel_id}")),
        Some(format!("/tmp/AgentDesk-codex-5951-{channel_id}.jsonl")),
        None,
        512,
    );
    state.turn_start_offset = Some(4_096);
    state.turn_nonce = Some(format!("nonce-{user_msg_id}"));
    state
}

fn row_bytes(root: &Path, channel_id: u64) -> Vec<u8> {
    fs::read(inflight_state_path(root, &PROVIDER, channel_id)).expect("durable row")
}

/// Seed `durable`, then drive the shipped identity-guarded save with `local`
/// and assert the durable row was left byte-identical by the refusal.
fn refused_save(
    channel_id: u64,
    durable: impl FnOnce(&mut InflightTurnState),
    local: impl FnOnce(&mut InflightTurnState),
) -> GuardedSaveOutcome {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let mut seed = turn(channel_id, 77_010);
    durable(&mut seed);
    save_inflight_state_in_root(temp.path(), &seed).expect("seed durable row");
    let before = row_bytes(temp.path(), channel_id);

    let mut snapshot = turn(channel_id, 77_010);
    local(&mut snapshot);
    let outcome =
        save_inflight_state_if_identity_unchanged_in_root(temp.path(), &mut snapshot, CALLER);
    assert_eq!(
        row_bytes(temp.path(), channel_id),
        before,
        "a refused guarded save must leave the durable row byte-identical"
    );
    outcome
}

/// §3.3's six causes, each reached through the shipped writer, each pinned to
/// the variant the repair/finalize axis requires.
///
/// The three load-bearing cells:
/// - cause 2 (`restart_mode`) must NOT be `RowAbsent`; `RowAbsent` is the only
///   value #5951 §1.2 lets a later slice repair on, and repairing over a
///   planned-restart marker is MR12.
/// - cause 6 (offsetless id-0) must NOT be `SuccessorOwned`; it means *no*
///   turn, which is the opposite of "my turn is over, go finalize".
/// - causes 4 and 5 must NOT be `AuthorityPinned`; a successor really does end
///   this turn.
#[test]
fn guarded_save_outcome_truth_table_separates_all_six_identity_mismatch_causes() {
    // The value that authorizes repair: nothing on disk at all.
    let temp = tempfile::TempDir::new().expect("runtime root");
    let mut orphan = turn(5_951_000, 77_010);
    let row_absent =
        save_inflight_state_if_identity_unchanged_in_root(temp.path(), &mut orphan, CALLER);
    assert_eq!(row_absent, GuardedSaveOutcome::RowAbsent);
    assert!(!row_absent.is_identity_mismatch_legacy());

    // Cause 1 — the durable row's `output_path` moved (identity_gate.rs).
    let output_path_changed = refused_save(
        5_951_001,
        |durable| durable.output_path = Some("/tmp/somewhere-else.jsonl".to_string()),
        |_local| {},
    );

    // Cause 2 — a planned-restart marker owns the row. MR12's target cell.
    let restart_marker = refused_save(
        5_951_002,
        |durable| {
            durable.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart)
        },
        |_local| {},
    );

    // Cause 3 — a rebind-origin placeholder owns the row.
    let rebind_origin = refused_save(
        5_951_003,
        |durable| durable.rebind_origin = true,
        |_local| {},
    );

    // Cause 4 — a genuinely different episode holds the row.
    let successor_turn = refused_save(
        5_951_004,
        |durable| durable.user_msg_id = 77_011,
        |_local| {},
    );

    // Cause 5 — terminal delivery already committed against another nonce.
    let committed_elsewhere = refused_save(
        5_951_005,
        |durable| durable.turn_nonce = Some("some-other-episode".to_string()),
        |local| local.terminal_delivery_committed = true,
    );

    // Cause 6 — the offsetless id-0 heartbeat identity, which names no turn at
    // all (identity_gate/heartbeat.rs). Driven through the heartbeat writer.
    let heartbeat_temp = tempfile::TempDir::new().expect("runtime root");
    let mut heartbeat_row = turn(5_951_006, 0);
    heartbeat_row.turn_start_offset = None;
    save_inflight_state_in_root(heartbeat_temp.path(), &heartbeat_row).expect("seed durable row");
    let before = row_bytes(heartbeat_temp.path(), 5_951_006);
    let unnameable = touch_inflight_state_if_matches_identity_in_root(
        heartbeat_temp.path(),
        &PROVIDER,
        5_951_006,
        &InflightTurnIdentity::from_state(&heartbeat_row),
        CALLER,
    );
    assert_eq!(
        row_bytes(heartbeat_temp.path(), 5_951_006),
        before,
        "an unnameable heartbeat must leave the durable row byte-identical"
    );

    assert_eq!(output_path_changed, GuardedSaveOutcome::AuthorityPinned);
    assert_eq!(restart_marker, GuardedSaveOutcome::AuthorityPinned);
    assert_eq!(rebind_origin, GuardedSaveOutcome::AuthorityPinned);
    assert_eq!(successor_turn, GuardedSaveOutcome::SuccessorOwned);
    assert_eq!(committed_elsewhere, GuardedSaveOutcome::SuccessorOwned);
    assert_eq!(unnameable, GuardedSaveOutcome::Unnameable);

    // The separations the later slices are gated on, stated as such so a
    // re-label cannot pass by agreeing with itself.
    for (name, cause) in [
        ("output_path", output_path_changed),
        ("restart_mode", restart_marker),
        ("rebind_origin", rebind_origin),
        ("successor turn", successor_turn),
        ("committed elsewhere", committed_elsewhere),
        ("offsetless id-0", unnameable),
    ] {
        assert!(
            cause.is_identity_mismatch_legacy(),
            "{name} was an IdentityMismatch before #5951 S1 and must stay one"
        );
        assert_ne!(
            cause,
            GuardedSaveOutcome::RowAbsent,
            "{name} must never become the value a projection repair is gated on"
        );
    }
    assert_ne!(
        restart_marker, successor_turn,
        "a planned restart is not a successor turn: one forbids finalize, the other licenses it"
    );
    assert_ne!(
        unnameable, successor_turn,
        "an unnameable id-0 identity is NO turn, not a later one"
    );
    assert_ne!(
        unnameable, output_path_changed,
        "a caller that cannot name itself is not the same refusal as a pinned row"
    );
}

/// Every variant the pre-#5951 `IdentityMismatch` stood for answers the legacy
/// predicate, and `Saved` / `RowAbsent` / `IoError` stay outside it. The gate
/// verdicts themselves are pinned beside each gate in `turn_bridge`.
#[test]
fn every_split_variant_keeps_the_pre_split_identity_mismatch_verdict() {
    const LEGACY: [GuardedSaveOutcome; 3] = [
        GuardedSaveOutcome::AuthorityPinned,
        GuardedSaveOutcome::Unnameable,
        GuardedSaveOutcome::SuccessorOwned,
    ];
    for refusal in LEGACY {
        assert!(
            refusal.is_identity_mismatch_legacy(),
            "{refusal:?} must answer the legacy identity-mismatch predicate"
        );
    }
    for outside in [
        GuardedSaveOutcome::Saved,
        GuardedSaveOutcome::RowAbsent,
        GuardedSaveOutcome::IoError,
    ] {
        assert!(
            !outside.is_identity_mismatch_legacy(),
            "{outside:?} was never an IdentityMismatch and must not become one"
        );
    }
}
