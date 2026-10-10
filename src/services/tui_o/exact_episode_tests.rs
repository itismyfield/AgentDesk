use super::*;

pub(crate) fn fixture() -> Vec<EpisodeMetadata> {
    let episode = Uuid::from_u128(1);
    let source = SourceIdentity {
        incarnation: Uuid::from_u128(2),
        opener: 10,
        digest: "native".into(),
    };
    let piece = ExactPieceRef {
        episode,
        source: source.clone(),
        native_unit: "row-1".into(),
        kind: "tool_result_error".into(),
        range: (10, 20),
        plan_version: 1,
        plan_digest: "plan".into(),
        piece_index: 0,
        piece_count: 1,
        payload_digest: "body".into(),
        obligation: Uuid::from_u128(3),
        attempt: Uuid::from_u128(4),
    };
    let frontier = FrontierWitness {
        id: Uuid::from_u128(5),
        source: source.clone(),
        range: (10, 20),
        digest: "durable".into(),
    };
    let manifest = ExactTerminalManifest {
        source: source.clone(),
        seal: Some(TerminalSeal {
            source: source.clone(),
            terminal_identity: "own-terminal".into(),
            terminal_end: 20,
            capture_witness: Uuid::from_u128(6),
            derive_witness: Uuid::from_u128(7),
        }),
        captured_through: 20,
        derived_through: 20,
        membership_digest: "membership".into(),
        required: vec![piece.clone()],
        no_body_policy: None,
    };
    let pin = ExactEpisodePin {
        episode,
        owner: "owner".into(),
        execution_nonce: "execution".into(),
        turn_nonce: "turn".into(),
        inflight_identity: "inflight".into(),
        born_generation: 1,
        channel_id: "10".into(),
        expected_author: "bot".into(),
        source: Some(source),
        context: FrozenSettlementContext {
            intake: Some((9, 1)),
            dispatch: None,
            aliases: vec!["alias".into()],
            required_effects: vec!["intake".into()],
            policy_version: 1,
        },
    };
    vec![
        EpisodeEvidence::Pin(pin),
        EpisodeEvidence::Manifest(manifest.clone()),
        EpisodeEvidence::Obligation {
            piece: piece.clone(),
        },
        EpisodeEvidence::Attempt {
            piece: piece.clone(),
            frontier: frontier.clone(),
        },
        EpisodeEvidence::Transport {
            piece: piece.clone(),
            receipt: DirectReceipt {
                requested_channel: "10".into(),
                returned_channel: "10".into(),
                message_id: "100".into(),
                author: "bot".into(),
                payload_digest: "body".into(),
            },
        },
        EpisodeEvidence::Committed {
            piece: piece.clone(),
            frontier: frontier.clone(),
        },
        EpisodeEvidence::WholeFrontier {
            manifest,
            pieces: vec![piece],
            frontier,
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(i, e)| EpisodeMetadata::new(episode, Uuid::from_u128(100 + i as u128), e))
    .collect()
}
fn resolve(records: Vec<EpisodeMetadata>) -> StrictResolution {
    resolve_strict(
        Uuid::from_u128(1),
        &ConsistentEpisodeSnapshot::fixture(records),
    )
}
#[test]
fn exact_body_requires_full_child_attempt_receipt_and_whole_frontier() {
    let original = fixture();
    assert_eq!(resolve(original.clone()).authority(), Authority::Body);
    for index in 1..original.len() {
        let mut records = original.clone();
        records.remove(index);
        assert_eq!(
            resolve(records).authority(),
            Authority::Pending,
            "missing {index}"
        );
    }
    for field in 0..10 {
        let mut records = original.clone();
        if let EpisodeEvidence::Transport { piece, receipt } = &mut records[4].evidence {
            match field {
                0 => piece.attempt = Uuid::from_u128(99),
                1 => receipt.returned_channel = "wrong".into(),
                2 => receipt.message_id.clear(),
                3 => receipt.author.clear(),
                4 => receipt.payload_digest = "wrong".into(),
                5 => piece.episode = Uuid::from_u128(99),
                6 => piece.source.incarnation = Uuid::from_u128(99),
                7 => piece.piece_index = 1,
                8 => receipt.author = "other".into(),
                _ => {
                    receipt.requested_channel = "other".into();
                    receipt.returned_channel = "other".into();
                }
            }
        }
        assert_eq!(
            resolve(records).authority(),
            Authority::Pending,
            "changed {field}"
        );
    }
}
#[test]
fn exact_empty_and_parent_transport_never_grant_body() {
    let mut records = fixture();
    records.retain(|r| {
        matches!(
            r.evidence,
            EpisodeEvidence::Pin(_)
                | EpisodeEvidence::Manifest(_)
                | EpisodeEvidence::WholeFrontier { .. }
        )
    });
    if let EpisodeEvidence::Manifest(m) = &mut records[1].evidence {
        m.required.clear();
    }
    if let EpisodeEvidence::WholeFrontier {
        manifest, pieces, ..
    } = &mut records[2].evidence
    {
        manifest.required.clear();
        pieces.clear();
    }
    assert_eq!(resolve(records).authority(), Authority::Pending);
    let mut records = fixture();
    if let EpisodeEvidence::Transport { piece, .. } = &mut records[4].evidence {
        piece.obligation = piece.episode;
    }
    assert_eq!(resolve(records).authority(), Authority::Pending);
}
#[test]
fn exact_versioned_reader_is_closed_without_mode_fallback() {
    let original = fixture();
    for version in [0, 2, u32::MAX] {
        let mut records = original.clone();
        records[0].version = version;
        assert_eq!(resolve(records).authority(), Authority::Pending);
    }
    let old = serde_json::json!({"intake_outbox_id":9});
    assert!(serde_json::from_value::<EpisodeMetadata>(old).is_err());
    let mut future = serde_json::to_value(&original[0]).unwrap();
    future["extra"] = true.into();
    assert!(serde_json::from_value::<EpisodeMetadata>(future).is_err());
    assert_eq!(resolve(Vec::new()).authority(), Authority::Pending);
}
#[test]
fn exact_submission_closure_and_settlement_do_not_manufacture_body() {
    let mut records = fixture();
    records.truncate(1);
    records.push(EpisodeMetadata::new(
        Uuid::from_u128(1),
        Uuid::from_u128(200),
        EpisodeEvidence::SubmissionClosed {
            generation: 1,
            basis: SubmissionBasis::NoAttempt,
            policy_version: 1,
        },
    ));
    if let EpisodeEvidence::Pin(pin) = &mut records[0].evidence {
        pin.source = None;
    }
    assert_eq!(resolve(records.clone()).authority(), Authority::Policy);
    records.push(EpisodeMetadata::new(
        Uuid::from_u128(1),
        Uuid::from_u128(201),
        EpisodeEvidence::InputAttemptBegun {
            nonce: Uuid::from_u128(50),
        },
    ));
    assert_eq!(resolve(records.clone()).authority(), Authority::Pending);
    if let EpisodeEvidence::SubmissionClosed { basis, .. } = &mut records[1].evidence {
        *basis = SubmissionBasis::GateRefused {
            nonce: Uuid::from_u128(50),
        };
    }
    assert_eq!(resolve(records).authority(), Authority::Policy);
    let mut records = fixture();
    records.push(EpisodeMetadata::new(
        Uuid::from_u128(1),
        Uuid::from_u128(202),
        EpisodeEvidence::Settled {
            effects: Vec::new(),
        },
    ));
    assert_eq!(
        resolve(records.clone()).settlement(),
        Settlement::Outstanding
    );
    if let EpisodeEvidence::Settled { effects } = &mut records[7].evidence {
        effects.push("intake".into());
    }
    assert_eq!(resolve(records).settlement(), Settlement::Settled);
}

#[test]
fn exact_source_resolution_extra_proof_and_missing_piece_fail_closed() {
    let original = fixture();
    let mut resolved = original.clone();
    let source = if let EpisodeEvidence::Pin(pin) = &mut resolved[0].evidence {
        pin.source.take().unwrap()
    } else {
        panic!("fixture pin")
    };
    resolved.push(EpisodeMetadata::new(
        Uuid::from_u128(1),
        Uuid::from_u128(300),
        EpisodeEvidence::SourceResolved(source.clone()),
    ));
    assert_eq!(resolve(resolved.clone()).authority(), Authority::Body);
    resolved.push(EpisodeMetadata::new(
        Uuid::from_u128(1),
        Uuid::from_u128(301),
        EpisodeEvidence::SourceResolved(source),
    ));
    assert_eq!(resolve(resolved).authority(), Authority::Pending);
    let mut extra = original.clone();
    let mut proof = original[4].clone();
    proof.record = Uuid::from_u128(302);
    if let EpisodeEvidence::Transport { piece, .. } = &mut proof.evidence {
        piece.obligation = Uuid::from_u128(303);
    }
    extra.push(proof);
    assert_eq!(resolve(extra).authority(), Authority::Pending);
    let mut partial = original;
    for record in &mut partial {
        match &mut record.evidence {
            EpisodeEvidence::Obligation { piece }
            | EpisodeEvidence::Attempt { piece, .. }
            | EpisodeEvidence::Transport { piece, .. }
            | EpisodeEvidence::Committed { piece, .. } => piece.piece_count = 2,
            EpisodeEvidence::Manifest(manifest) => manifest.required[0].piece_count = 2,
            EpisodeEvidence::WholeFrontier {
                manifest, pieces, ..
            } => {
                manifest.required[0].piece_count = 2;
                pieces[0].piece_count = 2;
            }
            _ => {}
        }
    }
    assert_eq!(resolve(partial).authority(), Authority::Pending);
}

#[test]
fn exact_repeated_settlement_is_monotone() {
    let mut records = fixture();
    for id in [501, 502] {
        records.push(EpisodeMetadata::new(
            Uuid::from_u128(1),
            Uuid::from_u128(id),
            EpisodeEvidence::Settled {
                effects: vec!["intake".into()],
            },
        ));
        assert_eq!(resolve(records.clone()).settlement(), Settlement::Settled);
    }
}

#[test]
fn exact_conflicting_settlements_never_union_effects() {
    for bad in [
        vec![],
        vec!["other".to_string()],
        vec!["intake".to_string(), "intake".to_string()],
    ] {
        for reverse in [false, true] {
            let mut records = fixture();
            let mut effects = vec![vec!["intake".to_string()], bad.clone()];
            if reverse {
                effects.reverse();
            }
            for (i, effects) in effects.into_iter().enumerate() {
                records.push(EpisodeMetadata::new(
                    Uuid::from_u128(1),
                    Uuid::from_u128(801 + i as u128),
                    EpisodeEvidence::Settled { effects },
                ));
            }
            assert_eq!(resolve(records).settlement(), Settlement::Outstanding);
        }
    }
    let mut records = fixture();
    if let EpisodeEvidence::Pin(p) = &mut records[0].evidence {
        p.context.required_effects = vec!["intake".into(), "mailbox".into()];
    }
    for (i, effect) in ["intake", "mailbox"].into_iter().enumerate() {
        records.push(EpisodeMetadata::new(
            Uuid::from_u128(1),
            Uuid::from_u128(901 + i as u128),
            EpisodeEvidence::Settled {
                effects: vec![effect.into()],
            },
        ));
    }
    assert_eq!(resolve(records).settlement(), Settlement::Outstanding);
}
