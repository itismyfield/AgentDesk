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
