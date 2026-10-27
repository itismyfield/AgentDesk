use super::super::tests::{Entry, durable_phase, managed_row, run};
use super::*;
use crate::services::claude_tui::busy_inject::Veto;

fn keys(sends: &[(String, String)]) -> Vec<&str> {
    sends.iter().map(|(key, _)| key.as_str()).collect()
}

/// A load failure that wrote nothing ends its birth for good; the same request then succeeds
/// once on a distinct birth, and the ended capability or a stale snapshot cannot touch it.
#[test]
fn a_not_submitted_birth_hands_the_request_to_one_distinct_successor() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let channel = 6_809_401;
    let (born, first) = managed_row(channel);
    let failed = run(Entry::Fresh, channel, "same request", &first, Some(0));
    assert!(failed.result.is_err());
    assert_eq!(
        keys(&failed.sends),
        ["load"],
        "the load failed before any key"
    );

    let proof = NoContentProof::from_leaf(Outcome::NotSent(Veto::LoadFailed)).unwrap();
    first.conclude_not_submitted(proof).unwrap();
    assert_eq!(durable_phase(channel), Some(SubmissionPhase::NotSubmitted));
    // A stale snapshot of the same birth cannot lower the terminal back to Waiting.
    super::super::super::save_inflight_state(&born).unwrap();
    assert_eq!(durable_phase(channel), Some(SubmissionPhase::NotSubmitted));
    let late = run(Entry::Fresh, channel, "same request", &first, None);
    assert!(late.result.is_err() && late.sends.is_empty());

    let next = first.successor("successor-birth".into()).unwrap();
    let row = super::super::super::load_inflight_state(&ProviderKind::Claude, channel).unwrap();
    assert_eq!(
        (row.user_msg_id, &row.user_text, row.request_owner_user_id),
        (
            born.user_msg_id,
            &born.user_text,
            born.request_owner_user_id
        ),
        "the successor keeps the same request"
    );
    assert!(
        !InflightEpisodePin::from_state(&row)
            .is_same_episode_as(&InflightEpisodePin::from_state(&born)),
        "the successor is a distinct birth"
    );
    assert_eq!(durable_phase(channel), Some(SubmissionPhase::Waiting));

    // The ended capability neither types nor moves the successor's row.
    let stale = run(Entry::Fresh, channel, "same request", &first, None);
    assert!(stale.result.is_err() && stale.sends.is_empty());
    assert!(first.fence().before_first_payload().is_err());
    assert!(first.successor("another-birth".into()).is_err());
    assert_eq!(durable_phase(channel), Some(SubmissionPhase::Waiting));

    let done = run(Entry::Fresh, channel, "same request", &next, None);
    assert_eq!(done.result, Ok(()));
    assert_eq!(keys(&done.sends), ["load", "paste", "Enter"]);
    assert!(
        done.sends
            .iter()
            .all(|(_, phase)| phase == "may_have_submitted")
    );
    assert!(
        next.successor("third-birth".into()).is_err(),
        "a submitted birth has no successor"
    );
}

/// Only a positive no-key leaf result proves no content; a write failure does not.
#[test]
fn only_a_no_key_leaf_result_is_a_no_content_proof() {
    use crate::services::claude_tui::busy_inject::WriteFailure;
    assert!(NoContentProof::from_leaf(Outcome::NotSent(Veto::LoadFailed)).is_some());
    assert!(NoContentProof::from_leaf(Outcome::NotSent(Veto::PaneUnavailable)).is_some());
    for written in [
        Outcome::Injected,
        Outcome::Failed(WriteFailure::Stash),
        Outcome::Failed(WriteFailure::Paste),
        Outcome::Failed(WriteFailure::Enter),
    ] {
        assert!(NoContentProof::from_leaf(written).is_none(), "{written:?}");
    }
}
