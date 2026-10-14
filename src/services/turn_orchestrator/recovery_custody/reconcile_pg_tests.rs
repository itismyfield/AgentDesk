use super::custody_pg_tests::{
    Fx, INCARNATION, NODE, channel, episode, evidence, legacy, live, scoped, seal,
};
use super::model::CanonicalSeal;
use super::reconcile::{
    Action, Evidence, LegacyResult, LiveOwner, NoApplyReason, PriorExecution, ReleaseEvidence,
    ReturnPath, Verdict,
};
use super::*;

const EPOCH: i64 = 41;

fn live_owner(operation_id: &str) -> LiveOwner {
    match live(operation_id) {
        PriorExecution::SameOwnerLive(owner) => owner,
        other => unreachable!("{other:?}"),
    }
}

async fn reconcile(
    fx: &Fx,
    ch: &str,
    revision: i64,
    action: Action,
    evidence: &Evidence,
) -> (Verdict, CustodyRecord) {
    let (verdict, revision) = fx
        .custody
        .reconcile(ch, revision, action, evidence)
        .await
        .expect("reconciled");
    let row = fx.record(ch).await;
    assert_eq!(row.revision, revision);
    (verdict, row)
}

fn is_closed(row: &CustodyRecord) -> bool {
    row.phase == Phase::Closed && row.active_operation.is_none()
}

fn op(row: &CustodyRecord) -> &ActiveOperation {
    row.active_operation.as_ref().expect("operation kept")
}

/// A legacy reservation whose Marker publication was armed and then lost with its process.
async fn marker_lost(fx: &Fx, ch: &str) -> i64 {
    let (revision, id) = fx.reserve(legacy(ch, seal("q1", None, None))).await;
    fx.arm_and_crash(
        ch,
        revision,
        &id,
        Stage::MarkerPublish,
        seal("q1", Some("m1"), None),
        None,
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_u03_reserved_only_and_marker_only_rows_pg() {
    let fx = Fx::create().await;
    let before = seal("q1", None, None);
    // Reserved only, no capability ever issued, stores as before: settle and allow a new one.
    let ch = channel(101);
    let (revision, _) = fx.reserve(legacy(&ch, before.clone())).await;
    let changed = evidence(seal("q2", None, None), PriorExecution::Unknown);
    let (verdict, row) = reconcile(&fx, &ch, revision, Action::RecheckApply, &changed).await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let ch = channel(102);
    let (revision, _) = fx.reserve(legacy(&ch, before.clone())).await;
    let same = evidence(before.clone(), PriorExecution::Unknown);
    let (verdict, row2) = reconcile(&fx, &ch, revision, Action::RecheckApply, &same).await;
    assert_eq!(verdict, Verdict::NoApply);
    assert!(is_closed(&row2));
    fx.reserve(legacy(&ch, before.clone())).await;
    assert_eq!(row.phase, Phase::Held);
    // Armed: matching stores alone never make it unapplied.
    let ch = channel(103);
    let revision = marker_lost(&fx, &ch).await;
    let unknown = evidence(before.clone(), PriorExecution::Unknown);
    let (verdict, row) = reconcile(&fx, &ch, revision, Action::RecheckApply, &unknown).await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let ended = evidence(before.clone(), PriorExecution::Ended);
    let (verdict, row) = reconcile(&fx, &ch, row.revision, Action::RecheckApply, &ended).await;
    assert_eq!(
        verdict,
        Verdict::Resume {
            stage: Stage::MarkerPublish,
            reuse: None
        }
    );
    assert_eq!(row.phase, Phase::LegacyReserved);
    assert_eq!(
        (op(&row).stage, op(&row).status),
        (Stage::MarkerPublish, StageStatus::ReservedNotArmed)
    );
    // Marker published, queue old, execution ended: continue the dequeue or return.
    let marker = seal("q1", Some("m1"), None);
    let ch = channel(104);
    let revision = marker_lost(&fx, &ch).await;
    let running = evidence(marker.clone(), PriorExecution::Unknown);
    let (verdict, row) = reconcile(&fx, &ch, revision, Action::RecheckApply, &running).await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let granted = Evidence {
        followup_granted: true,
        ..evidence(marker.clone(), PriorExecution::Ended)
    };
    let (verdict, row) = reconcile(&fx, &ch, row.revision, Action::RecheckApply, &granted).await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let ended = evidence(marker.clone(), PriorExecution::Ended);
    let (verdict, row) = reconcile(&fx, &ch, row.revision, Action::RecheckApply, &ended).await;
    assert_eq!(
        verdict,
        Verdict::Resume {
            stage: Stage::QueueCommit,
            reuse: None
        }
    );
    assert_eq!(
        (op(&row).stage, op(&row).applied.clone(), row.phase),
        (
            Stage::QueueCommit,
            vec![Stage::MarkerPublish],
            Phase::LegacyReserved
        )
    );
    let ch = channel(105);
    let revision = marker_lost(&fx, &ch).await;
    let (verdict, row) = reconcile(&fx, &ch, revision, Action::CompleteReturn, &ended).await;
    assert_eq!(
        verdict,
        Verdict::Return {
            path: ReturnPath::CleanupOnly,
            confirmed: Some(Stage::MarkerPublish),
        }
    );
    assert_eq!(
        (op(&row).kind, op(&row).stage),
        (OperationKind::ReturnOnly, Stage::SourceReturn)
    );
    fx.close().await;
}

/// A scoped root whose queue exit (debt `debt-1`) was armed and then lost with its process.
async fn exit_lost(fx: &Fx, ch: &str, root: Root) -> i64 {
    let before = seal("q1", None, None);
    let request = match root {
        Root::Scoped => {
            fx.home(ch, NODE, EPOCH).await;
            scoped(ch, before, EPOCH)
        }
        Root::Legacy => legacy(ch, before),
    };
    let (revision, id) = fx.reserve(request).await;
    fx.arm_and_crash(
        ch,
        revision,
        &id,
        Stage::QueueCommit,
        seal("q0", None, None),
        Some("debt-1"),
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_u03_queue_commit_row_reuses_its_debt_pg() {
    let fx = Fx::create().await;
    let committed = seal("q0", None, None);
    let with_debt = |debt: Option<&str>, prior| Evidence {
        debt: debt.map(str::to_string),
        ..evidence(committed.clone(), prior)
    };
    let ch = channel(201);
    let revision = exit_lost(&fx, &ch, Root::Scoped).await;
    let (verdict, row) = reconcile(
        &fx,
        &ch,
        revision,
        Action::RevalidateInstall,
        &with_debt(None, PriorExecution::Ended),
    )
    .await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let (verdict, row) = reconcile(
        &fx,
        &ch,
        row.revision,
        Action::RevalidateInstall,
        &with_debt(Some("debt-1"), PriorExecution::Ended),
    )
    .await;
    assert_eq!(
        verdict,
        Verdict::Resume {
            stage: Stage::ActorInstall,
            reuse: Some("debt-1".into())
        }
    );
    assert_eq!(
        op(&row).handoffs,
        ["debt-1"],
        "the debt is reused, not re-issued"
    );
    assert_eq!(
        (op(&row).applied.clone(), row.phase),
        (vec![Stage::QueueCommit], Phase::Installing)
    );
    // A legacy exit committed by an ended process returns through a rebuilt return.
    let ch = channel(202);
    let revision = exit_lost(&fx, &ch, Root::Legacy).await;
    let (verdict, _) = reconcile(
        &fx,
        &ch,
        revision,
        Action::CompleteReturn,
        &with_debt(None, PriorExecution::Ended),
    )
    .await;
    assert_eq!(
        verdict,
        Verdict::Return {
            path: ReturnPath::DurableReconstruct,
            confirmed: Some(Stage::QueueCommit),
        }
    );
    let ch = channel(203);
    let revision = exit_lost(&fx, &ch, Root::Legacy).await;
    let id = op(&fx.record(&ch).await).operation_id.clone();
    let (verdict, _) = reconcile(
        &fx,
        &ch,
        revision,
        Action::CompleteReturn,
        &with_debt(None, live(&id)),
    )
    .await;
    assert_eq!(
        verdict,
        Verdict::Return {
            path: ReturnPath::LiveLease,
            confirmed: Some(Stage::QueueCommit),
        }
    );
    // Not committed (queue old) by an ended process: re-arm the same exit; neither state holds.
    let ch = channel(204);
    let revision = exit_lost(&fx, &ch, Root::Scoped).await;
    let neither = evidence(seal("q9", None, None), PriorExecution::Ended);
    let (verdict, row) = reconcile(&fx, &ch, revision, Action::RecheckApply, &neither).await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let old = evidence(seal("q1", None, None), PriorExecution::Ended);
    let (verdict, row) = reconcile(&fx, &ch, row.revision, Action::RecheckApply, &old).await;
    assert_eq!(
        verdict,
        Verdict::Resume {
            stage: Stage::QueueCommit,
            reuse: Some("debt-1".into())
        }
    );
    assert_eq!(op(&row).handoffs, ["debt-1"]);
    fx.close().await;
}

/// A scoped root with its exit applied and its actor install armed.
async fn install_armed(fx: &Fx, ch: &str, crash: bool) -> (i64, Option<Armed>) {
    fx.home(ch, NODE, EPOCH).await;
    let (revision, id) = fx.reserve(scoped(ch, seal("q1", None, None), EPOCH)).await;
    let armed = fx
        .arm(
            ch,
            revision,
            &id,
            Stage::QueueCommit,
            seal("q0", None, None),
            Some("debt-1"),
        )
        .await;
    armed.report.send(StepOutcome::Applied {
        after: Box::new(seal("q0", None, None)),
        handoff: None,
    });
    let revision = armed.collector.await.unwrap().unwrap();
    let after = seal("q0", None, Some("episode-1"));
    if crash {
        let revision = fx
            .arm_and_crash(ch, revision, &id, Stage::ActorInstall, after, None)
            .await;
        return (revision, None);
    }
    let armed = fx
        .arm(ch, revision, &id, Stage::ActorInstall, after, None)
        .await;
    (revision + 1, Some(armed))
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_u03_actor_install_row_pg() {
    let fx = Fx::create().await;
    let installed = seal("q0", None, Some("episode-1"));
    // Same process: the actual actor with the issued identity confirms it; an empty slot holds.
    let ch = channel(301);
    let (revision, _running) = install_armed(&fx, &ch, false).await;
    let id = op(&fx.record(&ch).await).operation_id.clone();
    let slot_empty = evidence(installed.clone(), live(&id));
    let (verdict, row) =
        reconcile(&fx, &ch, revision, Action::RevalidateInstall, &slot_empty).await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let actor = Evidence {
        live_actor: Some(episode(None)),
        ..slot_empty.clone()
    };
    let (verdict, row) = reconcile(&fx, &ch, row.revision, Action::RevalidateInstall, &actor).await;
    assert_eq!(verdict, Verdict::Confirmed);
    assert_eq!(
        (op(&row).applied.clone(), row.phase),
        (
            vec![Stage::QueueCommit, Stage::ActorInstall],
            Phase::Installing
        )
    );
    // A new process rebuilds the same issued responsibility from durable admission and debt.
    let ch = channel(302);
    let (revision, _) = install_armed(&fx, &ch, true).await;
    let wrong_debt = Evidence {
        debt: Some("debt-2".into()),
        ..evidence(installed.clone(), PriorExecution::Ended)
    };
    let (verdict, row) =
        reconcile(&fx, &ch, revision, Action::RevalidateInstall, &wrong_debt).await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let restarted = Evidence {
        debt: Some("debt-1".into()),
        ..wrong_debt
    };
    let (verdict, row) = reconcile(
        &fx,
        &ch,
        row.revision,
        Action::RevalidateInstall,
        &restarted,
    )
    .await;
    assert_eq!(
        verdict,
        Verdict::Resume {
            stage: Stage::ActorInstall,
            reuse: Some("debt-1".into())
        }
    );
    assert_eq!(op(&row).handoffs, ["debt-1"]);
    assert_eq!(row.issued_episode, Some(episode(None)));
    // A legacy install after a restart waits for the legacy execution's own result.
    let ch = channel(303);
    let (revision, id) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let revision = fx
        .arm_and_crash(
            &ch,
            revision,
            &id,
            Stage::ActorInstall,
            installed.clone(),
            None,
        )
        .await;
    let (verdict, row) = reconcile(
        &fx,
        &ch,
        revision,
        Action::RevalidateInstall,
        &evidence(installed, PriorExecution::Ended),
    )
    .await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    assert_eq!(row.phase, Phase::Held);
    fx.close().await;
}

/// A legacy root whose Marker was published, then sent back through an owned return that was
/// armed and lost; returns the revision and the operation.
async fn return_lost(fx: &Fx, ch: &str) -> (i64, String) {
    let (revision, id) = fx.reserve(legacy(ch, seal("q1", None, None))).await;
    let revision = fx
        .apply(
            ch,
            revision,
            &id,
            Stage::MarkerPublish,
            seal("q1", Some("m1"), None),
        )
        .await;
    let (verdict, revision) = fx
        .custody
        .record_legacy_result(ch, revision, &id, &LegacyResult::ReturnRequired)
        .await
        .unwrap();
    assert_eq!(
        verdict,
        Verdict::Return {
            path: ReturnPath::LiveLease,
            confirmed: None,
        }
    );
    let revision = fx
        .arm_and_crash(
            ch,
            revision,
            &id,
            Stage::SourceReturn,
            seal("q1-returned", None, None),
            None,
        )
        .await;
    (revision, id)
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_u03_source_return_row_pg() {
    let fx = Fx::create().await;
    type Prior = fn(&str) -> PriorExecution;
    let ended: Prior = |_| PriorExecution::Ended;
    let other_process: Prior = |id| {
        PriorExecution::SameOwnerLive(LiveOwner {
            process_incarnation: "incarnation-0".into(),
            ..live_owner(id)
        })
    };
    let cases: [(CanonicalSeal, Prior, Verdict); 5] = [
        (seal("q1-returned", None, None), ended, Verdict::Close),
        (
            seal("q1-returned", Some("m1"), None),
            ended,
            Verdict::Return {
                path: ReturnPath::CleanupOnly,
                confirmed: Some(Stage::SourceReturn),
            },
        ),
        (
            seal("q1", Some("m1"), None),
            live,
            Verdict::Return {
                path: ReturnPath::LiveLease,
                confirmed: None,
            },
        ),
        (
            seal("q1", Some("m1"), None),
            ended,
            Verdict::Return {
                path: ReturnPath::DurableReconstruct,
                confirmed: None,
            },
        ),
        (
            seal("q1", Some("m1"), None),
            other_process,
            Verdict::Held {
                missing: "the source payload to rebuild the return",
            },
        ),
    ];
    for (n, (observed, prior, expected)) in cases.into_iter().enumerate() {
        let ch = channel(401 + n as u32);
        let (revision, id) = return_lost(&fx, &ch).await;
        let (verdict, row) = reconcile(
            &fx,
            &ch,
            revision,
            Action::CompleteReturn,
            &evidence(observed, prior(&id)),
        )
        .await;
        assert_eq!(verdict, expected, "case {n}");
        assert_eq!(is_closed(&row), expected == Verdict::Close, "case {n}");
    }
    // Without the payload a new process cannot rebuild the return; it never counts as done.
    let ch = channel(410);
    let (revision, _) = return_lost(&fx, &ch).await;
    let (verdict, row) = reconcile(
        &fx,
        &ch,
        revision,
        Action::CompleteReturn,
        &evidence(seal("q1", None, None), PriorExecution::Ended),
    )
    .await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    assert_eq!(row.phase, Phase::Held);
    // A failed return keeps the responsibility.
    let ch = channel(411);
    let (revision, id) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let revision = fx
        .apply(
            &ch,
            revision,
            &id,
            Stage::MarkerPublish,
            seal("q1", Some("m1"), None),
        )
        .await;
    let (_, revision) = fx
        .custody
        .record_legacy_result(&ch, revision, &id, &LegacyResult::ReturnRequired)
        .await
        .unwrap();
    let armed = fx
        .arm(
            &ch,
            revision,
            &id,
            Stage::SourceReturn,
            seal("q1-returned", None, None),
            None,
        )
        .await;
    armed.report.send(StepOutcome::NotApplied);
    let revision = armed.collector.await.unwrap().unwrap();
    let row = fx.record(&ch).await;
    assert_eq!(row.phase, Phase::LegacyReserved);
    assert_eq!(op(&row).kind, OperationKind::ReturnOnly);
    // The failed return is retried by the same operation.
    let returned = seal("q1-returned", None, None);
    fx.apply(&ch, revision, &id, Stage::SourceReturn, returned)
        .await;
    assert!(is_closed(&fx.record(&ch).await));
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_u03_exact_release_row_pg() {
    let fx = Fx::create().await;
    let release = |episode, released, owed_settled| Evidence {
        release: Some(ReleaseEvidence {
            episode,
            released,
            owed_settled,
        }),
        ..evidence(seal("q0", None, None), PriorExecution::Ended)
    };
    let refused_cases = [
        // No actor in the slot, no release result.
        evidence(seal("q0", None, None), PriorExecution::Ended),
        // Only the cancelled bit flipped.
        release(episode(None), false, true),
        // Another episode; `Some("")` is not `None`.
        release(episode(Some("")), true, true),
        // Released but Pending or debt still owed.
        release(episode(None), true, false),
    ];
    let ch = channel(501);
    fx.home(&ch, NODE, EPOCH).await;
    let (revision, id) = fx.reserve(scoped(&ch, seal("q0", None, None), EPOCH)).await;
    let mut revision = fx
        .arm_and_crash(
            &ch,
            revision,
            &id,
            Stage::ExactRelease,
            seal("q0", None, None),
            None,
        )
        .await;
    for (n, case) in refused_cases.iter().enumerate() {
        let (verdict, row) = reconcile(&fx, &ch, revision, Action::ConfirmExactRelease, case).await;
        assert!(
            matches!(verdict, Verdict::Held { .. }),
            "case {n}: {verdict:?}"
        );
        revision = row.revision;
    }
    let (verdict, row) = reconcile(
        &fx,
        &ch,
        revision,
        Action::ConfirmExactRelease,
        &release(episode(None), true, true),
    )
    .await;
    assert_eq!(verdict, Verdict::Close);
    assert!(is_closed(&row));
    assert!(row.ever_scoped);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_u03_legacy_results_end_only_with_their_evidence_pg() {
    let fx = Fx::create().await;
    let cases: [(LegacyResult, Option<Phase>); 12] = [
        (
            LegacyResult::NoApply(NoApplyReason::Empty),
            Some(Phase::Closed),
        ),
        (
            LegacyResult::NoApply(NoApplyReason::StaleSelection),
            Some(Phase::Closed),
        ),
        (
            LegacyResult::LocalClaim {
                bound_to: Some("actor-1".into()),
            },
            Some(Phase::LegacyOwned),
        ),
        (LegacyResult::LocalClaim { bound_to: None }, None),
        (
            LegacyResult::ConsumedToHold {
                hold_id: Some("hold-1".into()),
                handed_off: true,
            },
            Some(Phase::Closed),
        ),
        (
            LegacyResult::ConsumedToHold {
                hold_id: Some("hold-1".into()),
                handed_off: false,
            },
            None,
        ),
        (
            LegacyResult::Forwarded {
                receipt: Some("receipt-1".into()),
            },
            Some(Phase::Closed),
        ),
        (LegacyResult::Forwarded { receipt: None }, None),
        (
            LegacyResult::Duplicate {
                evidence: Some("dup-1".into()),
            },
            Some(Phase::Closed),
        ),
        (LegacyResult::Duplicate { evidence: None }, None),
        (LegacyResult::ReturnRequired, Some(Phase::LegacyReserved)),
        (LegacyResult::Unknown, None),
    ];
    for (n, (result, phase)) in cases.into_iter().enumerate() {
        let ch = channel(601 + n as u32);
        let (revision, id) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
        fx.custody
            .record_legacy_result(&ch, revision, &id, &result)
            .await
            .unwrap();
        let row = fx.record(&ch).await;
        // `None`: the result proves nothing, so the row holds and keeps its operation.
        let expected = phase.unwrap_or(Phase::Held);
        assert_eq!(row.phase, expected, "case {n}: {result:?}");
        assert_eq!(
            row.active_operation.is_some(),
            expected != Phase::Closed,
            "case {n}"
        );
    }
    let owned = fx.record(&channel(603)).await;
    assert_eq!(op(&owned).handoffs, ["actor-1"]);
    // After an applied stage, "nothing applied" cannot be true and holds.
    let ch = channel(620);
    let (revision, id) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let revision = fx
        .apply(
            &ch,
            revision,
            &id,
            Stage::MarkerPublish,
            seal("q1", Some("m1"), None),
        )
        .await;
    let (verdict, _) = fx
        .custody
        .record_legacy_result(
            &ch,
            revision,
            &id,
            &LegacyResult::NoApply(NoApplyReason::TransitionBusy),
        )
        .await
        .unwrap();
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    assert_eq!(fx.record(&ch).await.owner.process_incarnation, INCARNATION);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f1_a_return_keeps_the_effect_reconcile_confirmed_pg() {
    let fx = Fx::create().await;
    // A committed legacy exit turned into a return still answers for the dequeued input.
    let committed = evidence(seal("q0", None, None), PriorExecution::Ended);
    for (n, failed_return) in [false, true].into_iter().enumerate() {
        let ch = channel(701 + n as u32);
        let revision = exit_lost(&fx, &ch, Root::Legacy).await;
        let (verdict, row) =
            reconcile(&fx, &ch, revision, Action::CompleteReturn, &committed).await;
        assert_eq!(
            verdict,
            Verdict::Return {
                path: ReturnPath::DurableReconstruct,
                confirmed: Some(Stage::QueueCommit),
            }
        );
        assert_eq!(op(&row).applied, [Stage::QueueCommit], "case {n}");
        let id = op(&row).operation_id.clone();
        if failed_return {
            let returned = seal("q1-returned", None, None);
            let armed = fx
                .arm(&ch, row.revision, &id, Stage::SourceReturn, returned, None)
                .await;
            armed.report.send(StepOutcome::NotApplied);
            armed.collector.await.unwrap().unwrap();
        } else {
            fx.custody
                .settle_busy(&ch, row.revision, &id)
                .await
                .unwrap();
        }
        let row = fx.record(&ch).await;
        assert_eq!(
            (row.phase, op(&row).kind, op(&row).applied.clone()),
            (
                Phase::LegacyReserved,
                OperationKind::ReturnOnly,
                vec![Stage::QueueCommit]
            ),
            "case {n}"
        );
    }
    // A Marker-only return still owes the exact Marker cleanup.
    let ch = channel(703);
    let revision = marker_lost(&fx, &ch).await;
    let marker = evidence(seal("q1", Some("m1"), None), PriorExecution::Ended);
    let (_, row) = reconcile(&fx, &ch, revision, Action::CompleteReturn, &marker).await;
    assert_eq!(op(&row).applied, [Stage::MarkerPublish]);
    let id = op(&row).operation_id.clone();
    let revision = fx
        .custody
        .settle_busy(&ch, row.revision, &id)
        .await
        .unwrap();
    let armed = fx
        .arm(
            &ch,
            revision,
            &id,
            Stage::SourceReturn,
            seal("q1", None, None),
            None,
        )
        .await;
    armed.report.send(StepOutcome::NotApplied);
    armed.collector.await.unwrap().unwrap();
    let row = fx.record(&ch).await;
    assert_eq!(
        (row.phase, op(&row).applied.clone()),
        (Phase::LegacyReserved, vec![Stage::MarkerPublish])
    );
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f2_a_resumed_stage_starts_from_the_verified_seal_pg() {
    let fx = Fx::create().await;
    let ch = channel(711);
    let marker = seal("q1", Some("m1"), None);
    let revision = marker_lost(&fx, &ch).await;
    let ended = evidence(marker.clone(), PriorExecution::Ended);
    let (_, row) = reconcile(&fx, &ch, revision, Action::RecheckApply, &ended).await;
    assert_eq!(row.canonical_seal, marker);
    let id = op(&row).operation_id.clone();
    let revision = fx
        .arm_and_crash(
            &ch,
            row.revision,
            &id,
            Stage::QueueCommit,
            seal("q0", Some("m1"), None),
            None,
        )
        .await;
    assert_eq!(op(&fx.record(&ch).await).before, marker);
    // Lost again before the queue commit: the stores still read as the verified seal.
    let (verdict, row) = reconcile(&fx, &ch, revision, Action::RecheckApply, &ended).await;
    assert_eq!(
        verdict,
        Verdict::Resume {
            stage: Stage::QueueCommit,
            reuse: None
        }
    );
    assert_eq!(op(&row).applied, [Stage::MarkerPublish]);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f5_new_stages_and_ends_wait_for_the_earlier_execution_pg() {
    let fx = Fx::create().await;
    // A committed scoped exit with its exact debt: no next stage while the earlier one may run.
    let ch = channel(721);
    let mut revision = exit_lost(&fx, &ch, Root::Scoped).await;
    let id = op(&fx.record(&ch).await).operation_id.clone();
    let committed = |prior| Evidence {
        debt: Some("debt-1".into()),
        ..evidence(seal("q0", None, None), prior)
    };
    let not_over = [
        PriorExecution::Unknown,
        PriorExecution::SameOwnerLive(LiveOwner {
            operation_id: "another-operation".into(),
            ..live_owner(&id)
        }),
        PriorExecution::SameOwnerLive(LiveOwner {
            applier_ended: false,
            ..live_owner(&id)
        }),
    ];
    for (n, prior) in not_over.into_iter().enumerate() {
        let (verdict, row) = reconcile(
            &fx,
            &ch,
            revision,
            Action::RevalidateInstall,
            &committed(prior),
        )
        .await;
        assert_eq!(
            verdict,
            Verdict::Held {
                missing: "the end of the earlier execution"
            },
            "case {n}"
        );
        revision = row.revision;
    }
    let installed = seal("q0", None, Some("episode-1"));
    let refused = fx
        .custody
        .arm(&ch, revision, &id, Stage::ActorInstall, installed, None)
        .await
        .map(|_| ());
    assert!(
        matches!(refused, Err(CustodyError::Refused(Refusal::NotArmable))),
        "{refused:?}"
    );
    let (verdict, _) = reconcile(
        &fx,
        &ch,
        revision,
        Action::RevalidateInstall,
        &committed(PriorExecution::Ended),
    )
    .await;
    assert_eq!(
        verdict,
        Verdict::Resume {
            stage: Stage::ActorInstall,
            reuse: Some("debt-1".into())
        }
    );
    // A returned queue with the Marker gone ends the return only once the return is over.
    let ch = channel(722);
    let (revision, _) = return_lost(&fx, &ch).await;
    let returned = seal("q1-returned", None, None);
    let unknown = evidence(returned.clone(), PriorExecution::Unknown);
    let (verdict, row) = reconcile(&fx, &ch, revision, Action::CompleteReturn, &unknown).await;
    assert_eq!(
        verdict,
        Verdict::Held {
            missing: "the end of the earlier execution"
        }
    );
    assert!(!is_closed(&row));
    let over = evidence(returned, PriorExecution::Ended);
    let (verdict, row) = reconcile(&fx, &ch, row.revision, Action::CompleteReturn, &over).await;
    assert_eq!(verdict, Verdict::Close);
    assert!(is_closed(&row));
    // A report arriving after a reconcile held its still-running stage settles that stage.
    let ch = channel(723);
    fx.home(&ch, NODE, EPOCH).await;
    let (revision, id) = fx.reserve(scoped(&ch, seal("q1", None, None), EPOCH)).await;
    let exited = seal("q0", None, None);
    let armed = fx
        .arm(
            &ch,
            revision,
            &id,
            Stage::QueueCommit,
            exited.clone(),
            Some("debt-1"),
        )
        .await;
    let running = Evidence {
        debt: Some("debt-1".into()),
        ..evidence(exited.clone(), PriorExecution::Unknown)
    };
    let (verdict, row) = reconcile(&fx, &ch, revision + 1, Action::RecheckApply, &running).await;
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    assert_eq!(
        (row.phase, op(&row).status),
        (Phase::Held, StageStatus::Indeterminate)
    );
    armed.report.send(StepOutcome::Applied {
        after: Box::new(exited),
        handoff: None,
    });
    let recorded = armed.collector.await.unwrap();
    assert!(recorded.is_ok(), "{recorded:?}");
    let row = fx.record(&ch).await;
    assert_eq!(
        (row.phase, op(&row).status, op(&row).applied.clone()),
        (
            Phase::Installing,
            StageStatus::KnownOutcome,
            vec![Stage::QueueCommit]
        )
    );
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f7_a_held_known_outcome_resolves_through_a_verified_return_pg() {
    let fx = Fx::create().await;
    let ch = channel(731);
    let marker = seal("q1", Some("m1"), None);
    let (revision, id) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let revision = fx
        .apply(&ch, revision, &id, Stage::MarkerPublish, marker.clone())
        .await;
    let busy = LegacyResult::NoApply(NoApplyReason::TransitionBusy);
    let (verdict, mut revision) = fx
        .custody
        .record_legacy_result(&ch, revision, &id, &busy)
        .await
        .unwrap();
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    // Stores changed since the known outcome, or the earlier execution unknown: still held.
    let undecided = [
        evidence(seal("q2", Some("m1"), None), PriorExecution::Ended),
        evidence(marker.clone(), PriorExecution::Unknown),
    ];
    for (n, case) in undecided.iter().enumerate() {
        let result = fx
            .custody
            .reconcile(&ch, revision, Action::CompleteReturn, case)
            .await;
        assert!(
            matches!(result, Ok((Verdict::Held { .. }, _))),
            "case {n}: {result:?}"
        );
        let row = fx.record(&ch).await;
        assert_eq!(
            (row.phase, op(&row).status),
            (Phase::Held, StageStatus::KnownOutcome),
            "case {n}"
        );
        revision = row.revision;
    }
    let verified = evidence(marker, PriorExecution::Ended);
    let result = fx
        .custody
        .reconcile(&ch, revision, Action::CompleteReturn, &verified)
        .await;
    assert!(
        matches!(
            result,
            Ok((
                Verdict::Return {
                    path: ReturnPath::CleanupOnly,
                    confirmed: Some(Stage::MarkerPublish),
                },
                _
            ))
        ),
        "{result:?}"
    );
    let row = fx.record(&ch).await;
    assert_eq!(
        (row.phase, op(&row).kind, op(&row).applied.clone()),
        (
            Phase::LegacyReserved,
            OperationKind::ReturnOnly,
            vec![Stage::MarkerPublish]
        )
    );
    fx.apply(
        &ch,
        row.revision,
        &id,
        Stage::SourceReturn,
        seal("q1", None, None),
    )
    .await;
    assert!(is_closed(&fx.record(&ch).await));
    fx.close().await;
}
