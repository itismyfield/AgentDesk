use super::super::tests::{Entry, Run, durable_phase, managed_row, run};
use super::*;
use crate::services::claude_tui::busy_inject::{Veto, WriteFailure};

fn keys(sends: &[(String, String)]) -> Vec<&str> {
    sends.iter().map(|(key, _)| key.as_str()).collect()
}

/// Runs the managed submission as this attempt's leaf: no write or only a failed load is a
/// no-key result; anything after the load may have reached the pane.
fn leaf(
    submission: &ManagedSubmission,
    channel: u64,
    cut: Option<usize>,
) -> (Run, Option<NoContentProof>) {
    let mut ran = None;
    let (_, proof) = submission.run_leaf(|| {
        let run = run(Entry::Fresh, channel, "same request", submission, cut);
        let outcome = match (keys(&run.sends).as_slice(), &run.result) {
            ([], _) => Outcome::NotSent(Veto::LockContended),
            (["load"], _) => Outcome::NotSent(Veto::LoadFailed),
            (_, Ok(())) => Outcome::Injected,
            (_, Err(_)) => Outcome::Failed(WriteFailure::Enter),
        };
        ran = Some(run);
        outcome
    });
    (ran.unwrap(), proof)
}

fn durable_row(channel: u64) -> serde_json::Value {
    let row = super::super::super::load_inflight_state(&ProviderKind::Claude, channel).unwrap();
    serde_json::to_value(row).unwrap()
}

/// The production guarded writer refuses each stale snapshot of the ended birth unchanged.
fn assert_stale_snapshots_refused(channel: u64, stale: [&InflightTurnState; 2]) {
    let before = durable_row(channel);
    for snapshot in stale {
        let saved = super::super::super::save_inflight_state_if_identity_unchanged(
            snapshot,
            "managed_outcome_stale_snapshot",
        );
        let phase = &snapshot.managed_submission;
        assert_eq!(saved, GuardedSaveOutcome::SuccessorOwned, "{phase:?}");
    }
    assert_eq!(durable_row(channel), before);
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
    let (failed, proof) = leaf(&first, channel, Some(0));
    assert!(failed.result.is_err());
    assert_eq!(
        keys(&failed.sends),
        ["load"],
        "the load failed before any key"
    );

    first.conclude_not_submitted(proof.unwrap()).unwrap();
    assert_eq!(durable_phase(channel), Some(SubmissionPhase::NotSubmitted));
    let ended = super::super::super::load_inflight_state(&ProviderKind::Claude, channel).unwrap();
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
    assert_stale_snapshots_refused(channel, [&born, &ended]);

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
    assert_stale_snapshots_refused(channel, [&born, &ended]);
    assert_eq!(
        durable_phase(channel),
        Some(SubmissionPhase::MayHaveSubmitted)
    );
    let content = [&failed, &late, &stale, &done]
        .iter()
        .flat_map(|run| keys(&run.sends))
        .filter(|key| *key == "paste")
        .count();
    assert_eq!(content, 1, "the request reached the pane once");
}

/// A no-key proof ends only the birth whose leaf produced it: handed to an attempt that
/// submitted, it is refused and that row is left as it was.
#[test]
fn a_no_content_proof_cannot_end_another_birth() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let (failed_channel, done_channel) = (6_809_411, 6_809_412);
    let (_, failed) = managed_row(failed_channel);
    let (_, done) = managed_row(done_channel);
    let (_, proof) = leaf(&failed, failed_channel, Some(0));
    let (submitted, none) = leaf(&done, done_channel, None);
    assert_eq!(submitted.result, Ok(()));
    assert!(none.is_none());

    let before = durable_row(done_channel);
    assert!(done.conclude_not_submitted(proof.unwrap()).is_err());
    assert_eq!(durable_row(done_channel), before);
    assert_eq!(
        durable_phase(done_channel),
        Some(SubmissionPhase::MayHaveSubmitted)
    );
    assert!(done.successor("misrouted-birth".into()).is_err());
}

/// Only a positive no-key leaf result proves no content; a write failure does not.
#[test]
fn only_a_no_key_leaf_result_is_a_no_content_proof() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (_, submission) = managed_row(6_809_421);
    let proves = |outcome: Outcome| submission.run_leaf(|| outcome).1.is_some();
    assert!(proves(Outcome::NotSent(Veto::LoadFailed)));
    assert!(proves(Outcome::NotSent(Veto::PaneUnavailable)));
    for written in [
        Outcome::Injected,
        Outcome::Failed(WriteFailure::Stash),
        Outcome::Failed(WriteFailure::Paste),
        Outcome::Failed(WriteFailure::Enter),
    ] {
        assert!(!proves(written), "{written:?}");
    }
}
