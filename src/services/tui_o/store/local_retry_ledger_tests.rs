use super::*;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKind};
use crate::services::tui_o::writer::local_retry;

fn prepared(serial: u64, mark: Option<LocalRetryMark>) -> LedgerEntry {
    LedgerEntry::Prepared {
        serial,
        unit_key: UnitKey {
            channel_id: 7,
            provider: ShadowProvider::Claude,
            native_key: "root".into(),
            kind: UnitKind::Body,
        },
        piece_index: 0,
        payload: "원장 본문 ```x```".into(),
        anchor_id: 100,
        epoch: 1,
        local_retry: mark,
    }
}

#[test]
fn local_retry_delay_starts_at_the_terminal_line_time_instead_of_prepared_time() {
    let start = Utc::now();
    let finished = start + chrono::Duration::seconds(90);
    let mut state = LedgerState::default();
    let mut root = prepared(0, Some(LocalRetryMark::Budget));
    if let LedgerEntry::Prepared { anchor_id, .. } = &mut root {
        *anchor_id = 0;
    }
    state.apply(start, root);
    state.apply(finished, LedgerEntry::NotFound { serial: 0 });
    let record = state.piece(0).unwrap();
    assert_eq!(record.prepared_at, start);
    assert_eq!(record.outcome_at, Some(finished));
    assert_eq!(
        local_retry::pending(&state).unwrap().due,
        finished + chrono::Duration::seconds(30)
    );
}

#[test]
fn local_retry_reducer_rejects_duplicate_mismatched_or_ineligible_children() {
    let at = Utc::now();
    let mut root = LedgerState::default();
    // The default ledger's anchor is zero.
    let zero_anchor = |mut entry: LedgerEntry| {
        if let LedgerEntry::Prepared { anchor_id, .. } = &mut entry {
            *anchor_id = 0;
        }
        entry
    };
    root.apply(at, zero_anchor(prepared(0, Some(LocalRetryMark::Budget))));
    let outcomes = [
        LedgerEntry::NotFound { serial: 0 },
        LedgerEntry::Unresolved {
            serial: 0,
            reason: "err".into(),
        },
        LedgerEntry::Ambiguous {
            serial: 0,
            candidates: vec![1, 2],
        },
        LedgerEntry::NotSent { serial: 0 },
        LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        },
        LedgerEntry::Posted {
            serial: 0,
            msg_id: 1,
        },
    ];
    for outcome in outcomes {
        let eligible = matches!(outcome, LedgerEntry::NotFound { .. });
        let mut state = root.clone();
        state.apply(at, outcome);
        let mut child = zero_anchor(prepared(
            1,
            Some(LocalRetryMark::Attempt { root_serial: 0 }),
        ));
        if let LedgerEntry::Prepared { anchor_id, .. } = &mut child {
            *anchor_id = state.anchor();
        }
        state.apply(at, child);
        assert_eq!(state.violation().is_none(), eligible);
        if eligible {
            assert!(local_retry::pending(&state).is_none());
            state.apply(at, LedgerEntry::NotFound { serial: 1 });
            state.apply(
                at,
                zero_anchor(prepared(
                    2,
                    Some(LocalRetryMark::Attempt { root_serial: 0 }),
                )),
            );
            assert!(state.violation().is_some(), "a second child is never valid");
        }
    }
    for mismatch in 0..6 {
        let mut state = root.clone();
        state.apply(at, LedgerEntry::NotFound { serial: 0 });
        let mut child = zero_anchor(prepared(
            1,
            Some(LocalRetryMark::Attempt { root_serial: 0 }),
        ));
        if let LedgerEntry::Prepared {
            unit_key,
            piece_index,
            payload,
            local_retry,
            ..
        } = &mut child
        {
            match mismatch {
                0 => payload.push('!'),
                1 => unit_key.native_key.push('!'),
                2 => *piece_index = 1,
                3 => *local_retry = Some(LocalRetryMark::Attempt { root_serial: 9 }),
                4 => *local_retry = Some(LocalRetryMark::Attempt { root_serial: 1 }),
                _ => {
                    let mut unmarked = LedgerState::default();
                    unmarked.apply(at, zero_anchor(prepared(0, None)));
                    unmarked.apply(at, LedgerEntry::NotFound { serial: 0 });
                    state = unmarked;
                }
            }
        }
        state.apply(at, child);
        assert!(state.violation().is_some(), "mismatch {mismatch}");
    }
}
