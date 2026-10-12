//! The real loop: a merged receipt deferred by an append-time failure retries as the
//! same merged prefix once the cause is gone, and the next source follows.
use super::*;

fn missing_blob(rig: &Rig, key: u64, ids: Vec<u64>) -> Source {
    let pin = crate::services::tui_input::blob::BlobPin {
        local_path: format!("blobs/att/{key}/0_missing.txt").into(),
        sha256: "0".repeat(64),
        pinned: true,
    };
    let identity = ReceiptIdentity::new(key, ids.clone(), 7, rig.channel, rig.channel).unwrap();
    Source::new(
        key,
        identity,
        json!({"text":"merged","source_message_ids":ids}),
        vec![pin],
    )
    .unwrap()
}

async fn merged_retry(channel: u64, primary: u64) {
    let rig = Rig::new(channel);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let before = wal_seq(&mut driven);
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let failed = commit_merged(&mut driven, missing_blob(&rig, primary, vec![10, 11]), cap).await;
    assert_eq!(failed.receipt, Receipt::Deferred(Deferred::Persistence));
    assert_eq!(
        failed.settlement,
        Ok(()),
        "the first deferral itself settles"
    );
    assert_eq!(
        wal_seq(&mut driven),
        before,
        "the blob check refuses before append"
    );
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let retried = commit_merged(&mut driven, merged(&rig, primary, vec![10, 11]), cap).await;
    assert!(
        matches!(retried.receipt, Receipt::Accepted(_)),
        "{:?}",
        retried.receipt
    );
    assert_eq!(retried.settlement, Ok(()));
    assert_eq!(wal_seq(&mut driven), before + 1);
    let next = commit_source(&mut driven, &rig, 12, retried.capability).await;
    assert!(matches!(next.receipt, Receipt::Accepted(_)));
    assert_eq!(next.settlement, Ok(()));
    let (reply, response) = oneshot::channel();
    let complete = SupervisorCmd::CompleteScan {
        capability: next.capability,
        reply,
    };
    driven.supervisor.input_command(complete, true).await;
    assert!(response.await.unwrap().is_ok());
    assert!(driven.release());
}

#[tokio::test]
async fn g1b_merge_retry_primary_first() {
    merged_retry(6_326_180, 10).await;
}

#[tokio::test]
async fn g1b_merge_retry_primary_nonfirst() {
    merged_retry(6_326_181, 11).await;
}

async fn lookup(driven: &mut Driven, rig: &Rig, alias: u64) -> Responsibility {
    let identity = ReceiptIdentity::new(alias, vec![alias], 7, rig.channel, rig.channel).unwrap();
    let (reply, response) = oneshot::channel();
    let command = SupervisorCmd::LookupResponsibility { identity, reply };
    driven.supervisor.input_command(command, true).await;
    response.await.unwrap()
}

#[tokio::test]
async fn g1b_snapshot_conflict_holds_aliases_and_actor() {
    for (index, conflict) in [false, true].into_iter().enumerate() {
        let rig = Rig::new(6_326_190 + index as u64);
        let a = idle_transcript(&rig, "a", false);
        push(&rig, "n1", source(None, &a));
        let screen = Arc::new(Screen::default());
        let mut supervisor = admitted(&rig, &[]).await;
        let ledger = supervisor.slot().get().unwrap();
        let identity = ReceiptIdentity::new(41, vec![41, 42], 7, rig.channel, rig.channel);
        let mut input = json!({"text":"input 41","source_message_ids":[41,42],"receipt_identity":identity.unwrap()});
        if mutant("c2b_g1b_control_omits_source_ids") {
            input.as_object_mut().unwrap().remove("source_message_ids");
        }
        let received = Entry::Received { key: 41, input };
        let received_seq = ledger.append_entry(&received, &[]).unwrap();
        if conflict {
            // Both identities are complete, but the retained one names 43 instead of 42.
            let mut state = serde_json::to_value(ledger.rows().unwrap()).unwrap();
            let other = ReceiptIdentity::new(41, vec![41, 43], 7, rig.channel, rig.channel);
            state["receipts"] = json!({"41": other.unwrap()});
            ledger.checkpoint(state).unwrap();
            assert!(
                ledger.rows().is_err(),
                "the ledger opens but its rows do not fold"
            );
        }
        let mut driven = Driven::start(supervisor, &screen).await;
        driven.idle(4).await;
        let (aliases, sent) = (
            [
                lookup(&mut driven, &rig, 42).await,
                lookup(&mut driven, &rig, 43).await,
            ],
            driven.sent(),
        );
        if conflict {
            assert_eq!(aliases, [Responsibility::Unknown; 2]);
            assert_eq!(sent, 0, "no actor submits from a conflicting snapshot");
            assert_eq!(rig.gate.mode(), Mode::Held);
        } else {
            let known = Responsibility::Known {
                key: 41,
                received_seq,
            };
            assert_eq!(aliases, [known, Responsibility::Absent]);
            assert_eq!(sent, 1, "the control without the conflict submits");
        }
        assert!(driven.release());
    }
}

#[tokio::test]
async fn g1b_close_rejects_stale_closing_and_ready_cache() {
    for (index, stale) in [false, true].into_iter().enumerate() {
        let rig = Rig::new(6_326_400 + index as u64);
        let screen = Arc::new(Screen::default());
        let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
        let closing = driven.supervisor.registration.closing(&rig.gate).unwrap();
        closing.begin_handback().unwrap();
        if stale {
            closing
                .release_after_handback_with_pending(&driven.supervisor.pending_snapshot())
                .unwrap();
            rig.gate.restore_protection().unwrap();
            let _current = rig.gate.close().unwrap();
        }
        driven.supervisor.gate(&mut driven.drive, false);
        let folds = Arc::new(AtomicUsize::new(0));
        let observed = folds.clone();
        *driven.supervisor.after_close_loan.get_mut().unwrap() = Some(Box::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        let (ack, response) = oneshot::channel();
        driven
            .supervisor
            .input_command(SupervisorCmd::Close { ack }, false)
            .await;
        assert_eq!(
            response.await.unwrap(),
            Err(Deferred::Gate(if stale {
                fence::Failure::StalePermit
            } else {
                fence::Failure::Mode(Mode::Handback)
            })),
            "a healthy ledger cannot certify this Gate"
        );
        assert_eq!(
            folds.load(Ordering::SeqCst),
            0,
            "a Gate refusal precedes the loan"
        );
        assert!(!driven.supervisor.admission_open());
        assert_eq!(driven.sent(), 0);
        assert!(driven.release());
    }
}

#[tokio::test]
async fn g1b_close_rechecks_held_after_loan() {
    let rig = Rig::new(6_326_402);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    driven.supervisor.gate(&mut driven.drive, false);
    let closing = driven.supervisor.registration.closing(&rig.gate).unwrap();
    let snapshot = driven.supervisor.pending_snapshot();
    let gate = rig.gate.clone();
    *driven.supervisor.after_close_loan.get_mut().unwrap() = Some(Box::new(move || {
        closing.begin_handback().unwrap();
        closing
            .release_after_handback_with_pending(&snapshot)
            .unwrap();
        gate.restore_protection().unwrap();
        let _current = gate.close().unwrap();
    }));
    let (ack, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(SupervisorCmd::Close { ack }, false)
        .await;
    assert_eq!(
        response.await.unwrap(),
        Err(Deferred::Gate(fence::Failure::StalePermit)),
        "loan completion must revalidate the Gate"
    );
    assert!(driven.supervisor.closed);
    assert!(!driven.supervisor.admission_open());
    assert!(driven.release());
}

#[tokio::test]
async fn g1b_close_waits_for_loan_and_repeats_current_held() {
    let rig = Rig::new(6_326_405);
    let mut supervisor = admitted(&rig, &[]).await;
    let screen = Arc::new(Screen::default());
    let (entered, inside) = oneshot::channel();
    let (resume, latch) = mpsc::channel();
    *supervisor.after_close_loan.get_mut().unwrap() = Some(Box::new(move || {
        entered.send(()).unwrap();
        latch.recv_timeout(Duration::from_secs(10)).unwrap();
    }));
    let (commands, receiver) = tokio::sync::mpsc::channel(4);
    let script = async {
        let (ack, mut response) = oneshot::channel();
        commands.send(SupervisorCmd::Close { ack }).await.unwrap();
        inside.await.unwrap();
        assert_eq!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        assert_eq!(rig.gate.mode(), Mode::Held);
        let source = super::super::super::super::external::source(
            rig.channel,
            7,
            "new while Close is folding",
            None,
            "closed-new",
        )
        .unwrap();
        let (reply, mut submitted) = oneshot::channel();
        commands
            .send(SupervisorCmd::SubmitExternal {
                source: Box::new(source),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            submitted.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        resume.send(()).unwrap();
        response.await.unwrap().unwrap();
        assert_eq!(
            submitted.await.unwrap(),
            super::super::super::super::external::ExternalReceipt::Deferred(Deferred::Closed)
        );
        let (ack, response) = oneshot::channel();
        commands.send(SupervisorCmd::Close { ack }).await.unwrap();
        response.await.unwrap().unwrap();
        drop(commands);
    };
    tokio::join!(supervisor.run(DriveFake(screen.clone()), receiver), script);
    assert_eq!(screen.sent.lock().unwrap().len(), 0);
    assert_eq!(
        supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .open_rows()
            .count(),
        0
    );
    assert!(supervisor.release());
}

#[tokio::test]
async fn g1b_close_bound_and_reply_loss_do_not_cancel_loan() {
    let rig = Rig::new(6_326_406);
    let mut supervisor = admitted(&rig, &[]).await;
    let finished = Arc::new(AtomicUsize::new(0));
    let observed = finished.clone();
    *supervisor.after_close_loan.get_mut().unwrap() = Some(Box::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    let (ack, response) = oneshot::channel();
    drop(response);
    supervisor
        .input_command(SupervisorCmd::Close { ack }, false)
        .await;
    assert_eq!(finished.load(Ordering::SeqCst), 1);
    assert!(!supervisor.slot.loaned());
    assert!(supervisor.closed);
    assert!(supervisor.release());
}

#[tokio::test]
async fn g1b_failed_durable_handback_never_reaches_release() {
    for (index, broken) in [false, true].into_iter().enumerate() {
        let rig = Rig::new(6_326_407 + index as u64);
        let mut supervisor = admitted(&rig, &[]).await;
        let source = input(&rig, 41);
        let (entry, pins) = source.into_entry();
        supervisor
            .slot()
            .get()
            .unwrap()
            .append_entry(&entry, &pins)
            .unwrap();
        if broken {
            rig.break_ledger(true);
            let mut lease = supervisor.slot.lend().unwrap();
            lease.needs_reopen = true;
            supervisor.slot.restore(lease);
        }
        let closing = supervisor.registration.closing(&rig.gate).unwrap();
        let landing = supervisor.handback(&closing).await;
        let mut releases = 0;
        if landing == Ok(Landing::HandedBack) {
            releases += 1;
            closing
                .release_after_handback_with_pending(&supervisor.pending_snapshot())
                .unwrap();
        }
        assert_eq!(releases, usize::from(!broken));
        assert_eq!(
            rig.world.enqueued.lock().unwrap().len(),
            usize::from(!broken)
        );
        assert_eq!(
            fence::lookup(&ProviderKind::Claude, rig.channel).is_some(),
            broken
        );
        assert!(supervisor.release());
    }
}

#[tokio::test]
async fn g1b_exhausted_admission_cannot_reopen_on_boot() {
    for (index, admission) in [true, false].into_iter().enumerate() {
        let rig = Rig::new(6_326_403 + index as u64);
        let screen = Arc::new(Screen::default());
        let mut supervisor = admitted(&rig, &[]).await;
        let ticket = supervisor
            .order
            .fetch_ticket(supervisor.admission_gen)
            .unwrap();
        let paused = supervisor
            .order
            .begin_scan(ticket, supervisor.admission_gen, vec![11], 11, true)
            .unwrap();
        if admission {
            supervisor.admission_gen = u64::MAX - 2;
            supervisor.invalidate_order();
            assert_eq!(supervisor.admission_gen, u64::MAX - 1);
            let (mut drive, _) = supervisor.start_drive(DriveFake(screen.clone())).unwrap();
            supervisor.gate(&mut drive, true);
            assert_eq!(
                rig.gate.mode(),
                Mode::Frozen,
                "exhaustion precedes ledger opening"
            );
            assert!(
                !supervisor
                    .receipt_admission(&drive, Some(Mode::Frozen))
                    .receipt_open
            );
        } else {
            super::super::super::super::ordering::g1b_tests::exhaust_dirty(&mut supervisor.order);
            assert!(supervisor.order.admit_pending(&[11]).is_err());
        }
        let before = supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq();
        let stages = rig.world.log();
        for _ in 0..2 {
            let landing = supervisor.boot().await;
            assert_eq!(rig.world.log(), stages, "exhausted boot runs no stage");
            assert!(matches!(landing, Landing::Held(_)));
            assert!(!supervisor.admission_open());
            assert!(supervisor.start_drive(DriveFake(screen.clone())).is_none());
        }
        let (reply, response) = oneshot::channel();
        supervisor
            .input_command(
                SupervisorCmd::CommitFromScan {
                    source: Box::new(input(&rig, 11)),
                    capability: paused,
                    reply,
                },
                true,
            )
            .await;
        assert_eq!(
            response.await.unwrap().receipt,
            Receipt::Deferred(Deferred::Closed)
        );
        assert_eq!(
            external_receipt(&mut supervisor, &rig, "exhausted-new").await,
            super::super::super::super::external::ExternalReceipt::Deferred(Deferred::Closed)
        );
        assert_eq!(
            supervisor
                .slot()
                .get()
                .unwrap()
                .rows()
                .unwrap()
                .folded_seq(),
            before
        );
        assert_eq!(screen.sent.lock().unwrap().len(), 0);
        assert!(supervisor.release());
    }
}

#[tokio::test]
async fn g1b_running_order_exhaustion_closes_submit_without_boot() {
    let rig = Rig::new(6_326_430);
    let screen = Arc::new(Screen::default());
    let mut supervisor = admitted(&rig, &[]).await;
    let (mut drive, _) = supervisor.start_drive(DriveFake(screen.clone())).unwrap();
    supervisor.gate(&mut drive, true);
    assert!(supervisor.admission_open());
    assert!(
        supervisor
            .receipt_admission(&drive, Some(rig.gate.mode()))
            .receipt_open
    );
    let before = supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq();
    let stages = rig.world.log();
    let generation = supervisor.admission_gen;

    // Keep the running admission state so only dirty exhaustion can close it.
    super::super::super::super::ordering::g1b_tests::exhaust_dirty(&mut supervisor.order);
    assert!(supervisor.order.admit_pending(&[11]).is_err());
    assert!(supervisor.order.exhausted());
    assert!(supervisor.admitted);
    assert!(!supervisor.closed);
    assert!(!supervisor.admission_open());
    assert!(supervisor.start_drive(DriveFake(screen.clone())).is_none());
    assert!(
        !supervisor
            .receipt_admission(&drive, Some(rig.gate.mode()))
            .receipt_open
    );
    assert_eq!(
        external_receipt(&mut supervisor, &rig, "running-exhausted-new").await,
        super::super::super::super::external::ExternalReceipt::Deferred(Deferred::Closed)
    );
    assert_eq!(
        supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq(),
        before
    );
    assert!(supervisor.admitted);
    assert!(!supervisor.closed);
    assert_eq!(supervisor.admission_gen, generation);
    assert_eq!(rig.world.log(), stages);
    assert_eq!(screen.sent.lock().unwrap().len(), 0);
    drop(drive);
    assert!(supervisor.release());
}

async fn external_receipt(
    supervisor: &mut Supervisor<Fake>,
    rig: &Rig,
    origin: &str,
) -> super::super::super::super::external::ExternalReceipt {
    let source = super::super::super::super::external::source(
        rig.channel,
        7,
        "external input",
        None,
        origin,
    )
    .unwrap();
    let (reply, response) = oneshot::channel();
    supervisor
        .input_command(
            SupervisorCmd::SubmitExternal {
                source: Box::new(source),
                reply,
            },
            true,
        )
        .await;
    response.await.unwrap()
}

#[tokio::test]
async fn g1b_integrated_mapping_close_and_exhaustion_regression() {
    use super::super::super::super::external::ExternalReceipt;
    for (row, (mapping, close_case)) in ["normal", "present", "unavailable"]
        .into_iter()
        .flat_map(|mapping| ["normal", "repeat", "gate", "flush"].map(|case| (mapping, case)))
        .enumerate()
    {
        let rig = Rig::new(6_326_410 + row as u64);
        let screen = Arc::new(Screen::default());
        let mut supervisor = admitted(&rig, &[]).await;
        let first = external_receipt(&mut supervisor, &rig, "existing").await;
        let receipt = match first {
            ExternalReceipt::Received {
                receipt,
                duplicate: false,
                ..
            } => receipt,
            other => panic!("positive external control: {other:?}"),
        };
        let mapping_health = if mapping != "normal" {
            if mapping == "present" {
                rig.world
                    .parents()
                    .insert(ChannelId::new(rig.channel), ChannelId::new(8));
            } else {
                rig.world.mapping_unavailable.store(true, Ordering::SeqCst);
            }
            let Landing::Held(cause) = supervisor.boot().await else {
                panic!("mapping {mapping} must hold boot");
            };
            assert!(supervisor.start_drive(DriveFake(screen.clone())).is_none());
            Some(cause.health("claude", rig.channel))
        } else {
            None
        };
        let closing = supervisor.registration.closing(&rig.gate).unwrap();
        if close_case == "gate" {
            closing.begin_handback().unwrap();
        } else if close_case == "flush" {
            rig.break_ledger(true);
            let mut lease = supervisor.slot.lend().unwrap();
            lease.needs_reopen = true;
            supervisor.slot.restore(lease);
        }
        let folds = Arc::new(AtomicUsize::new(0));
        for iteration in 0..if close_case == "repeat" { 2 } else { 1 } {
            let observed = folds.clone();
            *supervisor.after_close_loan.get_mut().unwrap() = Some(Box::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }));
            let (ack, response) = oneshot::channel();
            supervisor
                .input_command(SupervisorCmd::Close { ack }, false)
                .await;
            let result = response.await.unwrap();
            match close_case {
                "gate" => assert_eq!(
                    result,
                    Err(Deferred::Gate(fence::Failure::Mode(Mode::Handback))),
                    "mapping={mapping} close={close_case}"
                ),
                "flush" => assert_eq!(
                    result,
                    Err(Deferred::Persistence),
                    "mapping={mapping} close={close_case}"
                ),
                _ => assert!(result.is_ok(), "mapping={mapping} close={close_case}"),
            }
            assert!(supervisor.closed);
            let health = rig.health();
            assert_eq!(
                health
                    .iter()
                    .any(|line| line.contains("ledger_close_gate_unconfirmed")),
                close_case == "gate"
            );
            assert_eq!(
                health
                    .iter()
                    .any(|line| line.contains("ledger_close_flush_unconfirmed")),
                close_case == "flush"
            );
            assert!(!supervisor.admission_open());
            assert_eq!(
                folds.load(Ordering::SeqCst),
                if close_case == "gate" {
                    0
                } else {
                    iteration + 1
                },
                "Close validates Gate before this slot's fold"
            );
            assert_eq!(
                rig.gate.mode(),
                if close_case == "gate" {
                    Mode::Handback
                } else {
                    Mode::Held
                }
            );
            assert!(fence::lookup(&ProviderKind::Claude, rig.channel).is_some());
        }
        let closed_stages = rig.world.log();
        let closed_boot = supervisor.boot().await;
        assert_eq!(
            rig.world.log(),
            closed_stages,
            "Close ACK keeps boot closed"
        );
        assert!(matches!(closed_boot, Landing::Held(_)));
        if close_case == "flush" {
            rig.break_ledger(false);
            let mut lease = supervisor.slot.lend().unwrap();
            lease.needs_reopen = true;
            supervisor.slot.restore(lease);
        }
        let before = supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq();
        supervisor.admission_gen = u64::MAX - 1;
        supervisor.invalidate_order();
        let stages = rig.world.log();
        for _ in 0..2 {
            let landing = supervisor.boot().await;
            assert_eq!(rig.world.log(), stages, "exhausted boot runs no stage");
            assert!(matches!(landing, Landing::Held(_)));
            assert!(supervisor.start_drive(DriveFake(screen.clone())).is_none());
        }
        assert!(
            matches!(external_receipt(&mut supervisor, &rig, "existing").await,
            ExternalReceipt::Received { receipt: saved, duplicate: true, .. } if saved == receipt)
        );
        assert_eq!(
            external_receipt(&mut supervisor, &rig, "new").await,
            ExternalReceipt::Deferred(Deferred::Closed)
        );
        assert_eq!(
            supervisor
                .slot()
                .get()
                .unwrap()
                .rows()
                .unwrap()
                .folded_seq(),
            before
        );
        assert_eq!(screen.sent.lock().unwrap().len(), 0);
        assert_eq!(
            supervisor.registration.latched().is_some(),
            mapping == "present"
        );
        if let Some(line) = mapping_health {
            assert!(
                rig.health().contains(&line),
                "Close and exhaustion keep the mapping hold"
            );
        }
        assert!(supervisor.release());
    }
}

#[tokio::test]
async fn g1b_unstarted_mapping_drive_never_synthesizes_close_ack() {
    for (index, present) in [true, false].into_iter().enumerate() {
        let rig = Rig::new(6_326_425 + index as u64);
        if present {
            rig.world
                .parents()
                .insert(ChannelId::new(rig.channel), ChannelId::new(8));
        } else {
            rig.world.mapping_unavailable.store(true, Ordering::SeqCst);
        }
        let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
        assert!(matches!(supervisor.boot().await, Landing::Held(_)));
        let before = rig.registry.health_reasons();
        let folded = Arc::new(AtomicUsize::new(0));
        let observed = folded.clone();
        *supervisor.after_close_loan.get_mut().unwrap() = Some(Box::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        let (commands, receiver) = tokio::sync::mpsc::channel(1);
        let (ack, response) = oneshot::channel();
        commands.try_send(SupervisorCmd::Close { ack }).unwrap();
        let screen = Arc::new(Screen::default());
        supervisor.run(DriveFake(screen.clone()), receiver).await;
        assert!(
            response.await.is_err(),
            "an unstarted drive cannot process Close"
        );
        assert_eq!(folded.load(Ordering::SeqCst), 0);
        assert!(!supervisor.admission_open());
        assert_eq!(rig.registry.health_reasons(), before);
        assert_eq!(screen.sent.lock().unwrap().len(), 0);
        assert!(fence::lookup(&ProviderKind::Claude, rig.channel).is_some());
        assert!(supervisor.release());
    }
}
