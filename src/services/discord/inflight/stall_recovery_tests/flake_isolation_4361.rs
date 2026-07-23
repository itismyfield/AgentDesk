//! #4361 test-isolation regression suite.
//!
//! Migrated out of the `inflight.rs` decomposition parent so its inline test
//! residue stays under the frozen `parent_test_residue` ceiling (#4267/#4269 —
//! raise the cap is forbidden; move the tests instead). These belong to the
//! `stall_recovery_tests` family: the CI `stall_recovery` filter still matches
//! their `…::stall_recovery_tests::flake_isolation_4361::…` path, and they reach
//! the shared fixtures (`build_synth_3358`, `build_inflight_for_guard_tests`,
//! `monotonic_3358_test_mutex`, `set_agentdesk_root_for_test`, the `_in_root`
//! helpers, …) through the parent module via `use super::*`.

use super::*;

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
    let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].last_offset, committed_frontier,
        "offsets end at the committed frontier, never regressed"
    );
}

/// #4361 regression (DETERMINISTIC — no clock dependency): pins down the exact
/// mechanism of the parallel-only flake in
/// `synthetic_carry_forward_keeps_reclaim_monotonic_3358`.
///
/// The carried-birth re-claim is gated by `InflightTurnIdentity`, which
/// includes `started_at` — a 1-second-resolution `now_string()` stamp. The
/// historical flake minted the persisted row and the re-claim with TWO
/// independent `InflightTurnState::new` calls; when they straddled a wall-clock
/// second (parallel/loaded CI) their `started_at` diverged and the
/// identity-gated refresh rejected the "same turn" write, panicking the green
/// assertion (reproduced locally by inserting a >1s straddle between the two
/// births). This test reproduces the same rejection WITHOUT any timing race by
/// skewing `started_at` directly, and proves the same-identity re-claim is
/// accepted — so the "one shared birth" fix is provably load-bearing and any
/// future desync is caught here instead of on CI.
#[test]
fn carried_birth_reclaim_rejects_started_at_skew_4361() {
    let _serialized = monotonic_3358_test_mutex()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = TempDir::new().unwrap();
    // Pin the observability runtime root to the tempdir (#3293 guard) so a
    // rejected re-claim's invariant record never resolves the live release root.
    let _env_reset = set_agentdesk_root_for_test(temp.path());
    let frontier: u64 = 2_838_484;

    // Persist one committed-frontier row (the same shape the green test writes).
    let mut on_disk = build_synth_3358(frontier);
    on_disk.full_response = "X".repeat(20_000);
    on_disk.response_sent_offset = 18_000;
    on_disk.last_offset = frontier;
    save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

    // A re-claim that differs from the persisted row ONLY by `started_at` — the
    // exact residue of two `now_string()` stamps straddling a second. It is a
    // DIFFERENT identity, so the identity-gated refresh MUST reject it (return
    // false). This is the #4361 parallel flake, reproduced deterministically.
    let mut skewed = on_disk.clone();
    skewed.started_at = "1999-01-01 00:00:00".to_string();
    assert_ne!(
        skewed.started_at, on_disk.started_at,
        "the skew fixture must actually differ"
    );
    let skew_res = refresh_inflight_last_offset_if_matches_identity_in_root(
        temp.path(),
        &ProviderKind::Claude,
        321,
        &InflightTurnIdentity::from_state(&skewed),
        skewed.turn_start_offset,
        "/tmp/out.jsonl",
        Some(skewed.current_msg_id),
        frontier,
        RelayOwnerKind::Watcher,
    );
    assert!(
        !skew_res,
        "a started_at-skewed re-claim is a DIFFERENT identity and must be rejected (#4361 flake mechanism)"
    );

    // The SAME identity (started_at preserved, as the one-shared-birth fix
    // guarantees) IS accepted — a forward/equal watermark write, zero invariant
    // violation, offsets end at the committed frontier.
    let same_res = refresh_inflight_last_offset_if_matches_identity_in_root(
        temp.path(),
        &ProviderKind::Claude,
        321,
        &InflightTurnIdentity::from_state(&on_disk),
        on_disk.turn_start_offset,
        "/tmp/out.jsonl",
        Some(on_disk.current_msg_id),
        frontier,
        RelayOwnerKind::Watcher,
    );
    assert!(
        same_res,
        "the same-identity re-claim (shared started_at) must be accepted"
    );
    let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].last_offset, frontier,
        "the accepted re-claim keeps last_offset at the committed frontier"
    );
}

/// Skip → the on-disk row became a planned-restart marker. The guarded save
/// must not clobber it (`IdentityMismatch`); restart recovery owns it.
#[test]
fn skip_save_does_not_clobber_planned_restart_marker() {
    // #4361: process generation and the runtime root are ambient process state.
    // Pin the root under the shared env lock so marker stamping and loading read
    // one deterministic epoch, exactly like the sibling rebind-adoption test.
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = TempDir::new().unwrap();
    let _env_reset = set_agentdesk_root_for_test(temp.path());
    let mut marker = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
    marker.set_restart_mode(InflightRestartMode::DrainRestart);
    save_inflight_state_in_root(temp.path(), &marker).unwrap();

    let preserved = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
    let expected = InflightTurnIdentity::from_state(&preserved);

    let outcome = save_inflight_state_if_matches_identity_in_root(
        temp.path(),
        &preserved,
        &expected,
        preserved.turn_start_offset,
    );

    assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
    let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].restart_mode.is_some(),
        "the planned-restart marker must be preserved for recovery"
    );
}

/// #4361 regression plus #4775 replacement-boot contract: an ambient generation
/// change must be deterministic, but the immediate E -> E+1 successor is now the
/// intended DrainRestart consumer and must preserve the marker for recovery.
#[test]
fn planned_restart_marker_survives_immediate_replacement_generation_4361() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = TempDir::new().unwrap();
    let _env_reset = set_agentdesk_root_for_test(temp.path());
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(7));

    let mut marker = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
    marker.set_restart_mode(InflightRestartMode::DrainRestart);
    assert_eq!(marker.restart_generation, Some(7));
    save_inflight_state_in_root(temp.path(), &marker).unwrap();

    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(8));
    let replacement_rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
    assert_eq!(replacement_rows.len(), 1);
    assert_eq!(
        replacement_rows[0].restart_mode,
        Some(InflightRestartMode::DrainRestart),
        "the immediate replacement must receive the planned-restart row"
    );

    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(9));
    let skipped_successor_rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
    assert!(
        skipped_successor_rows.is_empty(),
        "an unconsumed marker outside the immediate replacement window is stale"
    );
    crate::services::discord::runtime_store::set_process_generation_for_tests(None);
}

#[test]
fn immediate_replacement_adoption_consumes_restart_marker_4775() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = TempDir::new().unwrap();
    let _env_reset = set_agentdesk_root_for_test(temp.path());
    let provider = ProviderKind::Claude;
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(7));

    let mut marker = build_inflight_for_guard_tests(provider.clone(), 322, 778);
    marker.readopted_from_inflight = true;
    marker.set_restart_mode(InflightRestartMode::DrainRestart);
    let identity = InflightTurnIdentity::from_state(&marker);
    save_inflight_state_in_root(temp.path(), &marker).unwrap();

    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(8));
    let replacement_rows = load_inflight_states_from_root(temp.path(), &provider);
    assert_eq!(replacement_rows.len(), 1);
    assert_eq!(
        super::super::save_store::identity_gate::mark_readopted_from_inflight_if_identity_unchanged_in_root(
            temp.path(),
            &provider,
            marker.channel_id,
            &identity,
        ),
        GuardedSaveOutcome::Saved
    );
    let adopted = load_inflight_states_from_root(temp.path(), &provider);
    assert_eq!(adopted.len(), 1);
    assert!(adopted[0].readopted_from_inflight);
    assert_eq!(adopted[0].restart_mode, None);
    assert_eq!(adopted[0].restart_generation, None);
    assert_eq!(
        clear_inflight_state_if_matches_in_root(
            temp.path(),
            &provider,
            marker.channel_id,
            marker.user_msg_id,
        ),
        GuardedClearOutcome::Cleared,
        "idempotent readoption must consume the marker so ordinary completion clear succeeds"
    );
    crate::services::discord::runtime_store::set_process_generation_for_tests(None);
}
