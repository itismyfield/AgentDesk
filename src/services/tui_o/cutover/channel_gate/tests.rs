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

// The helper is where a Legacy body ends a pending adoption: its send runs right after the claim
// and finds it released, O's channel sends nothing, and a body-free send claims nothing.
#[tokio::test(flavor = "current_thread")]
async fn claim_then_send_releases_a_pending_adoption_only_for_the_send_it_runs() {
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
        assert_eq!(sent, Ok(BodySend::Sent(Adoption::Released)));
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

// A claimed Legacy body stays counted against its channel's adoption until its transport is done,
// a held one until the caller drops it; O's channel, a peek and an unclaimed send count nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_claimed_legacy_send_stays_counted_until_its_transport_is_done() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::{Adoption, Candidate};
    let claim = |channel| Some(BodyClaim::new(channel, Some(ClaudeTui)));
    let candidate = |channel| -> Candidate {
        test_override::with_channels(|boot| boot?.candidate(channel).cloned()).unwrap()
    };
    let _pending = test_override::force_candidates(&[(61, ClaudeTui), (62, ClaudeTui)]);
    let (pending, deferred) = (candidate(61), candidate(62));
    assert!(deferred.defer(62));
    for (channel, adoption) in [(61, &pending), (62, &deferred)] {
        let during = claim_then_send(claim(channel), || async { adoption.sends() }).await;
        assert_eq!(
            during,
            Ok(BodySend::Sent((1, 0))),
            "{channel}: counted while it sends"
        );
        assert_eq!(adoption.sends(), (1, 1), "{channel}: done once the send is");
    }
    assert_eq!(
        deferred.peek(),
        Adoption::Deferred,
        "a body leaves a deferral in place"
    );
    let held = claim_then_send_held(claim(62), || async { deferred.sends() }).await;
    let (sent, held) = held.unwrap();
    assert_eq!(sent, BodySend::Sent((2, 1)));
    assert_eq!(
        deferred.sends(),
        (2, 1),
        "still counted after its first step"
    );
    drop(held);
    assert_eq!(deferred.sends(), (2, 2));
    let unclaimed = claim_then_send(None, || async { deferred.sends() }).await;
    assert_eq!(unclaimed, Ok(BodySend::Sent((2, 2))));
    assert_eq!(
        peek_o_owns_tui_output_for_channel(62, Some(ClaudeTui)),
        Ok(false)
    );
    assert_eq!(deferred.sends(), (2, 2), "a peek sends nothing");

    let _owned = test_override::force_channels(&[(63, ClaudeTui)]);
    let owned = claim_then_send_held(claim(63), || async {}).await;
    assert!(matches!(owned, Ok((BodySend::OwnedByO, None))));
    assert_eq!(
        candidate(63).sends(),
        (0, 0),
        "O's channel reserves no Legacy send"
    );
}
