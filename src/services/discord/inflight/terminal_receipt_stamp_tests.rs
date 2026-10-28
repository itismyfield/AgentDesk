//! A row whose receipt commit failed keeps its first mark and only its own execution can mark it;
//! a boot eviction keeps an origin row marked failed instead of discarding it.
use tempfile::TempDir;

use super::super::{
    InflightTurnIdentity, InflightTurnState, invalidate_stale_generation_in_root,
    load_inflight_states_from_root, save_inflight_state, save_inflight_state_in_root,
};
use super::*;
use crate::db::turns::TerminalOutcome;
use crate::services::provider::ProviderKind;

fn row(channel_id: u64, origin: Option<&str>) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        None,
        7,
        channel_id + 1,
        8,
        "prompt".into(),
        None,
        Some(format!("AgentDesk-codex-stamp-{channel_id}")),
        None,
        None,
        0,
    );
    row.turn_nonce = Some(format!("nonce-{channel_id}"));
    row.input_origin_ids = origin.map(str::to_string).into_iter().collect();
    row
}

fn stamp(outcome: TerminalOutcome) -> TerminalReceiptStamp {
    TerminalReceiptStamp {
        outcome,
        confirmed_at: chrono::Utc::now(),
    }
}

#[test]
fn only_the_same_execution_marks_its_row_and_the_first_mark_stays() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mine = row(7_340_801, Some("G-mark"));
    save_inflight_state(&mine).unwrap();
    let identity = InflightTurnIdentity::from_state(&mine);
    let provider = ProviderKind::Codex;

    let mut successor = mine.clone();
    successor.turn_nonce = Some("another-turn".into());
    let refused = stamp_terminal_receipt_if_matches_identity(
        &provider,
        mine.channel_id,
        &identity,
        successor.turn_nonce.as_deref(),
        stamp(TerminalOutcome::Failed),
    );
    assert_eq!(refused, GuardedSaveOutcome::SuccessorOwned);

    let first = stamp(TerminalOutcome::Completed);
    for outcome in [first, stamp(TerminalOutcome::Failed)] {
        let marked = stamp_terminal_receipt_if_matches_identity(
            &provider,
            mine.channel_id,
            &identity,
            mine.turn_nonce.as_deref(),
            outcome,
        );
        assert_eq!(marked, GuardedSaveOutcome::Saved);
    }
    let on_disk = super::super::load_inflight_state(&provider, mine.channel_id).unwrap();
    assert_eq!(
        on_disk.terminal_receipt,
        Some(first),
        "the first mark is kept"
    );
    assert!(!on_disk.owes_origin_receipt());
}

#[test]
fn a_stale_generation_origin_row_is_kept_marked_failed_not_discarded() {
    let temp = TempDir::new().unwrap();
    let provider = ProviderKind::Codex;
    let mut owed = row(7_340_811, Some("G-stale"));
    owed.restart_generation = Some(3);
    let mut plain = row(7_340_812, None);
    plain.restart_generation = Some(3);
    save_inflight_state_in_root(temp.path(), &owed).unwrap();
    save_inflight_state_in_root(temp.path(), &plain).unwrap();

    let removed = invalidate_stale_generation_in_root(temp.path(), &provider, 5);
    assert_eq!(
        removed,
        vec![(7_340_812, Some(3))],
        "only the row without an origin goes"
    );

    let left = load_inflight_states_from_root(temp.path(), &provider);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].channel_id, 7_340_811);
    let mark = left[0].terminal_receipt.expect("the origin row is marked");
    assert_eq!(mark.outcome, TerminalOutcome::Failed);

    // A second boot neither evicts nor re-marks it.
    assert!(invalidate_stale_generation_in_root(temp.path(), &provider, 6).is_empty());
    let again = load_inflight_states_from_root(temp.path(), &provider);
    assert_eq!(again[0].terminal_receipt, Some(mark));
}
