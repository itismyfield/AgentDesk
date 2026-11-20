use super::*;
use crate::services::tui_o::cutover::test_override;

// A verified empty list is O off for every destination, unknown ones included; no snapshot holds.
#[test]
fn an_empty_list_leaves_even_an_unknown_destination_to_legacy_while_no_snapshot_holds() {
    {
        let _empty = test_override::force_channels(&[]);
        assert_eq!(o_owns_tui_output_for_channel(0, None), Ok(false));
        assert_eq!(
            o_owns_tui_output_for_channel_tmux(0, Some("unbound-session")),
            Ok(false)
        );
    }
    assert_eq!(
        o_owns_tui_output_for_channel(0, None),
        Err(IdentityError::MissingSnapshot),
        "an uninstalled snapshot must never read as the empty list"
    );
}

// The helper holds a pending channel's body: its send never runs and the adoption stays, O's
// channel sends nothing, and a body-free send claims nothing.
#[tokio::test(flavor = "current_thread")]
async fn claim_then_send_holds_a_pending_body_and_runs_no_send_until_o_adopts() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::{Adoption, BodyCheck, SinkOp};
    let claim = |channel| Some(BodyClaim::new(channel, Some(ClaudeTui)));
    {
        let _pending = test_override::force_candidates(&[(51, ClaudeTui)]);
        let check = BodyCheck::watch(51, "body");
        let quiet = claim_then_send(None, || async { check.adoption() }).await;
        assert_eq!(quiet, Ok(BodySend::Sent(Adoption::Pending)));
        let sent = claim_then_send(claim(51), || async {
            check.sink(51, SinkOp::Post, "body");
            check.adoption()
        })
        .await;
        assert_eq!(sent, Err(IdentityError::AwaitingAdoption));
        check.assert_settled();
    }
    let _owned = test_override::force_channels(&[(52, ClaudeTui)]);
    let ran = std::cell::Cell::new(false);
    let owned = claim_then_send(claim(52), || async { ran.set(true) }).await;
    assert_eq!(owned, Ok(BodySend::OwnedByO));
    let indirect = claim(52).map(|claim| claim.direct(false));
    let indirect = claim_then_send(indirect, || async { ran.set(true) }).await;
    assert_eq!(indirect, Ok(BodySend::OwnedByO));
    assert!(!ran.get(), "nothing is sent on O's channel");
}

// A pending, deferred or refused channel's body is held whether its transport is held or not: no
// send runs or is counted, and the adoption stays; O's channel reserves no Legacy send either.
#[tokio::test(flavor = "current_thread")]
async fn a_held_body_runs_and_counts_no_legacy_send_in_any_unadopted_state() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::Candidate;
    let claim = |channel| Some(BodyClaim::new(channel, Some(ClaudeTui)));
    let candidate = |channel| -> Candidate {
        test_override::with_channels(|boot| boot?.candidate(channel).cloned()).unwrap()
    };
    let channels = [(61, ClaudeTui), (62, ClaudeTui), (64, ClaudeTui)];
    let _pending = test_override::force_candidates(&channels);
    let (pending, deferred, refused) = (candidate(61), candidate(62), candidate(64));
    assert!(deferred.defer(62));
    refused.release(64);
    let ran = std::cell::Cell::new(0);
    for (channel, adoption, error) in [
        (61, &pending, IdentityError::AwaitingAdoption),
        (62, &deferred, IdentityError::AwaitingAdoption),
        (64, &refused, IdentityError::AdoptionRefused),
    ] {
        let state = adoption.peek();
        let during = claim_then_send(claim(channel), || async { ran.set(ran.get() + 1) }).await;
        assert_eq!(during, Err(error), "{channel}");
        let held = claim_then_send_held(claim(channel), || async { ran.set(ran.get() + 1) });
        assert!(
            matches!(held.await, Err(held) if held == error),
            "{channel}"
        );
        assert_eq!(
            peek_o_owns_tui_output_for_channel(channel, Some(ClaudeTui)),
            Err(error)
        );
        assert_eq!((adoption.sends(), adoption.peek()), ((0, 0), state));
    }
    assert_eq!(ran.get(), 0, "no held body runs its send");
    let unclaimed = claim_then_send(None, || async { deferred.sends() }).await;
    assert_eq!(unclaimed, Ok(BodySend::Sent((0, 0))));

    let _owned = test_override::force_channels(&[(63, ClaudeTui)]);
    let owned = claim_then_send_held(claim(63), || async {}).await;
    assert!(matches!(owned, Ok((BodySend::OwnedByO, None))));
    assert_eq!(
        candidate(63).sends(),
        (0, 0),
        "O's channel reserves no Legacy send"
    );
}

#[test]
fn claim_reason_follows_gate_returns_without_changing_ownership() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    let observed = |channel| {
        test_override::with_channels(|snapshot| {
            decide_with_snapshot(snapshot, channel, Use::Body, None, || Some(ClaudeTui)).unwrap()
        })
    };
    {
        let _empty = test_override::force_channels(&[]);
        let answer = observed(0);
        assert!(!answer.owned);
        assert!(format!("{answer:?}").contains("reason: EmptySelection"));
    }
    {
        let _owned = test_override::force_channels(&[(701, ClaudeTui)]);
        let answer = observed(702);
        assert!(!answer.owned);
        assert!(format!("{answer:?}").contains("reason: NonMember"));
        let answer = observed(701);
        assert!(answer.owned);
        assert!(format!("{answer:?}").contains("reason: OwnedByO"));
    }
    let _foreign = test_override::force_foreign(&[(701, ClaudeTui)], "other-node");
    let held = test_override::with_channels(|snapshot| {
        decide_with_snapshot(snapshot, 701, Use::Body, None, || Some(ClaudeTui))
    });
    assert_eq!(held.unwrap_err(), IdentityError::AwaitingAdoption);
}

#[test]
fn claim_reason_distinguishes_adoption_transition_and_existing_states() {
    use crate::services::tui_o::channel_policy::{Adoption, Candidate};
    for (state, reason) in [
        (Adoption::Pending, "PendingReleased"),
        (Adoption::Released, "Released"),
        (Adoption::Refused, "Refused"),
        (Adoption::Deferred, "Deferred"),
        (Adoption::Committed, "OwnedByO"),
        (Adoption::Held, "OwnedHeld"),
    ] {
        let candidate = Candidate::new(state);
        let answer = candidate.claim_body(703);
        assert_eq!(answer.owned, state.owned());
        assert!(
            format!("{answer:?}").contains(&format!("reason: {reason}")),
            "{state:?}"
        );
        assert_eq!(candidate.sends(), (u64::from(!state.owned()), 0));
        assert_eq!(candidate.peek(), state, "a claim changes no adoption");
        drop(answer);
        assert_eq!(
            candidate.sends(),
            (u64::from(!state.owned()), u64::from(!state.owned()))
        );
        if !state.owned() {
            let second = candidate.claim_body(703);
            assert!(format!("{second:?}").contains(&format!("reason: {reason}")));
        }
    }
}

#[test]
fn claim_reason_is_fixed_while_the_existing_adoption_lock_excludes_activation() {
    use crate::services::tui_o::channel_policy::{Adoption, Candidate};
    use crate::services::tui_o::writer::activation::test_hook::{self, Step};
    use std::sync::{Arc, Barrier, mpsc};
    let candidate = Candidate::new(Adoption::Pending);
    let barrier = Arc::new(Barrier::new(2));
    let (arrived, waiting) = mpsc::channel();
    let paused = barrier.clone();
    test_hook::set(9_765_102, Step::BeforeLegacyStarted, move || {
        arrived.send(()).unwrap();
        paused.wait();
        Ok(())
    });
    let claiming = candidate.clone();
    let worker = std::thread::spawn(move || claiming.claim_body(9_765_102));
    waiting
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    assert!(candidate.locked_for_test());
    let activating = candidate.clone();
    let activation = std::thread::spawn(move || activating.confirm_store());
    barrier.wait();
    let answer = worker.join().unwrap();
    assert_eq!(answer.reason, ClaimReason::PendingReleased);
    assert!(
        activation.join().unwrap(),
        "the claim left the adoption pending, so the activation commits once the lock frees"
    );
    assert_eq!(candidate.peek(), Adoption::Committed);
    assert_eq!(candidate.sends(), (1, 0));
    drop(answer);
    assert_eq!(candidate.sends(), (1, 1));
}

#[tokio::test(flavor = "current_thread")]
async fn site_api_keeps_identity_errors_owned_suppression_and_no_claim_independence() {
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
    let ran = std::cell::Cell::new(0);
    assert!(matches!(
        claim_then_send_at(BodySite::StandbyRelay, None, || async {
            ran.set(ran.get() + 1)
        })
        .await,
        Ok(BodySend::Sent(()))
    ));
    let _boot = test_override::force_channels(&[(9_765_103, ClaudeTui)]);
    let claim = |kind| Some(BodyClaim::new(9_765_103, kind));
    assert!(matches!(
        claim_then_send_at(BodySite::StandbyRelay, claim(Some(ClaudeTui)), || async {
            ran.set(ran.get() + 1)
        })
        .await,
        Ok(BodySend::OwnedByO)
    ));
    assert!(matches!(
        claim_then_send_at(BodySite::StandbyRelay, claim(None), || async {
            ran.set(ran.get() + 1)
        })
        .await,
        Err(IdentityError::UnknownKind)
    ));
    assert!(matches!(
        claim_then_send_at(BodySite::StandbyRelay, claim(Some(CodexTui)), || async {
            ran.set(ran.get() + 1)
        })
        .await,
        Err(IdentityError::KindMismatch { .. })
    ));
    assert_eq!(ran.get(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn an_unobserved_held_context_leaves_later_ambient_metadata_intact() {
    let (_, context) = claim_then_send_held_at(BodySite::StandbyRelay, None, || async {})
        .await
        .unwrap();
    let tag = Some(serde_json::json!({"site":"W30", "reason":"RELEASED"}));
    let actual = census_scope(tag.clone(), context.run(async { current_census() })).await;
    assert_eq!(actual, tag);
}
