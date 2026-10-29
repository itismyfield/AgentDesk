use super::*;
use crate::services::campaign_execution::{
    control::tests as domain,
    evidence_gate::tests::{OUTPUT, actions, assess, collected, registered, seal},
    profile::TestOutcome,
    verification_run::{Cancellation, NotSentReason, PhaseInitiation},
};

fn actor(attempt: &str, session: &str, native: &str) -> ActorIdentity {
    let mut identity = domain::binding(&domain::running()).session;
    identity.session = session.into();
    ActorIdentity {
        attempt_id: attempt.into(),
        session: identity,
        native_conversation: native.into(),
    }
}

fn implementer() -> ActorIdentity {
    actor("attempt", "worker", "native-work")
}

fn record(id: &str, extent: ReviewExtent, base: &str, head: &str) -> ReviewRecord {
    ReviewRecord {
        evidence_id: id.into(),
        reviewer: actor("review", "reviewer", "native-review"),
        review: ReviewEvidence {
            range: ReviewRange {
                base_sha: base.into(),
                head_sha: head.into(),
                extent,
            },
            verdict: ReviewVerdict::Clean,
            findings: Vec::new(),
            unverified: Vec::new(),
            inherited: Vec::new(),
            webgpt: Vec::new(),
        },
    }
}

fn full() -> ReviewRecord {
    record("full", ReviewExtent::Full, "base", OUTPUT)
}

fn gate(records: &[ReviewRecord]) -> StageAssessment {
    let sealed = seal(&domain::binding(&domain::running()));
    let accepted = BTreeSet::from(["accepted".to_string()]);
    evaluate_review(&sealed, &implementer(), records, &accepted, &["pg".into()])
}

fn review_binding() -> AttemptBinding {
    let mut binding = domain::binding(&domain::running());
    binding.attempt_id = "review".into();
    binding.role = Role::Review;
    binding.stage = Stage::Review;
    binding
}

#[test]
fn reviewer_independence_compares_native_conversation_and_session_not_names() {
    assert_eq!(gate(&[full()]).verdict, Verdict::Pass);
    let dependent = [
        actor("review", "renamed", "native-work"),
        actor("review", "worker", "fresh-native"),
        actor("attempt", "reviewer", "native-review"),
    ];
    for reviewer in dependent {
        let mut same = full();
        same.reviewer = reviewer;
        let assessment = gate(&[same]);
        assert_eq!(assessment.verdict, Verdict::Reject);
        assert_eq!(
            assessment.reasons,
            vec![
                GateReason::NotIndependent("full".into()),
                GateReason::MissingFullReview
            ]
        );
    }
}

#[test]
fn clean_text_alone_never_passes_without_full_chain_and_resolved_findings() {
    let delta = record("delta", ReviewExtent::Delta, "mid", OUTPUT);
    let only_delta = gate(std::slice::from_ref(&delta));
    assert_eq!(only_delta.reasons, vec![GateReason::MissingFullReview]);
    let chained = gate(&[
        record("first", ReviewExtent::Full, "base", "mid"),
        delta.clone(),
    ]);
    assert_eq!(chained.verdict, Verdict::Pass);
    assert_eq!(chained.adopted, vec!["first", "delta"]);
    let gap = record("gap", ReviewExtent::Delta, "elsewhere", OUTPUT);
    let broken = gate(&[record("first", ReviewExtent::Full, "base", "mid"), gap]);
    assert_eq!(
        broken.reasons,
        vec![GateReason::RangeMismatch("first".into())]
    );
    let finding = |open, resolution: Option<&str>| Finding {
        id: "F1".into(),
        open,
        resolution: resolution.map(Into::into),
    };
    let with = |edit: &dyn Fn(&mut ReviewEvidence)| {
        let mut review = full();
        edit(&mut review.review);
        gate(&[review])
    };
    let open = with(&|r| r.findings.push(finding(true, None)));
    assert_eq!(open.verdict, Verdict::QualityFail);
    assert_eq!(open.reasons, vec![GateReason::OpenFinding("F1".into())]);
    let bare = with(&|r| r.findings.push(finding(false, Some(" "))));
    assert_eq!(
        bare.reasons,
        vec![GateReason::UnresolvedFinding("F1".into())]
    );
    // A later delta resolving the finding with evidence clears it.
    let mut first = record("first", ReviewExtent::Full, "base", "mid");
    first.review.findings.push(finding(true, None));
    let mut fix = delta;
    fix.review
        .findings
        .push(finding(false, Some("commit mid..out fixes it")));
    assert_eq!(gate(&[first.clone(), fix]).verdict, Verdict::Pass);
    // A later full review that simply omits the open finding does not clear it.
    let mut dropped = full();
    dropped.evidence_id = "full-2".into();
    assert_eq!(gate(&[first, dropped]).verdict, Verdict::QualityFail);
    let unverified = with(&|r| r.unverified = vec!["pg".into(), "docs".into()]);
    assert_eq!(
        unverified.reasons,
        vec![GateReason::UnverifiedScope("pg".into())]
    );
    let inherited = with(&|r| {
        r.inherited = vec![
            InheritedEvidence {
                evidence_id: "accepted".into(),
                reason: "same seal, unchanged files".into(),
            },
            InheritedEvidence {
                evidence_id: "unknown".into(),
                reason: "trust me".into(),
            },
        ]
    });
    assert_eq!(
        inherited.reasons,
        vec![GateReason::InvalidInheritance("unknown".into())]
    );
    let webgpt = with(&|r| {
        r.webgpt = vec![WebGptResult {
            result_ref: "webgpt-1".into(),
            target_sha: "base".into(),
        }]
    });
    assert_eq!(
        webgpt.reasons,
        vec![GateReason::WebGptMismatch("webgpt-1".into())]
    );
    assert_eq!(
        with(&|r| r.verdict = ReviewVerdict::Issues).verdict,
        Verdict::QualityFail
    );
}

#[test]
fn review_outcome_waits_for_its_exact_prerequisite_and_takes_late_pass_or_fail() {
    let work = domain::binding(&domain::running());
    let sealed = seal(&work);
    let mut pending = registered(&work, 0);
    pending.subject.bind(sealed.bound_subject()).unwrap();
    let resource = crate::services::campaign_execution::adk_resources::resource(
        crate::services::campaign_execution::profile::ResourceKind::Build,
        "mac",
        "token",
    );
    pending.phases[1]
        .initiations
        .push(PhaseInitiation::NotSent {
            reason: NotSentReason::ResourceWait { resource },
            evidence: "token busy".into(),
            observed_at: domain::now(),
        });
    let review = review_binding();
    let prerequisite = StagePrerequisite::bind(&pending, &sealed, &review, 2, 1).unwrap();
    let authority = prerequisite.authority();
    assert_eq!(
        (
            authority.consumer_attempt.as_str(),
            authority.consumer_node_epoch
        ),
        ("review", 2)
    );
    assert_eq!(
        authority.original_binding_digest,
        pending.origin.binding_digest
    );
    assert_eq!(pending.origin.owner_attempt, "attempt");
    let clean = gate(&[full()]);
    let waiting = evaluate_review_stage(clean.clone(), [assess(&pending)]);
    assert_eq!(waiting.verdict, Verdict::Wait);
    let passed = collected(&work, &sealed, &[("t1", TestOutcome::Passed)], Some(0));
    assert_eq!(passed.origin, pending.origin);
    prerequisite.consumes(&review, &passed).unwrap();
    assert_eq!(
        evaluate_review_stage(clean.clone(), [assess(&passed)]).verdict,
        Verdict::Pass
    );
    let failed = collected(&work, &sealed, &[("t1", TestOutcome::Failed)], Some(1));
    assert_eq!(
        evaluate_review_stage(clean, [assess(&failed)]).verdict,
        Verdict::QualityFail
    );
    let mut stranger = review.clone();
    stranger.attempt_id = "review-2".into();
    assert_eq!(
        prerequisite.consumes(&stranger, &passed),
        Err(PrerequisiteError::ConsumerMismatch)
    );
}

type Change = (&'static str, fn(&mut VerificationRun));

#[test]
fn prerequisite_refuses_prior_cycle_same_sha_and_any_changed_identity() {
    let work = domain::binding(&domain::running());
    let sealed = seal(&work);
    let v = registered(&work, 0);
    let review = review_binding();
    // Cycle 0's Work A0 produced the same SHA; its V is neither authority nor evidence for A1.
    let mut previous = work.clone();
    previous.attempt_id = "attempt-cycle-0".into();
    let old = collected(
        &previous,
        &seal(&previous),
        &[("t1", TestOutcome::Passed)],
        Some(0),
    );
    assert_eq!(old.subject.bound().unwrap().output_sha, sealed.output_sha);
    assert_eq!(
        StagePrerequisite::bind(&old, &sealed, &review, 2, 1),
        Err(PrerequisiteError::OriginMismatch("seal".into()))
    );
    let exact = ExecutionRef::of(&v, Some(&sealed)).unwrap();
    let changed: [Change; 6] = [
        ("execution_id", |v| v.origin.execution_id = "v-new".into()),
        ("cycle", |v| v.origin.repair_cycle = 1),
        ("epoch", |v| v.origin.registered_node_epoch = 3),
        ("spec", |v| v.origin.spec_hash = "spec-2".into()),
        ("gate", |v| v.origin.gate.host = "other-host".into()),
        ("phases", |v| v.origin.phase_ids.push("extra".into())),
    ];
    for (field, change) in changed {
        let mut other = v.clone();
        change(&mut other);
        assert_eq!(
            exact.matches(&other),
            Err(PrerequisiteError::OriginMismatch(field.into()))
        );
    }
    let mut other_seal = v.clone();
    other_seal
        .subject
        .bind(seal(&previous).bound_subject())
        .unwrap();
    assert_eq!(
        exact.matches(&other_seal),
        Err(PrerequisiteError::OriginMismatch("subject".into()))
    );
    let mut cancelled = v.clone();
    cancelled.cancellation = Some(Cancellation {
        evidence: "Immediate".into(),
        cancelled_at: domain::now(),
    });
    assert_eq!(
        StagePrerequisite::bind(&cancelled, &sealed, &review, 2, 1),
        Err(PrerequisiteError::Cancelled)
    );
    assert_eq!(
        StagePrerequisite::bind(&v, &sealed, &work, 2, 1),
        Err(PrerequisiteError::ConsumerMismatch)
    );
}

#[test]
fn resume_reuses_collected_replaces_cancelled_and_blocks_unreconciled_runs() {
    let work = domain::binding(&domain::running());
    let sealed = seal(&work);
    let mut state = domain::running();
    let run = state.run.as_mut().unwrap();
    let with_actions = |run: &mut Run, v: &VerificationRun, s: ActionState| {
        run.actions = actions(v, s);
    };
    let done = collected(&work, &sealed, &[("t1", TestOutcome::Passed)], Some(0));
    with_actions(run, &done, ActionState::Reconciled);
    assert_eq!(
        resume_disposition(&done, &sealed, run),
        Ok(ResumeDisposition::Reuse)
    );
    let mut cancelled = registered(&work, 0);
    cancelled.cancellation = Some(Cancellation {
        evidence: "Immediate before test start".into(),
        cancelled_at: domain::now(),
    });
    with_actions(run, &cancelled, ActionState::Cancelled);
    assert_eq!(
        resume_disposition(&cancelled, &sealed, run),
        Ok(ResumeDisposition::Replace)
    );
    with_actions(run, &cancelled, ActionState::Unknown);
    assert_eq!(
        resume_disposition(&cancelled, &sealed, run),
        Ok(ResumeDisposition::Blocked)
    );
    let mut previous = work.clone();
    previous.attempt_id = "attempt-cycle-0".into();
    assert!(resume_disposition(&registered(&previous, 0), &sealed, run).is_err());
}
