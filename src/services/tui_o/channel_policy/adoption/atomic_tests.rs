use super::super::output_disposition::{Identity, Output, Phase, Reason};
use super::*;
use tokio::time::{Duration, Instant};

fn output(proof: &FirstProof, expired: bool) -> Output {
    Output::new(
        Identity::from_proof(proof, proof.mark.order),
        Instant::now() - Duration::from_secs(if expired { 26 } else { 0 }),
    )
}

#[test]
fn rejection_wins_the_candidate_lock_and_prevents_the_waiting_actor_init() {
    let f = Fixture::new(8_165_991);
    let proof = f.proof();
    let output = output(&proof, true);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    test_hook::set(f.channel, Step::BeforeLock, move || {
        entered_tx.send(()).unwrap();
        recv_within(&release_rx, WAIT_CAP);
        Ok(())
    });
    let (store, candidate, channel, actor_proof) = (
        f.store.clone(),
        f.candidate.clone(),
        f.channel,
        proof.clone(),
    );
    let actor = std::thread::spawn(move || activate(&store, channel, &candidate, &actor_proof));
    recv_within(&entered_rx, WAIT_CAP);
    assert_eq!(
        output.finish_first(&f.candidate, &proof, &f.store, Reason::Deadline),
        Err(Reason::Deadline)
    );
    release_tx.send(()).unwrap();
    assert!(join_within(actor, WAIT_CAP).is_err());
    assert!(f.store.read_init(f.channel).unwrap().is_none());
    assert_eq!(f.candidate.peek(), Adoption::Refused);
    assert!(f.candidate.first_mark().is_none());
    assert_eq!(output.phase(), Phase::RejectPendingNotice(Reason::Deadline));
}

#[test]
fn published_init_wins_expiry_even_after_a_write_error_and_current_nonce_changes() {
    let f = Fixture::new(8_165_992);
    let proof = f.proof();
    let output = output(&proof, true);
    test_hook::set(f.channel, Step::AfterWrite, || {
        Err("lost init acknowledgement".into())
    });
    f.activate().unwrap();
    let before = serde_json::to_vec(&f.store.read_init(f.channel).unwrap().unwrap()).unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&proof.context.tmux_session, "spawn_nonce"),
        "successor-execution",
    )
    .unwrap();
    assert_eq!(
        output.finish_first(&f.candidate, &proof, &f.store, Reason::Deadline),
        Ok(proof)
    );
    assert_eq!(output.phase(), Phase::Adopted);
    assert!(!output.reject(Reason::Deadline));
    assert_eq!(
        serde_json::to_vec(&f.store.read_init(f.channel).unwrap().unwrap()).unwrap(),
        before
    );
}

#[test]
fn committed_source_identity_is_required_and_ambiguous_effect_never_becomes_rejection() {
    for fault in ["source", "prefix", "origin", "damaged", "held"] {
        let f = Fixture::new(8_165_993);
        let proof = f.proof();
        let output = output(&proof, true);
        f.activate().unwrap();
        let path = crate::config::runtime_root()
            .unwrap()
            .join("o_store")
            .join(f.channel.to_string())
            .join("init");
        let mut init: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        match fault {
            "source" => init["sources"][0]["source_id"]["session_id"] = "foreign".into(),
            "prefix" => init["sources"][0]["prefix_hash"] = "foreign".into(),
            "origin" => init["origin"] = "fresh".into(),
            "held" => *f.candidate.lock() = Adoption::Held,
            "damaged" => {}
            _ => unreachable!(),
        }
        std::fs::write(
            &path,
            if fault == "damaged" {
                b"{".to_vec()
            } else {
                serde_json::to_vec(&init).unwrap()
            },
        )
        .unwrap();
        assert!(
            output
                .finish_first(&f.candidate, &proof, &f.store, Reason::Deadline)
                .is_err(),
            "{fault}"
        );
        assert_eq!(output.phase(), Phase::ExistingEffectPending, "{fault}");
        assert!(!output.reject(Reason::Deadline), "{fault}");
        assert!(!output.take_repair(), "{fault}");
        *f.candidate.lock() = Adoption::Deferred;
        assert!(
            output
                .finish_first(&f.candidate, &proof, &f.store, Reason::Deadline)
                .is_err()
        );
        assert_eq!(output.phase(), Phase::ExistingEffectPending);
    }
}

#[test]
fn old_output_rejection_cannot_invalidate_a_successor_first_slot() {
    let f = Fixture::new(8_165_994);
    let proof = f.proof();
    let output = output(&proof, true);
    let next = f
        .candidate
        .admit_first(
            f.channel,
            "successor-input",
            "hi",
            &proof.context.tmux_session,
            || Ok(0),
        )
        .unwrap();
    f.candidate
        .attach_first_execution(&f.prepared.context, "hi", Some("successor-input"));
    let successor = f.proof();
    assert_ne!(successor.mark.order, proof.mark.order);
    assert_eq!(
        output.finish_first(&f.candidate, &proof, &f.store, Reason::Deadline),
        Err(Reason::Invalidated)
    );
    assert_eq!(f.proof(), successor);
    assert_eq!(f.candidate.peek(), Adoption::Deferred);
    assert_eq!(
        output.phase(),
        Phase::RejectPendingNotice(Reason::Invalidated)
    );
    drop(next);
}

#[test]
fn identity_mismatch_closes_the_attempt_without_touching_the_first_slot() {
    let f = Fixture::new(8_165_995);
    let proof = f.proof();
    let mut id = Identity::from_proof(&proof, proof.mark.order);
    id.input += 1;
    let output = Output::new(id, Instant::now());
    assert!(
        output
            .finish_first(&f.candidate, &proof, &f.store, Reason::Deadline)
            .is_err()
    );
    assert_eq!(output.phase(), Phase::ExistingEffectPending);
    assert_eq!(f.proof(), proof);
    assert_eq!(f.candidate.peek(), Adoption::Deferred);
}
