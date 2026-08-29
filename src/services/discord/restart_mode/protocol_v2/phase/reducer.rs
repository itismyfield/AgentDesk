use super::*;
use super::{
    DecodeDisposition as Disposition, PhaseConflictV2 as Conflict, ReducedPhaseStateV2 as State,
};

pub(super) fn reduce(history: &[PhaseEventDispositionV2]) -> PhaseReductionDispositionV2 {
    let mut state: Option<State> = None;
    let mut last_raw: Box<[u8]> = Box::default();
    let mut previous_sequence = None;
    let mut previous_hash = None;
    let mut identity: Option<PhaseIdentityV2> = None;
    for disposition in history {
        let (event, raw) = match disposition {
            Disposition::Accepted { value, raw } => (value, raw),
            Disposition::Conflict { reason, raw } => {
                return Disposition::Conflict {
                    reason: *reason,
                    raw: raw.clone(),
                };
            }
            Disposition::Poison { reason, raw } => {
                return Disposition::Poison {
                    reason: *reason,
                    raw: raw.clone(),
                };
            }
            Disposition::Unsupported { raw } => {
                return Disposition::Unsupported { raw: raw.clone() };
            }
        };
        if let Some(reason) = invalid_sequence(previous_sequence, event.sequence) {
            return conflict(reason, raw);
        }
        if event.previous_hash != previous_hash {
            return conflict(Conflict::PreviousHashMismatch, raw);
        }
        if identity
            .as_ref()
            .is_some_and(|value| value != &event.identity)
        {
            return conflict(Conflict::IdentityMismatch, raw);
        }
        state = match transition(state, event) {
            Ok(value) => Some(value),
            Err(reason) => return conflict(reason, raw),
        };
        previous_sequence = Some(event.sequence);
        previous_hash = Some(event.event_hash);
        let _ = identity.get_or_insert_with(|| event.identity.clone());
        last_raw = raw.clone();
    }
    match state {
        Some(value) => Disposition::Accepted {
            value,
            raw: last_raw,
        },
        None => Disposition::Conflict {
            reason: Conflict::EmptyHistory,
            raw: Box::default(),
        },
    }
}

fn invalid_sequence(previous: Option<u64>, actual: u64) -> Option<Conflict> {
    let expected = match previous {
        None => 0,
        Some(value) => match value.checked_add(1) {
            Some(expected) => expected,
            None => {
                return Some(Conflict::ReorderedSequence {
                    previous: value,
                    actual,
                });
            }
        },
    };
    (actual != expected).then(|| {
        if actual > expected {
            Conflict::SequenceGap { expected, actual }
        } else if previous == Some(actual) {
            Conflict::DuplicateSequence { sequence: actual }
        } else {
            Conflict::ReorderedSequence {
                previous: previous.unwrap_or(expected),
                actual,
            }
        }
    })
}

fn conflict(reason: Conflict, raw: &[u8]) -> PhaseReductionDispositionV2 {
    Disposition::Conflict {
        reason,
        raw: raw.into(),
    }
}

fn transition(state: Option<State>, event: &PhaseEventV2) -> Result<State, Conflict> {
    let head = PhaseHeadV2 {
        sequence: event.sequence,
        event_hash: event.event_hash,
    };
    match state {
        None => match &event.kind {
            PhaseKindV2::Bound => Ok(State::BoundPending {
                identity: event.identity.clone(),
                head,
            }),
            _ => Err(Conflict::IllegalFirstPhase),
        },
        Some(State::BoundPending { identity, .. }) => match &event.kind {
            PhaseKindV2::Bound => Err(Conflict::RepeatedBound),
            PhaseKindV2::Started => Ok(State::RunningPending { identity, head }),
            PhaseKindV2::Terminal { .. } => Err(Conflict::TerminalBeforeStart),
            PhaseKindV2::Receipt { .. } => Err(Conflict::ReceiptBeforeTerminal),
        },
        Some(State::RunningPending { identity, .. }) => match &event.kind {
            PhaseKindV2::Bound => Err(Conflict::RepeatedBound),
            PhaseKindV2::Started => Err(Conflict::RepeatedStarted),
            PhaseKindV2::Terminal {
                outcome,
                terminal_proof,
            } => Ok(State::TerminalWithoutReceipt {
                identity,
                head,
                outcome: *outcome,
                terminal_proof: terminal_proof.clone(),
            }),
            PhaseKindV2::Receipt { .. } => Err(Conflict::ReceiptBeforeTerminal),
        },
        Some(State::TerminalWithoutReceipt {
            identity,
            outcome,
            terminal_proof,
            ..
        }) => match &event.kind {
            PhaseKindV2::Bound => Err(Conflict::RepeatedBound),
            PhaseKindV2::Started => Err(Conflict::RepeatedStarted),
            PhaseKindV2::Terminal { .. } => Err(Conflict::RepeatedTerminal),
            PhaseKindV2::Receipt { durable_receipt } => Ok(State::TerminalWithReceipt {
                identity,
                head,
                outcome,
                terminal_proof,
                durable_receipt: durable_receipt.clone(),
            }),
        },
        Some(State::TerminalWithReceipt { .. }) => Err(Conflict::EventAfterReceipt),
    }
}

#[cfg(test)]
mod high_risk_recovery {
    use super::super::codec::{decode, encode, event_digest, fixture};
    use super::*;
    fn kind(index: usize) -> PhaseKindV2 {
        match index {
            0 => PhaseKindV2::Bound,
            1 => PhaseKindV2::Started,
            2 => PhaseKindV2::Terminal {
                outcome: TerminalOutcomeV2::Completed,
                terminal_proof: SafeRelativeRefV2::parse("proofs/terminal.json").unwrap(),
            },
            _ => PhaseKindV2::Receipt {
                durable_receipt: SafeRelativeRefV2::parse("receipts/durable.json").unwrap(),
            },
        }
    }
    fn chain() -> Vec<PhaseEventV2> {
        let a = fixture(0, None, kind(0));
        let b = fixture(1, Some(a.event_hash), kind(1));
        let c = fixture(2, Some(b.event_hash), kind(2));
        let d = fixture(3, Some(c.event_hash), kind(3));
        vec![a, b, c, d]
    }
    fn head(event: &PhaseEventV2) -> PhaseHeadV2 {
        PhaseHeadV2 {
            sequence: event.sequence,
            event_hash: event.event_hash,
        }
    }
    fn accepted(value: PhaseEventV2, raw: &[u8]) -> PhaseEventDispositionV2 {
        Disposition::Accepted {
            value,
            raw: raw.into(),
        }
    }
    fn rejects(prefix: &[PhaseEventV2], event: PhaseEventV2, reason: Conflict, raw: &[u8]) {
        let mut history = prefix
            .iter()
            .cloned()
            .map(|value| accepted(value, b"prior"))
            .collect::<Vec<_>>();
        history.push(accepted(event, raw));
        assert_eq!(
            reduce(&history),
            Disposition::Conflict {
                reason,
                raw: raw.into()
            }
        );
    }
    fn propagated(value: &PhaseEventDispositionV2) -> PhaseReductionDispositionV2 {
        match value {
            Disposition::Conflict { reason, raw } => Disposition::Conflict {
                reason: *reason,
                raw: raw.clone(),
            },
            Disposition::Poison { reason, raw } => Disposition::Poison {
                reason: *reason,
                raw: raw.clone(),
            },
            Disposition::Unsupported { raw } => Disposition::Unsupported { raw: raw.clone() },
            Disposition::Accepted { .. } => panic!("accepted is not propagated"),
        }
    }
    #[test]
    fn positive_prefixes_reach_all_four_states_and_keep_last_original_raw() {
        let events = chain();
        let identity = events[0].identity.clone();
        let expected = [
            State::BoundPending {
                identity: identity.clone(),
                head: head(&events[0]),
            },
            State::RunningPending {
                identity: identity.clone(),
                head: head(&events[1]),
            },
            State::TerminalWithoutReceipt {
                identity: identity.clone(),
                head: head(&events[2]),
                outcome: TerminalOutcomeV2::Completed,
                terminal_proof: SafeRelativeRefV2::parse("proofs/terminal.json").unwrap(),
            },
            State::TerminalWithReceipt {
                identity,
                head: head(&events[3]),
                outcome: TerminalOutcomeV2::Completed,
                terminal_proof: SafeRelativeRefV2::parse("proofs/terminal.json").unwrap(),
                durable_receipt: SafeRelativeRefV2::parse("receipts/durable.json").unwrap(),
            },
        ];
        let mut history = Vec::new();
        for (event, state) in events.iter().zip(expected) {
            let mut raw = b" \n".to_vec();
            raw.extend(encode(event).unwrap());
            raw.push(b'\t');
            history.push(decode(&raw));
            assert_eq!(
                reduce(&history),
                Disposition::Accepted {
                    value: state,
                    raw: raw.into()
                }
            );
        }
    }
    #[test]
    fn sequence_hash_and_identity_conflicts_keep_the_first_offending_raw() {
        assert_eq!(
            reduce(&[]),
            Disposition::Conflict {
                reason: Conflict::EmptyHistory,
                raw: Box::default()
            }
        );
        let events = chain();
        let mut rows = Vec::new();
        let mut event = events[1].clone();
        event.sequence = 2;
        event.event_hash = event_digest(&event).unwrap();
        rows.push((
            1,
            event,
            Conflict::SequenceGap {
                expected: 1,
                actual: 2,
            },
            b"gap".as_slice(),
        ));
        let mut event = events[1].clone();
        event.sequence = 0;
        event.event_hash = event_digest(&event).unwrap();
        rows.push((
            1,
            event,
            Conflict::DuplicateSequence { sequence: 0 },
            b"duplicate".as_slice(),
        ));
        rows.push((
            3,
            events[0].clone(),
            Conflict::ReorderedSequence {
                previous: 2,
                actual: 0,
            },
            b"reordered".as_slice(),
        ));
        let mut event = events[0].clone();
        event.previous_hash = Some(EventDigestV2([7; 32]));
        event.event_hash = event_digest(&event).unwrap();
        rows.push((
            0,
            event,
            Conflict::PreviousHashMismatch,
            b"first-hash".as_slice(),
        ));
        for (hash, raw) in [
            (Some(EventDigestV2([8; 32])), b"wrong-hash".as_slice()),
            (None, b"missing-hash".as_slice()),
        ] {
            let mut event = events[1].clone();
            event.previous_hash = hash;
            event.event_hash = event_digest(&event).unwrap();
            rows.push((1, event, Conflict::PreviousHashMismatch, raw));
        }
        let mut identities = Vec::new();
        let mut event = events[1].clone();
        event.identity.request_id =
            RequestIdV2::parse("123e4567-e89b-12d3-a456-426614174002").unwrap();
        event.event_hash = event_digest(&event).unwrap();
        identities.push(event);
        let mut event = events[1].clone();
        event.identity.attempt_id =
            AttemptIdV2::parse("123e4567-e89b-12d3-a456-426614174003").unwrap();
        event.event_hash = event_digest(&event).unwrap();
        identities.push(event);
        let mut event = events[1].clone();
        event.identity.provider = ProviderIdentityV2::parse("other-provider").unwrap();
        event.event_hash = event_digest(&event).unwrap();
        identities.push(event);
        let mut event = events[1].clone();
        event.identity.channel = ChannelIdentityV2::parse("other-channel").unwrap();
        event.event_hash = event_digest(&event).unwrap();
        identities.push(event);
        let mut event = events[1].clone();
        event.identity.nonce = NonceV2::parse("other-nonce").unwrap();
        event.event_hash = event_digest(&event).unwrap();
        identities.push(event);
        for event in identities {
            rows.push((1, event, Conflict::IdentityMismatch, b"identity".as_slice()));
        }
        for (prefix, event, reason, raw) in rows {
            rejects(&events[..prefix], event, reason, raw);
        }
    }
    #[test]
    fn phase_order_repeats_and_post_receipt_conflict_without_over_suppression() {
        let events = chain();
        let matrix = [
            [
                None,
                Some(Conflict::IllegalFirstPhase),
                Some(Conflict::IllegalFirstPhase),
                Some(Conflict::IllegalFirstPhase),
            ],
            [
                Some(Conflict::RepeatedBound),
                None,
                Some(Conflict::TerminalBeforeStart),
                Some(Conflict::ReceiptBeforeTerminal),
            ],
            [
                Some(Conflict::RepeatedBound),
                Some(Conflict::RepeatedStarted),
                None,
                Some(Conflict::ReceiptBeforeTerminal),
            ],
            [
                Some(Conflict::RepeatedBound),
                Some(Conflict::RepeatedStarted),
                Some(Conflict::RepeatedTerminal),
                None,
            ],
            [Some(Conflict::EventAfterReceipt); 4],
        ];
        for (row, reasons) in matrix.into_iter().enumerate() {
            let prefix = &events[..row];
            for (column, reason) in reasons.into_iter().enumerate() {
                let incoming = fixture(
                    row as u64,
                    prefix.last().map(|event| event.event_hash),
                    kind(column),
                );
                let mut history = prefix
                    .iter()
                    .cloned()
                    .map(|event| accepted(event, b"prior"))
                    .collect::<Vec<_>>();
                history.push(accepted(incoming, b"cell"));
                let result = reduce(&history);
                match reason {
                    Some(reason) => assert_eq!(
                        result,
                        Disposition::Conflict {
                            reason,
                            raw: b"cell".as_slice().into()
                        }
                    ),
                    None => assert!(matches!(result, Disposition::Accepted { .. })),
                }
            }
        }
    }
    #[test]
    fn nonaccepted_dispositions_propagate_exactly_and_reducer_is_total() {
        let variants = [
            Disposition::Conflict {
                reason: Conflict::IdentityMismatch,
                raw: b"conflict".as_slice().into(),
            },
            Disposition::Poison {
                reason: PhasePoisonV2::InvalidEventHash,
                raw: b"poison".as_slice().into(),
            },
            Disposition::Unsupported {
                raw: b"unsupported".as_slice().into(),
            },
        ];
        for value in variants {
            for position in 0..=2 {
                let events = chain();
                let mut history = events
                    .into_iter()
                    .take(2)
                    .map(|event| accepted(event, b"prior"))
                    .collect::<Vec<_>>();
                history.insert(position, value.clone());
                let result = std::panic::catch_unwind(|| reduce(&history)).unwrap();
                assert_eq!(result, propagated(&value));
            }
        }
        let empty = std::panic::catch_unwind(|| reduce(&[])).unwrap();
        assert_eq!(
            empty,
            Disposition::Conflict {
                reason: Conflict::EmptyHistory,
                raw: Box::default()
            }
        );
        let malformed = fixture(u64::MAX, Some(EventDigestV2([9; 32])), kind(0));
        let history = [accepted(malformed, b"malformed")];
        let result = std::panic::catch_unwind(|| reduce(&history)).unwrap();
        assert_eq!(
            result,
            Disposition::Conflict {
                reason: Conflict::SequenceGap {
                    expected: 0,
                    actual: u64::MAX
                },
                raw: b"malformed".as_slice().into()
            }
        );
    }
}
