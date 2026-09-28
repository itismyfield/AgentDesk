use super::super::schema::{
    Attempt, ChunkReceipt, CleanupState, ObligationClass, ObligationLedger, OpenObligation,
    Publication, SourceToken,
};
use super::proof_input::{ProbedAttempt, ProbedChunk, ProofRejection};
use super::{Evidence, EvidenceSnapshot};

fn fixture() -> (ObligationLedger, ProbedAttempt) {
    let publication = Publication {
        epoch: SourceToken {
            generation_mtime_ns: 42,
            source_dev: 7,
            source_ino: 9,
            serial: 1,
            reset_incarnation: 1,
        },
        extent_end: 200,
        digest: [7; 16],
        rev: 5,
    };
    let attempt = Attempt {
        key: "attempt-a".into(),
        range: (100, 200),
        prepared_at_ms: Some(1000),
        chunk_nonces: Some(vec!["nonce-0".into(), "nonce-1".into()]),
        chunk_total: Some(2),
        receipts: vec![],
    };
    let probe = ProbedAttempt {
        publication,
        attempt_key: attempt.key.clone(),
        chunks: (0..2)
            .map(|chunk| ProbedChunk {
                nonce: format!("nonce-{chunk}"),
                receipt: ChunkReceipt {
                    chunk,
                    message_id: 500 + u64::from(chunk),
                    cleanup: CleanupState::NotRemoved,
                },
            })
            .collect(),
    };
    let ledger = ObligationLedger {
        publication,
        open: vec![OpenObligation {
            range: attempt.range,
            origin: attempt.range,
            class: ObligationClass::Owed,
            attempt: Some(attempt),
            unresolved: true,
            unresolved_since_ms: Some(1001),
            redrive_capped: None,
        }],
        held: vec![],
        intents: vec![],
    };
    (ledger, probe)
}

#[test]
fn whole_proof_binds_complete_chunks_to_the_durable_attempt() {
    let (ledger, mut probe) = fixture();
    probe.chunks.reverse();
    let verified = EvidenceSnapshot::verify_whole(&ledger, probe).unwrap();
    assert_eq!(verified.publication(), ledger.publication);
    assert_eq!(verified.attempt_key(), "attempt-a");
    let Evidence::WholeProof(proof) = verified.evidence() else {
        panic!("whole proof missing")
    };
    assert_eq!(proof.publication(), verified.publication());
    assert_eq!(proof.attempt_key(), verified.attempt_key());
    assert_eq!(proof.range(), (100, 200));
    assert_eq!(
        proof.receipts().iter().map(|r| r.chunk).collect::<Vec<_>>(),
        [0, 1]
    );
}

#[test]
fn whole_proof_rejects_empty_duplicate_partial_and_unproven_cleanup() {
    use ProofRejection::*;
    for (case, reason) in [
        ("empty", EmptyProof),
        ("duplicate", DuplicateChunk),
        ("partial", MissingChunk),
        ("out_of_range", ChunkOutOfRange),
        ("nonce", NonceMismatch),
        ("removed", CleanupUnproven),
        ("unknown", CleanupUnproven),
        ("message", InvalidMessageId),
        ("duplicate_message", InvalidMessageId),
    ] {
        let (ledger, mut probe) = fixture();
        match case {
            "empty" => probe.chunks.clear(),
            "duplicate" => probe.chunks[1] = probe.chunks[0].clone(),
            "partial" => {
                probe.chunks.pop();
            }
            "out_of_range" => probe.chunks[1].receipt.chunk = 2,
            "nonce" => probe.chunks[1].nonce = "another-nonce".into(),
            "removed" => probe.chunks[1].receipt.cleanup = CleanupState::Removed,
            "unknown" => probe.chunks[1].receipt.cleanup = CleanupState::Unknown,
            "message" => probe.chunks[1].receipt.message_id = 0,
            "duplicate_message" => {
                probe.chunks[1].receipt.message_id = probe.chunks[0].receipt.message_id
            }
            _ => unreachable!(),
        }
        assert_eq!(
            EvidenceSnapshot::verify_whole(&ledger, probe),
            Err(reason),
            "{case}"
        );
    }
}

#[test]
fn whole_proof_rejects_unbound_epoch_attempt_revision_and_invalid_plans() {
    use ProofRejection::*;
    for (case, reason) in [
        ("generation", EpochMismatch),
        ("device", EpochMismatch),
        ("inode", EpochMismatch),
        ("serial", EpochMismatch),
        ("reset", EpochMismatch),
        ("revision", RevisionMismatch),
        ("extent", PublicationMismatch),
        ("digest", PublicationMismatch),
        ("attempt", AttemptMismatch),
        ("empty_key", AttemptMismatch),
        ("unknown_plan", InvalidPlan),
        ("empty_range", InvalidPlan),
        ("outside_extent", InvalidPlan),
        ("zero_plan", InvalidPlan),
        ("missing_nonce", InvalidPlan),
        ("duplicate_nonce", InvalidPlan),
        ("empty_nonce", InvalidPlan),
        ("conflicting_attempt", InvalidPlan),
    ] {
        let (mut ledger, mut probe) = fixture();
        match case {
            "generation" => probe.publication.epoch.generation_mtime_ns += 1,
            "device" => probe.publication.epoch.source_dev += 1,
            "inode" => probe.publication.epoch.source_ino += 1,
            "serial" => probe.publication.epoch.serial += 1,
            "reset" => probe.publication.epoch.reset_incarnation += 1,
            "revision" => probe.publication.rev += 1,
            "extent" => probe.publication.extent_end += 1,
            "digest" => probe.publication.digest[0] += 1,
            "attempt" => probe.attempt_key = "different-attempt".into(),
            "empty_key" => probe.attempt_key.clear(),
            _ => {
                let attempt = ledger.open[0].attempt.as_mut().unwrap();
                match case {
                    "unknown_plan" => attempt.chunk_nonces = None,
                    "empty_range" => attempt.range = (100, 100),
                    "outside_extent" => attempt.range.1 = 201,
                    "zero_plan" => {
                        attempt.chunk_total = Some(0);
                        attempt.chunk_nonces = Some(vec![]);
                    }
                    "missing_nonce" => {
                        attempt.chunk_nonces.as_mut().unwrap().pop();
                    }
                    "duplicate_nonce" => {
                        attempt.chunk_nonces.as_mut().unwrap()[1] = "nonce-0".into()
                    }
                    "empty_nonce" => attempt.chunk_nonces.as_mut().unwrap()[1].clear(),
                    "conflicting_attempt" => {
                        let mut other = ledger.open[0].clone();
                        other.attempt.as_mut().unwrap().range = (100, 150);
                        ledger.open.push(other);
                    }
                    _ => unreachable!(),
                }
            }
        }
        assert_eq!(
            EvidenceSnapshot::verify_whole(&ledger, probe),
            Err(reason),
            "{case}"
        );
    }
}
