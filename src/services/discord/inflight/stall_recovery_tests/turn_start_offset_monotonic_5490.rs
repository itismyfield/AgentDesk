//! #5490 regression: the cross-turn `turn_start_offset` monotonic guard.
//!
//! `same_turn_identity` is keyed on `(user_msg_id, turn_start_offset)`, so a
//! BACKWARD move of `turn_start_offset` declares itself a fresh turn and thereby
//! silences every same-identity monotonic guard — including the one that would
//! have caught it. These tests pin the cross-identity observer that closes that
//! hole, and prove the hole is real by asserting the pre-existing guards stay
//! silent on the exact same write.

use super::*;
use crate::services::observability::InvariantSeverity;

/// The 2026-08-22 shape: a turn pinned at 269088 followed by a turn claiming 0.
const PRIOR_TURN_START: u64 = 269_088;

fn captured(
    root: &std::path::Path,
    channel_id: u64,
    next: &InflightTurnState,
) -> Vec<super::super::invariant_test_capture::CapturedInvariant> {
    let path = inflight_state_path(root, &ProviderKind::Claude, channel_id);
    let (_result, events) = super::super::invariant_test_capture::capture(|| {
        validate_inflight_state_for_save(
            root,
            &path,
            next,
            "src/services/discord/inflight/store.rs:test",
        );
    });
    events
}

fn severities_for(
    events: &[super::super::invariant_test_capture::CapturedInvariant],
    invariant: &str,
) -> Vec<InvariantSeverity> {
    events
        .iter()
        .filter(|event| event.invariant == invariant)
        .map(|event| event.severity)
        .collect()
}

/// Seeds a prior turn at `PRIOR_TURN_START` and returns the successor turn that
/// rewinds `turn_start_offset` to 0 under a fresh `user_msg_id`.
fn seed_prior_and_build_rewound_successor(
    root: &std::path::Path,
    channel_id: u64,
) -> InflightTurnState {
    let mut prior = seed_watcher_stream_state(
        root,
        channel_id,
        "AgentDesk-claude-5490",
        "the previous turn's committed answer",
        PRIOR_TURN_START + 1_024,
    );
    prior.turn_start_offset = Some(PRIOR_TURN_START);
    force_write_state(root, &prior);

    let mut next = prior.clone();
    // A genuinely fresh turn: new anchor, cleared body, frontier back to 0.
    next.user_msg_id = prior.user_msg_id + 1;
    next.full_response = String::new();
    next.response_sent_offset = 0;
    next.last_offset = 0;
    // ...but its start offset REWINDS past the previous turn's start.
    next.turn_start_offset = Some(0);
    next
}

#[test]
fn backward_turn_start_offset_across_turns_records_typed_warn_5490() {
    let temp = TempDir::new().unwrap();
    let channel_id = 54_900_001;
    let next = seed_prior_and_build_rewound_successor(temp.path(), channel_id);

    let events = captured(temp.path(), channel_id, &next);

    assert_eq!(
        severities_for(&events, "turn_start_offset_monotonic"),
        [InvariantSeverity::Warn],
        "a backward cross-turn turn_start_offset must record exactly one typed WARN: {events:?}"
    );
}

/// The whole point of #5490: the two pre-existing offset guards are keyed on
/// `same_turn_identity`, and `turn_start_offset` is PART of that key — so this
/// exact write is invisible to them. If this test ever starts failing because
/// one of them fired, the identity key changed and #5490's guard may be
/// redundant; that is a deliberate tripwire, not a nuisance.
#[test]
fn existing_same_identity_guards_stay_silent_on_the_cross_turn_rewind_5490() {
    let temp = TempDir::new().unwrap();
    let channel_id = 54_900_002;
    let next = seed_prior_and_build_rewound_successor(temp.path(), channel_id);

    let events = captured(temp.path(), channel_id, &next);

    assert!(
        severities_for(&events, "response_sent_offset_monotonic").is_empty(),
        "response_sent_offset_monotonic is identity-gated and must not fire here — \
         its silence is exactly the gap #5490 closes: {events:?}"
    );
    assert!(
        severities_for(&events, "last_offset_monotonic").is_empty(),
        "last_offset_monotonic is identity-gated and must not fire here — \
         its silence is exactly the gap #5490 closes: {events:?}"
    );
}

#[test]
fn forward_turn_start_offset_across_turns_is_not_a_violation_5490() {
    let temp = TempDir::new().unwrap();
    let channel_id = 54_900_003;
    let mut next = seed_prior_and_build_rewound_successor(temp.path(), channel_id);
    // The healthy successor: the next turn begins AFTER the previous one.
    next.turn_start_offset = Some(PRIOR_TURN_START + 2_048);

    let events = captured(temp.path(), channel_id, &next);

    assert!(
        severities_for(&events, "turn_start_offset_monotonic").is_empty(),
        "a forward cross-turn start offset must not be reported: {events:?}"
    );
}

/// `None` on either side is UNMEASURED, not a violation — mirrors the #5071
/// relay-tail S2 rule that an absent reading reads as unknown, never as a
/// satisfied bound. Without this the guard would false-fire on every first turn.
#[test]
fn unmeasured_turn_start_offset_is_not_a_violation_5490() {
    let temp = TempDir::new().unwrap();
    let channel_id = 54_900_004;
    let mut next = seed_prior_and_build_rewound_successor(temp.path(), channel_id);
    next.turn_start_offset = None;

    let events = captured(temp.path(), channel_id, &next);

    assert!(
        severities_for(&events, "turn_start_offset_monotonic").is_empty(),
        "an unmeasured start offset must not be reported as backward: {events:?}"
    );
}
