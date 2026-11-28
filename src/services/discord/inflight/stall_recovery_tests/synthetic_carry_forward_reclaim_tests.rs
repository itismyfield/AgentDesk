use tempfile::TempDir;

use crate::config::set_agentdesk_root_for_test;
use crate::services::discord::inflight::{
    InflightTurnIdentity, RelayOwnerKind, load_inflight_states_for_test,
    refresh_inflight_last_offset_if_matches_identity_in_root, save_inflight_state_in_root,
    synthetic_carry_forward_fixture_tests::{build_synth_3358, monotonic_3358_test_mutex},
};
use crate::services::provider::ProviderKind;

#[test]
fn synthetic_carry_forward_keeps_reclaim_monotonic_3358() {
    let _serialized = monotonic_3358_test_mutex()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // FIX: birth carried up to the committed frontier → re-claim is forward/
    // equal, ZERO invariant violations, offsets end at the frontier.
    let temp = TempDir::new().unwrap();
    // The refresh path records inflight-invariant observability via the
    // PROCESS-WIDE runtime root (#3293 guard) — pin it to the tempdir so a
    // standalone/parallel run never resolves the live release root.
    let _env_reset = set_agentdesk_root_for_test(temp.path());
    let relay_last_offset: u64 = 2_821_677;
    let committed_frontier: u64 = 2_838_484;
    // Drive the ACTUAL production carry-forward helper (not an inline copy) so
    // this green test honestly tracks the production wiring — if the helper
    // regressed, `carried` would no longer reach the frontier and the
    // monotonicity assertions below would fail. The frontier is `Some(..)`
    // because the watcher advanced it WITHIN this same wrapper generation (the
    // claim choke-point validates that before clamping — #3358 round 2).
    let carried = crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
        relay_last_offset,
        Some(committed_frontier),
    );
    assert_eq!(
        carried, committed_frontier,
        "carry-forward must lift birth to the frontier"
    );

    // #4361: build the synthetic turn identity ONCE and derive BOTH the
    // persisted row and the re-claim from that single birth. `InflightTurnState::
    // new` stamps `started_at = now_string()` at 1-SECOND resolution, and
    // `started_at` is part of `InflightTurnIdentity` (the refresh's same-turn
    // gate). Two independent `build_synth_3358` calls that straddle a wall-clock
    // second — common under parallel/loaded CI scheduling — mint DIFFERENT
    // `started_at` values, so the identity-gated refresh rejects the re-claim as
    // a *different* identity and the "forward/equal — accepted" assertion below
    // panics. That is the #4361 parallel-only flake (green in isolation because
    // both `new()` calls land in the same second). One shared birth keeps the
    // re-claim honestly same-turn and clock-independent.
    let synth = build_synth_3358(carried);
    let mut on_disk = synth.clone();
    on_disk.full_response = "X".repeat(20_000);
    on_disk.response_sent_offset = 18_000;
    on_disk.last_offset = committed_frontier;
    save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

    // Carried-birth re-claim: turn_start_offset == last_offset == frontier,
    // rso == 0. The rso 0 is NOT a regression because the identity key matches
    // (same turn) and `response_sent_offset_monotonic` only flags within-turn
    // backward moves AFTER bytes were sent — here the re-claim's last_offset
    // equals the on-disk frontier (forward/equal) and rso reset is the
    // documented fresh-claim seed. Assert last_offset never regresses below
    // the committed frontier.
    let carried_reseed = synth;
    // #4361 clock-skew guard: the persisted row and the re-claim MUST be the
    // SAME turn identity. If a future edit regresses to two independent
    // `now_string()` stamps, this catches the desync deterministically instead
    // of leaking it back to CI as a parallel flake.
    assert_eq!(
        on_disk.started_at, carried_reseed.started_at,
        "same-turn re-claim must share one started_at (#4361 clock-skew guard)"
    );
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Drive the enforcing watermark path: a carried-birth refresh writes
        // last_offset == committed_frontier — forward/equal, accepted.
        refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &InflightTurnIdentity::from_state(&carried_reseed),
            carried_reseed.turn_start_offset,
            "/tmp/out.jsonl",
            Some(carried_reseed.current_msg_id),
            committed_frontier,
            RelayOwnerKind::Watcher,
        )
    }));
    assert!(res.is_ok(), "carried-birth refresh must not panic");
    assert!(
        res.unwrap(),
        "carried-birth watermark write is forward/equal — accepted"
    );
    let loaded = load_inflight_states_for_test(temp.path(), &ProviderKind::Claude);
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].last_offset, committed_frontier,
        "offsets end at the committed frontier, never regressed"
    );
}
