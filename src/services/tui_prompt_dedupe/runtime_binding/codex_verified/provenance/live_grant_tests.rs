use super::super::decisions::*;
use super::super::dormant::construction::{
    EpisodeEvidenceSnapshot, LiveGrantRequest, submit_warm, submitted_episode,
};
use super::*;
use crate::services::codex_tui::input::{
    CodexFollowupPromptSubmitOutcome as Outcome, PromptReadinessSnapshot,
};
use crate::services::tui_o::writer::actor::codex_submission::Prepared;
use crate::services::tui_o::writer::actor::p5a2_tests::Fixture;

fn evidence(f: &Fixture) -> (Arc<CancelToken>, EpisodeEvidenceSnapshot) {
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        f.launch.episode.turn_nonce.clone(),
    )));
    let evidence =
        EpisodeEvidenceSnapshot::capture(token.clone(), f.launch.episode.clone()).unwrap();
    (token, evidence)
}
fn open(f: &Fixture, prepared: &Prepared, through: u64) -> (LiveGrant, Arc<CancelToken>) {
    let (token, evidence) = evidence(f);
    let receipt = submit_warm(&evidence, &token, &f.launch.context, || Outcome::Submitted).unwrap();
    let submitted = submitted_episode(&evidence, &token, &f.launch.context, &receipt).unwrap();
    let grant = receipt
        .issue_live(
            &f.ready,
            LiveGrantRequest {
                evidence: &evidence,
                submitted: &submitted,
                prepared,
                source: &f.source,
                through,
            },
        )
        .unwrap();
    (grant, token)
}

#[test]
fn p5a2_decisions_preserve_purpose_and_reason() {
    use crate::services::tui_o::store::StoreError;
    assert_eq!(
        PersistenceReason::from_store(&StoreError::Rejected("conflict".into())),
        PersistenceReason::IdentityMismatch
    );
    assert_eq!(
        PersistenceReason::from_store(&StoreError::Io(std::io::Error::other("disk"))),
        PersistenceReason::StorageFailed
    );
    let publication = PublicationDecision::MetadataPublished(7);
    let checkpoint = CheckpointDecision::<u64>::MetadataOnly(CheckpointReason::NoProcessedWitness);
    let live = LiveReadDecision::<u64>::Held(LiveReadReason::Evidence(E::MissingEvidence));
    let historical = HistoricalDecision::Readable(7);
    let post = PostDecision::<u64>::Held(PostReason::MissingExactPermit);
    assert!(matches!(
        publication,
        PublicationDecision::MetadataPublished(7)
    ));
    assert!(matches!(checkpoint, CheckpointDecision::MetadataOnly(_)));
    assert!(matches!(live, LiveReadDecision::Held(_)));
    assert!(matches!(historical, HistoricalDecision::Readable(7)));
    assert!(matches!(
        post,
        PostDecision::Held(PostReason::MissingExactPermit)
    ));
    assert_ne!(
        LiveReadReason::Evidence(E::PermissionUnknown),
        LiveReadReason::Evidence(E::Cancelled)
    );
    assert_ne!(
        PersistenceReason::ReadBackMismatch,
        PersistenceReason::StorageFailed
    );
}

#[tokio::test(start_paused = true)]
async fn p5a2_actual_submit_receipt_opens_one_finite_grant() {
    let f = Fixture::new(true).await;
    let prepared = f.prepare().await;
    let snap = PromptReadinessSnapshot {
        composer_marker_detected: false,
        prompt_draft_detected: false,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: String::new(),
    };
    for outcome in [
        Outcome::NotSubmitted {
            error: "refused".into(),
        },
        Outcome::Cancelled,
        Outcome::RetrySafeDraft {
            first: snap.clone(),
            second: snap.clone(),
        },
        Outcome::Unconfirmed {
            error: "unknown".into(),
            snapshot: snap,
        },
        Outcome::Refused {
            run: crate::services::codex_tui::host_input::InputRun::Refused(
                crate::services::codex_tui::host_input::InputRefusal::Conflict,
            ),
        },
    ] {
        let (token, evidence) = evidence(&f);
        assert!(submit_warm(&evidence, &token, &f.launch.context, || outcome).is_err());
    }
    let end = f.append("task_started", "t1");
    let (grant, _) = open(&f, &prepared, end);
    assert_eq!(grant.frontier(), end);
    assert_eq!(grant.lifecycle(), Lifecycle::Open);
    let LiveReadDecision::Granted(range) = grant.range(prepared.floor().checkpoint().1..end) else {
        panic!("positive range must be granted")
    };
    assert_eq!(range.span().episode.native_turn_id, "t1");
    assert_eq!(range.floor(), prepared.floor());
    assert_eq!(*range.bytes(), prepared.floor().checkpoint().1..end);
    assert!(matches!(
        grant.range(end..u64::MAX),
        LiveReadDecision::Held(LiveReadReason::Evidence(E::FiniteCloseExceeded))
    ));
    f.stop().await;
}

#[tokio::test(start_paused = true)]
async fn p5a2_two_warm_episodes_never_share_a_reader_grant() {
    let f = Fixture::new(true).await;
    let prepared = f.prepare().await;
    let opener = f.append("task_started", "t1");
    let (mut grant, _) = open(&f, &prepared, opener);
    let body = f.append("agent_message", "t1");
    assert_eq!(
        grant.frontier(),
        opener,
        "file length alone never grows a grant"
    );
    assert_eq!(grant.advance(opener + 1..body), Err(E::RangeGap.into()));
    assert_eq!(grant.close(body), Err(E::MissingEvidence.into()));
    assert_eq!(grant.frontier(), opener);
    grant.advance(opener..body).unwrap();
    let foreign = f.append("task_started", "t2");
    assert_eq!(grant.advance(body..foreign), Err(E::ForeignTurn.into()));
    assert_eq!(grant.frontier(), body);
    assert!(matches!(
        grant.range(body..foreign),
        LiveReadDecision::Held(_)
    ));
    let bytes = std::fs::read(&f.source.execution().source.path).unwrap();
    let mut changed = bytes.clone();
    changed[(body - 3) as usize] ^= 1;
    std::fs::write(&f.source.execution().source.path, &changed).unwrap();
    assert!(matches!(
        grant.range(prepared.floor().checkpoint().1..body),
        LiveReadDecision::Held(LiveReadReason::Evidence(E::PrefixChanged))
    ));
    std::fs::write(&f.source.execution().source.path, bytes).unwrap();
    f.stop().await;
}

#[tokio::test(start_paused = true)]
async fn p5a2_closed_buffered_tail_survives_unavailable_and_restore() {
    let f = Fixture::new(true).await;
    let prepared = f.prepare().await;
    let opener = f.append("task_started", "t1");
    let (mut grant, token) = open(&f, &prepared, opener);
    let terminal = f.append("task_complete", "t1");
    grant.close(terminal).unwrap();
    grant.close(terminal).unwrap();
    let closed = Lifecycle::Closed {
        final_frontier: terminal,
    };
    assert_eq!(grant.lifecycle(), closed);
    grant.unavailable(LiveReadReason::TemporarilyUnavailable);
    assert!(matches!(
        grant.range(opener..terminal),
        LiveReadDecision::Held(LiveReadReason::TemporarilyUnavailable)
    ));
    grant.restore().unwrap();
    assert_eq!(grant.lifecycle(), closed);
    assert_eq!(grant.frontier(), terminal);
    let LiveReadDecision::Granted(first) = grant.range(opener..terminal) else {
        panic!("closed buffered tail must survive")
    };
    let id = first.identity().clone();
    let successor = f.append("task_started", "t2");
    assert_eq!(
        grant.advance(terminal..successor),
        Err(E::FiniteCloseExceeded.into())
    );
    let LiveReadDecision::Granted(buffered) = grant.range(opener..terminal) else {
        panic!("original buffered range remains")
    };
    assert!(Arc::ptr_eq(&id, buffered.identity()));
    token.publish_cancel("cancel buffered output");
    assert!(matches!(
        grant.range(opener..terminal),
        LiveReadDecision::Held(LiveReadReason::Evidence(E::Cancelled))
    ));
    grant.unavailable(LiveReadReason::TemporarilyUnavailable);
    assert_eq!(grant.restore(), Err(E::Cancelled.into()));
    assert_eq!(grant.lifecycle(), closed);
    f.stop().await;
}

#[tokio::test(start_paused = true)]
async fn p5a2_deny_blocks_buffered_emission_and_incomplete_records_hold() {
    let f = Fixture::new(true).await;
    let prepared = f.prepare().await;
    let opener = f.append("task_started", "t1");
    let (mut grant, _) = open(&f, &prepared, opener);
    use std::io::Write;
    let path = &f.source.execution().source.path;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(b"{\"type\":").unwrap();
    let end = file.metadata().unwrap().len();
    assert_eq!(grant.advance(opener..end), Err(E::IncompleteRecord.into()));
    assert_eq!(grant.frontier(), opener);
    grant.deny();
    assert!(matches!(
        grant.range(prepared.floor().checkpoint().1..opener),
        LiveReadDecision::Held(LiveReadReason::Evidence(E::Denied))
    ));
    grant.cancel();
    assert!(matches!(
        grant.range(prepared.floor().checkpoint().1..opener),
        LiveReadDecision::Held(LiveReadReason::Evidence(E::Cancelled))
    ));
    f.stop().await;
}

#[tokio::test(start_paused = true)]
async fn p5a2_receipt_cannot_bind_another_prepared_episode() {
    use crate::services::tui_o::writer::actor::codex_submission::Operation;
    let f = Fixture::new(true).await;
    let mut next = f.launch.clone();
    next.episode.user_message_id += 1;
    next.episode.turn_nonce = "another-turn".into();
    let reply = f
        .port()
        .request(Operation::Capture(Box::new((
            next,
            f.source.execution().clone(),
        ))))
        .await
        .unwrap();
    let prepared = f
        .ready
        .prepared_reply(f.launch.episode.channel_id, reply)
        .unwrap();
    let through = f.append("task_started", "t1");
    let (token, evidence) = evidence(&f);
    let receipt = submit_warm(&evidence, &token, &f.launch.context, || Outcome::Submitted).unwrap();
    let submitted = submitted_episode(&evidence, &token, &f.launch.context, &receipt).unwrap();
    let result = receipt.issue_live(
        &f.ready,
        LiveGrantRequest {
            evidence: &evidence,
            submitted: &submitted,
            prepared: &prepared,
            source: &f.source,
            through,
        },
    );
    assert!(matches!(
        result,
        Err(LiveReadReason::Evidence(E::IdentityMismatch))
    ));
    f.stop().await;
}
