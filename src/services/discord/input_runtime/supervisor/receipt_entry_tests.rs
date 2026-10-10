use super::*;
use crate::services::discord::input_runtime::supervisor::command::ScanCommit;
use crate::services::discord::input_runtime::supervisor::receipt::{Deferred, Receipt};
use crate::services::discord::input_runtime::supervisor::source::Source;
use crate::services::tui_input::rows::receipt_identity::{ReceiptIdentity, Responsibility};
use tokio::sync::oneshot;

fn input(rig: &Rig, key: u64) -> Source {
    Source::new(
        key,
        ReceiptIdentity::new(key, vec![key], 7, rig.channel, rig.channel).unwrap(),
        json!({"text":format!("message {key}")}),
        vec![],
    )
    .unwrap()
}

#[tokio::test]
async fn g1a_real_loop_receipts_while_running_or_draft_held_and_finishes_after_receiver_drop() {
    for (index, binding) in ["running", "draft", "pending", "absent"]
        .into_iter()
        .enumerate()
    {
        let running = binding == "running";
        let rig = Rig::new(6_325_931 + index as u64);
        let a = if running {
            transcript(&rig, "a", false)
        } else {
            idle_transcript(&rig, "a", false)
        };
        if running {
            user(&a.path, "prior human input");
        }
        match binding {
            "pending" => {
                push(&rig, "n1", pending("next", &a.path));
            }
            "absent" => {}
            _ => {
                push(&rig, "n1", source(None, &a));
            }
        }
        let screen = Arc::new(Screen::default());
        screen.draft.store(binding == "draft", Ordering::SeqCst);
        let mut supervisor = admitted(&rig, &[]).await;
        let (commands, received) = tokio::sync::mpsc::channel(4);
        let script = async {
            let (reply, response) = oneshot::channel();
            commands
                .send(SupervisorCmd::PendingSource {
                    sources: vec![10],
                    reply,
                })
                .await
                .unwrap();
            assert_eq!(response.await.unwrap().unwrap().sources, [10]);
            let (reply, response) = oneshot::channel();
            commands
                .send(SupervisorCmd::FetchTicket { reply })
                .await
                .unwrap();
            let ticket = response.await.unwrap().unwrap();
            let (reply, response) = oneshot::channel();
            commands
                .send(SupervisorCmd::BeginScan {
                    ticket,
                    sources: vec![10],
                    horizon: 10,
                    complete_fetch: true,
                    reply,
                })
                .await
                .unwrap();
            let capability = response.await.unwrap().unwrap();
            let identity = input(&rig, 10).identity().clone();
            let (reply, response) = oneshot::channel();
            commands
                .send(SupervisorCmd::CommitFromScan {
                    source: Box::new(input(&rig, 10)),
                    capability,
                    reply,
                })
                .await
                .unwrap();
            drop(response);
            let (reply, response) = oneshot::channel();
            commands
                .send(SupervisorCmd::LookupResponsibility { identity, reply })
                .await
                .unwrap();
            assert!(matches!(
                response.await.unwrap(),
                Responsibility::Known { key: 10, .. }
            ));
            assert_eq!(screen.sent.lock().unwrap().len(), 0);
            drop(commands);
        };
        tokio::join!(supervisor.run(DriveFake(screen.clone()), received), script);
        let restored = supervisor.slot().reopen().unwrap().rows().unwrap();
        assert_eq!(restored.row(10).unwrap().state, RowState::Received);
        assert_eq!(restored.open_rows().count(), 1);
        assert_eq!(screen.sent.lock().unwrap().len(), 0);
        assert!(supervisor.release());
    }
}

async fn scan(
    driven: &mut Driven,
    key: u64,
) -> crate::services::discord::input_runtime::supervisor::ordering::ScanCapability {
    let ticket = fetch_ticket(driven).await;
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
                ticket,
                sources: vec![key],
                horizon: key,
                complete_fetch: true,
                reply,
            },
            true,
        )
        .await;
    response.await.unwrap().unwrap()
}

async fn commit_source(
    driven: &mut Driven,
    rig: &Rig,
    key: u64,
    capability: crate::services::discord::input_runtime::supervisor::ordering::ScanCapability,
) -> ScanCommit {
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CommitFromScan {
                source: Box::new(input(rig, key)),
                capability,
                reply,
            },
            true,
        )
        .await;
    response.await.unwrap()
}

#[tokio::test]
async fn g1a_clear_and_binding_boundaries_reject_paused_scan_receipts_without_a_wal_append() {
    let rig = Rig::new(6_325_903);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.draft.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let before = driven
        .supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq();
    let paused = scan(&mut driven, 10).await;
    let at = driven.at;
    driven.supervisor.clear(&mut driven.drive, at).await;
    assert_eq!(
        commit_source(&mut driven, &rig, 10, paused).await.receipt,
        Receipt::Deferred(Deferred::Order)
    );
    let paused = scan(&mut driven, 10).await;
    push(&rig, "n1", source(Some(&a), &a));
    driven.wake().await;
    assert_eq!(
        commit_source(&mut driven, &rig, 10, paused).await.receipt,
        Receipt::Deferred(Deferred::Order)
    );
    let paused = scan(&mut driven, 10).await;
    assert!(matches!(
        commit_source(&mut driven, &rig, 10, paused).await.receipt,
        Receipt::Accepted(_)
    ));
    assert_eq!(
        driven
            .supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq(),
        before + 1
    );
    assert_eq!(driven.sent(), 0);
    assert!(driven.release());
}

#[tokio::test]
async fn g1a_scan_commands_cannot_commit_a_younger_source_over_an_unsettled_head() {
    let rig = Rig::new(6_325_904);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let ticket = fetch_ticket(&mut driven).await;
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
                ticket,
                sources: vec![10, 11],
                horizon: 11,
                complete_fetch: true,
                reply,
            },
            true,
        )
        .await;
    let capability = response.await.unwrap().unwrap();
    let result = commit_source(&mut driven, &rig, 11, capability).await;
    assert_eq!(result.receipt, Receipt::Deferred(Deferred::Order));
    assert!(
        driven
            .supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .row(11)
            .is_none()
    );
    let result = commit_source(&mut driven, &rig, 10, result.capability).await;
    assert!(matches!(result.receipt, Receipt::Accepted(_)));
    let result = commit_source(&mut driven, &rig, 11, result.capability).await;
    assert!(matches!(result.receipt, Receipt::Accepted(_)));
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CompleteScan {
                capability: result.capability,
                reply,
            },
            true,
        )
        .await;
    assert!(response.await.unwrap().is_ok());
    let rows = driven.supervisor.slot().reopen().unwrap().rows().unwrap();
    assert!(rows.row(10).unwrap().received_seq < rows.row(11).unwrap().received_seq);
    assert_eq!(driven.sent(), 0);
    assert!(driven.release());
}

async fn fetch_ticket(driven: &mut Driven) -> super::super::super::ordering::FetchTicket {
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(SupervisorCmd::FetchTicket { reply }, true)
        .await;
    response.await.unwrap().unwrap()
}

async fn scan_page(
    driven: &mut Driven,
    ids: Vec<u64>,
    horizon: u64,
) -> super::super::super::ordering::ScanCapability {
    let ticket = fetch_ticket(driven).await;
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
                ticket,
                sources: ids,
                horizon,
                complete_fetch: true,
                reply,
            },
            true,
        )
        .await;
    response.await.unwrap().unwrap()
}

fn merged(rig: &Rig, key: u64, ids: Vec<u64>) -> Source {
    Source::new(
        key,
        ReceiptIdentity::new(key, ids.clone(), 7, rig.channel, rig.channel).unwrap(),
        json!({"text":"merged", "source_message_ids":ids}),
        vec![],
    )
    .unwrap()
}

fn wal_seq(driven: &mut Driven) -> u64 {
    driven
        .supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq()
}

async fn commit_merged(
    driven: &mut Driven,
    source: Source,
    capability: super::super::super::ordering::ScanCapability,
) -> ScanCommit {
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CommitFromScan {
                source: Box::new(source),
                capability,
                reply,
            },
            true,
        )
        .await;
    response.await.unwrap()
}

#[tokio::test]
async fn g1a_r1a_full_source_prefix_before_append() {
    for (index, (page, horizon, key, ids)) in [
        (vec![10, 11, 12], 12, 10, vec![10, 12]),
        (vec![10], 10, 10, vec![10, 12]),
        (vec![11, 12], 12, 11, vec![10, 11]),
    ]
    .into_iter()
    .enumerate()
    {
        let rig = Rig::new(6_325_910 + index as u64);
        let screen = Arc::new(Screen::default());
        let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
        if key == 11 {
            let cap = scan(&mut driven, 10).await;
            assert!(matches!(
                commit_source(&mut driven, &rig, 10, cap).await.receipt,
                Receipt::Accepted(_)
            ));
        }
        let cap = scan_page(&mut driven, page, horizon).await;
        let before = wal_seq(&mut driven);
        let result = commit_merged(&mut driven, merged(&rig, key, ids), cap).await;
        assert_eq!(result.receipt, Receipt::Deferred(Deferred::Order));
        assert_eq!(wal_seq(&mut driven), before);
        assert!(driven.release());
    }
}

#[tokio::test]
async fn g1a_r1a_contiguous_merge_settles_every_source_and_alias_duplicate_keeps_canonical() {
    let rig = Rig::new(6_325_913);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let before = wal_seq(&mut driven);
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let result = commit_merged(&mut driven, merged(&rig, 10, vec![10, 11]), cap).await;
    assert!(matches!(result.receipt, Receipt::Accepted(_)));
    assert!(matches!(
        commit_source(&mut driven, &rig, 12, result.capability)
            .await
            .receipt,
        Receipt::Accepted(_)
    ));
    assert_eq!(wal_seq(&mut driven), before + 2);
    assert!(driven.release());
    drop(rig);

    let rig = Rig::new(6_325_914);
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let source = merged(&rig, 10, vec![10, 11]);
    let receipt = loan(&mut driven.supervisor.slot, move |lease| {
        super::super::super::receipt::commit(lease, source, true)
    })
    .await
    .unwrap();
    assert!(matches!(receipt, Receipt::Accepted(_)));
    let saved = driven
        .supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .row(10)
        .unwrap()
        .input
        .clone();
    let before = wal_seq(&mut driven);
    let cap = scan_page(&mut driven, vec![11], 11).await;
    assert!(matches!(
        commit_source(&mut driven, &rig, 11, cap).await.receipt,
        Receipt::DuplicateQueued(_)
    ));
    assert_eq!(wal_seq(&mut driven), before);
    assert_eq!(
        driven
            .supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .row(10)
            .unwrap()
            .input,
        saved
    );
    assert!(driven.release());
}

#[tokio::test]
async fn g1a_r1b_prefetch_ticket_rejects_clear_binding_and_epoch_changes_and_allows_fresh_retry() {
    for (index, boundary) in ["clear", "binding", "epoch"].into_iter().enumerate() {
        let rig = Rig::new(6_325_920 + index as u64);
        let a = idle_transcript(&rig, "a", false);
        push(&rig, "n1", source(None, &a));
        let screen = Arc::new(Screen::default());
        screen.draft.store(true, Ordering::SeqCst);
        let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
        let ticket = fetch_ticket(&mut driven).await;
        let (resume, paused) = oneshot::channel();
        let fetch = tokio::spawn(async move {
            paused.await.unwrap();
            ticket
        });
        let before = wal_seq(&mut driven);
        match boundary {
            "clear" => {
                let at = driven.at;
                driven.supervisor.clear(&mut driven.drive, at).await;
            }
            "binding" => {
                push(&rig, "n1", source(Some(&a), &a));
                driven.wake().await;
            }
            _ => driven.supervisor.invalidate_order(),
        }
        resume.send(()).unwrap();
        let ticket = fetch.await.unwrap();
        let (reply, response) = oneshot::channel();
        driven
            .supervisor
            .input_command(
                SupervisorCmd::BeginScan {
                    ticket,
                    sources: vec![10],
                    horizon: 10,
                    complete_fetch: true,
                    reply,
                },
                true,
            )
            .await;
        assert!(response.await.unwrap().is_err());
        assert_eq!(wal_seq(&mut driven), before);
        let cap = scan(&mut driven, 10).await;
        assert!(matches!(
            commit_source(&mut driven, &rig, 10, cap).await.receipt,
            Receipt::Accepted(_)
        ));
        assert!(driven.release());
    }
}

#[tokio::test]
async fn g1a_hold_open_cycle_and_unsettled_clear_close_receipts() {
    let rig = Rig::new(6_325_923);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let before = wal_seq(&mut driven);
    let cap = scan(&mut driven, 10).await;
    driven.supervisor.gate(&mut driven.drive, false);
    driven.supervisor.gate(&mut driven.drive, true);
    assert_eq!(
        commit_source(&mut driven, &rig, 10, cap).await.receipt,
        Receipt::Deferred(Deferred::Order)
    );
    let cap = scan(&mut driven, 10).await;
    driven.drive.clearing = Some(Default::default());
    let admission = driven
        .supervisor
        .receipt_admission(&driven.drive, Some(Mode::LedgerOpen));
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CommitFromScan {
                source: Box::new(input(&rig, 10)),
                capability: cap,
                reply,
            },
            admission.receipt_open,
        )
        .await;
    assert_eq!(
        response.await.unwrap().receipt,
        Receipt::Deferred(Deferred::Closed)
    );
    assert_eq!(wal_seq(&mut driven), before);
    assert!(driven.release());
}

#[tokio::test]
async fn g1a_close_ack_follows_synced_commit_and_blocks_later_commit() {
    let rig = Rig::new(6_325_924);
    let screen = Arc::new(Screen::default());
    let mut supervisor = admitted(&rig, &[]).await;
    let before = supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq();
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let script = async {
        let ticket = super::super::super::command::request(&sender, |reply| {
            SupervisorCmd::FetchTicket { reply }
        })
        .await
        .unwrap()
        .unwrap();
        let capability =
            super::super::super::command::request(&sender, |reply| SupervisorCmd::BeginScan {
                ticket,
                sources: vec![10, 11],
                horizon: 11,
                complete_fetch: true,
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        let (reply, mut receipt) = oneshot::channel();
        sender
            .send(SupervisorCmd::CommitFromScan {
                source: Box::new(input(&rig, 10)),
                capability,
                reply,
            })
            .await
            .unwrap();
        let (ack, closed) = oneshot::channel();
        sender.send(SupervisorCmd::Close { ack }).await.unwrap();
        closed.await.unwrap().unwrap();
        assert_eq!(rig.gate.mode(), Mode::Held);
        let result = receipt.try_recv().expect("receipt ACK before Close ACK");
        assert!(matches!(result.receipt, Receipt::Accepted(_)));
        let result =
            super::super::super::command::request(&sender, |reply| SupervisorCmd::CommitFromScan {
                source: Box::new(input(&rig, 11)),
                capability: result.capability,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(result.receipt, Receipt::Deferred(Deferred::Closed));
        drop(sender);
    };
    tokio::join!(supervisor.run(DriveFake(screen), receiver), script);
    assert_eq!(
        supervisor
            .slot()
            .reopen()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq(),
        before + 1
    );
    assert!(supervisor.release());
}

#[tokio::test]
async fn g1a_old_snapshot_missing_identity_is_held_with_health_and_new_source_wal_zero() {
    let rig = Rig::new(6_325_925);
    let screen = Arc::new(Screen::default());
    let mut supervisor = admitted(&rig, &[]).await;
    supervisor
        .slot()
        .get()
        .unwrap()
        .append_entry(
            &Entry::Received {
                key: 9,
                input: json!(null),
            },
            &[],
        )
        .unwrap();
    let mut driven = Driven::start(supervisor, &screen).await;
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert!(
        rig.health()
            .iter()
            .any(|line| line.contains("ledger_receipt_identity_missing"))
    );
    let cap = scan(&mut driven, 10).await;
    let before = wal_seq(&mut driven);
    let (reply, response) = oneshot::channel();
    let admission = driven
        .supervisor
        .receipt_admission(&driven.drive, Some(Mode::Held));
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CommitFromScan {
                source: Box::new(input(&rig, 10)),
                capability: cap,
                reply,
            },
            admission.receipt_open,
        )
        .await;
    assert_eq!(
        response.await.unwrap().receipt,
        Receipt::Deferred(Deferred::Closed)
    );
    assert_eq!(wal_seq(&mut driven), before);
    assert!(driven.release());
}

#[tokio::test]
async fn g1a_r1a_nonfirst_primary_accepts_and_settles_every_source() {
    let rig = Rig::new(6_325_926);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let before = wal_seq(&mut driven);
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let result = commit_merged(&mut driven, merged(&rig, 11, vec![10, 11]), cap).await;
    assert!(matches!(result.receipt, Receipt::Accepted(_)));
    let result = commit_source(&mut driven, &rig, 12, result.capability).await;
    assert!(matches!(result.receipt, Receipt::Accepted(_)));
    assert_eq!(wal_seq(&mut driven), before + 2);
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CompleteScan {
                capability: result.capability,
                reply,
            },
            true,
        )
        .await;
    assert!(response.await.unwrap().is_ok());
    assert!(driven.release());
}

#[tokio::test]
async fn g1a_r1a_nonfirst_primary_duplicate_and_legacy_settle_full_prefix_without_append() {
    for (index, legacy) in [false, true].into_iter().enumerate() {
        let rig = Rig::new(6_325_927 + index as u64);
        let screen = Arc::new(Screen::default());
        let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
        let source = merged(&rig, 11, vec![10, 11]);
        let receipt = loan(&mut driven.supervisor.slot, move |lease| {
            crate::services::discord::input_runtime::supervisor::receipt::commit(
                lease, source, true,
            )
        })
        .await
        .unwrap();
        assert!(matches!(receipt, Receipt::Accepted(_)));
        if legacy {
            loan(&mut driven.supervisor.slot, |lease| {
                lease.get().unwrap().append_entry(
                    &Entry::Transition {
                        key: 11,
                        state: RowState::Abandoned(AbandonReason::Handback),
                        attempt: None,
                    },
                    &[],
                )
            })
            .await
            .unwrap()
            .unwrap();
        }
        let saved = driven
            .supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .row(11)
            .unwrap()
            .input
            .clone();
        let before = wal_seq(&mut driven);
        let cap = scan_page(&mut driven, vec![10, 11], 11).await;
        let result = commit_merged(&mut driven, merged(&rig, 11, vec![10, 11]), cap).await;
        if legacy {
            assert!(matches!(result.receipt, Receipt::LegacyResponsibility(_)));
        } else {
            assert!(matches!(result.receipt, Receipt::DuplicateQueued(_)));
        }
        assert_eq!(wal_seq(&mut driven), before);
        assert_eq!(
            driven
                .supervisor
                .slot()
                .get()
                .unwrap()
                .rows()
                .unwrap()
                .row(11)
                .unwrap()
                .input,
            saved
        );
        let (reply, response) = oneshot::channel();
        driven
            .supervisor
            .input_command(
                SupervisorCmd::CompleteScan {
                    capability: result.capability,
                    reply,
                },
                true,
            )
            .await;
        assert!(response.await.unwrap().is_ok());
        assert!(driven.release());
    }
}

#[tokio::test]
async fn g1a_r1a_nonfirst_primary_persistence_failure_retains_oldest_retry_obligation() {
    let rig = Rig::new(6_325_929);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let before = wal_seq(&mut driven);
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let source = Source::new(
        11,
        ReceiptIdentity::new(11, vec![10, 11], 7, rig.channel, rig.channel).unwrap(),
        json!({"text":"merged", "source_message_ids":[10,11]}),
        vec![crate::services::tui_input::blob::BlobPin {
            local_path: "blobs/att/11/0_missing.txt".into(),
            sha256: "0".repeat(64),
            pinned: true,
        }],
    )
    .unwrap();
    let result = commit_merged(&mut driven, source, cap).await;
    assert_eq!(result.receipt, Receipt::Deferred(Deferred::Persistence));
    assert_eq!(wal_seq(&mut driven), before);
    let ticket = fetch_ticket(&mut driven).await;
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
                ticket,
                sources: vec![12],
                horizon: 12,
                complete_fetch: true,
                reply,
            },
            true,
        )
        .await;
    assert!(response.await.unwrap().is_err());
    assert_eq!(wal_seq(&mut driven), before);
    let mut cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    for key in [10, 11, 12] {
        let result = commit_source(&mut driven, &rig, key, cap).await;
        assert!(matches!(result.receipt, Receipt::Accepted(_)));
        cap = result.capability;
    }
    assert_eq!(wal_seq(&mut driven), before + 3);
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CompleteScan {
                capability: cap,
                reply,
            },
            true,
        )
        .await;
    assert!(response.await.unwrap().is_ok());
    assert!(driven.release());
}

#[tokio::test]
async fn g1a_synced_receipt_dirty_race_reports_failed_settlement_and_blocks_offer_until_full_proof()
{
    let rig = Rig::new(6_325_936);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    *screen.record.lock().unwrap() = Some(a.path.clone());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let cap = scan_page(&mut driven, vec![10, 11], 11).await;
    let before = wal_seq(&mut driven);
    driven.supervisor.after_receipt_io = Some(driven.supervisor.pending_overflow());
    let result = commit_merged(&mut driven, merged(&rig, 11, vec![10, 11]), cap).await;
    assert!(matches!(result.receipt, Receipt::Accepted(_)));
    assert_eq!(result.settlement, Err(Failure::StalePermit));
    assert_eq!(wal_seq(&mut driven), before + 1);
    assert_eq!(driven.supervisor.pending_snapshot().sources, [10, 11]);
    let admission = driven
        .supervisor
        .receipt_admission(&driven.drive, Some(Mode::LedgerOpen));
    assert!(admission.receipt_open);
    assert!(!admission.submit_ready);
    driven.idle(4).await;
    assert_eq!(driven.sent(), 0);
    assert_eq!(wal_seq(&mut driven), before + 1);
    let cap = scan_page(&mut driven, vec![10, 11], 11).await;
    let result = commit_merged(&mut driven, merged(&rig, 11, vec![10, 11]), cap).await;
    assert!(matches!(result.receipt, Receipt::DuplicateQueued(_)));
    assert_eq!(result.settlement, Ok(()));
    assert!(driven.supervisor.pending_snapshot().sources.is_empty());
    driven.idle(4).await;
    assert_eq!(
        driven.sent(),
        0,
        "settled receipts still need a fresh complete-fetch proof"
    );
    assert_eq!(wal_seq(&mut driven), before + 1);
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CompleteScan {
                capability: result.capability,
                reply,
            },
            true,
        )
        .await;
    assert!(response.await.unwrap().is_ok());
    driven.idle(4).await;
    assert_eq!(driven.sent(), 1);
    assert!(driven.release());
}

#[tokio::test]
async fn g1a_uncertain_persistence_dirty_race_retains_whole_sources_without_wal_append() {
    let rig = Rig::new(6_325_937);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let before = wal_seq(&mut driven);
    driven.supervisor.after_receipt_io = Some(driven.supervisor.pending_overflow());
    let source = Source::new(
        11,
        ReceiptIdentity::new(11, vec![10, 11], 7, rig.channel, rig.channel).unwrap(),
        json!({"text":"merged", "source_message_ids":[10,11]}),
        vec![crate::services::tui_input::blob::BlobPin {
            local_path: "blobs/att/11/0_missing.txt".into(),
            sha256: "0".repeat(64),
            pinned: true,
        }],
    )
    .unwrap();
    let result = commit_merged(&mut driven, source, cap).await;
    assert_eq!(result.receipt, Receipt::Deferred(Deferred::Persistence));
    assert_eq!(result.settlement, Err(Failure::StalePermit));
    assert_eq!(wal_seq(&mut driven), before);
    assert_eq!(driven.supervisor.pending_snapshot().sources, [10, 11]);
    let ticket = fetch_ticket(&mut driven).await;
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
                ticket,
                sources: vec![12],
                horizon: 12,
                complete_fetch: true,
                reply,
            },
            true,
        )
        .await;
    assert!(response.await.unwrap().is_err());
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let result = commit_merged(&mut driven, merged(&rig, 11, vec![10, 11]), cap).await;
    assert!(matches!(result.receipt, Receipt::Accepted(_)));
    assert_eq!(result.settlement, Ok(()));
    let result = commit_source(&mut driven, &rig, 12, result.capability).await;
    assert!(matches!(result.receipt, Receipt::Accepted(_)));
    assert_eq!(result.settlement, Ok(()));
    assert_eq!(wal_seq(&mut driven), before + 2);
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::CompleteScan {
                capability: result.capability,
                reply,
            },
            true,
        )
        .await;
    assert!(response.await.unwrap().is_ok());
    assert!(driven.release());
    drop(rig);

    for (index, (receipt_open, reason)) in [(false, Deferred::Closed), (true, Deferred::Order)]
        .into_iter()
        .enumerate()
    {
        let rig = Rig::new(6_325_950 + index as u64);
        let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
        let cap = scan(&mut driven, 10).await;
        driven.supervisor.invalidate_order();
        let overflow = driven.supervisor.pending_overflow();
        driven.supervisor.after_receipt_io = Some(overflow.clone());
        let before = wal_seq(&mut driven);
        let (reply, response) = oneshot::channel();
        driven
            .supervisor
            .input_command(
                SupervisorCmd::CommitFromScan {
                    source: Box::new(input(&rig, 10)),
                    capability: cap,
                    reply,
                },
                receipt_open,
            )
            .await;
        let result = response.await.unwrap();
        assert_eq!(result.receipt, Receipt::Deferred(reason));
        assert_eq!(result.settlement, Err(Failure::StalePermit));
        assert!(driven.supervisor.pending_snapshot().sources.is_empty());
        assert_eq!(
            overflow.generation(),
            0,
            "pre-append rejection did not run the I/O seam"
        );
        assert_eq!(wal_seq(&mut driven), before);
        assert!(driven.release());
    }
}

#[tokio::test]
async fn g1a_uncertain_other_deferred_dirty_race_retains_only_prevalidated_retry_sources() {
    for (index, reason) in [Deferred::Conflict, Deferred::Unknown, Deferred::Order]
        .into_iter()
        .enumerate()
    {
        let rig = Rig::new(6_325_952 + index as u64);
        let screen = Arc::new(Screen::default());
        let mut supervisor = admitted(&rig, &[]).await;
        match reason {
            Deferred::Conflict => {
                let existing = input(&rig, 10);
                let receipt = loan(&mut supervisor.slot, move |lease| {
                    super::super::super::receipt::commit(lease, existing, true)
                })
                .await
                .unwrap();
                assert!(matches!(receipt, Receipt::Accepted(_)));
            }
            Deferred::Unknown => {
                supervisor
                    .slot()
                    .get()
                    .unwrap()
                    .append_entry(
                        &Entry::Received {
                            key: 9,
                            input: json!({"text":"old partial provenance"}),
                        },
                        &[],
                    )
                    .unwrap();
            }
            Deferred::Order => {}
            _ => unreachable!(),
        }
        let mut driven = Driven::start(supervisor, &screen).await;
        let admission = driven
            .supervisor
            .receipt_admission(&driven.drive, Some(rig.gate.mode()));
        assert!(
            admission.receipt_open,
            "partial old input has no identity hold"
        );
        let singleton = reason == Deferred::Order;
        let page = if singleton {
            vec![10, 12]
        } else {
            vec![10, 11, 12]
        };
        let cap = scan_page(&mut driven, page.clone(), 12).await;
        let before = wal_seq(&mut driven);
        let key = if singleton { 10 } else { 11 };
        let incoming = merged(&rig, key, vec![10, 11]);
        assert_eq!(
            driven
                .supervisor
                .order
                .permits_sources(
                    &cap,
                    driven.supervisor.admission_gen,
                    &incoming.identity().source_ids
                )
                .is_ok(),
            !singleton
        );
        if singleton {
            assert!(
                driven
                    .supervisor
                    .order
                    .permits(&cap, driven.supervisor.admission_gen, key)
                    .is_ok()
            );
        }
        driven.supervisor.after_receipt_io = Some(driven.supervisor.pending_overflow());
        let result = commit_merged(&mut driven, incoming, cap).await;
        assert_eq!(result.receipt, Receipt::Deferred(reason));
        assert_eq!(result.settlement, Err(Failure::StalePermit));
        assert_eq!(wal_seq(&mut driven), before);
        let expected = if singleton { vec![10] } else { vec![10, 11] };
        assert_eq!(driven.supervisor.pending_snapshot().sources, expected);
        assert!(!driven.supervisor.order.offer_ready());
        let ticket = fetch_ticket(&mut driven).await;
        let (reply, response) = oneshot::channel();
        driven
            .supervisor
            .input_command(
                SupervisorCmd::BeginScan {
                    ticket,
                    sources: vec![12],
                    horizon: 12,
                    complete_fetch: true,
                    reply,
                },
                true,
            )
            .await;
        assert!(response.await.unwrap().is_err());
        let cap = scan_page(&mut driven, page, 12).await;
        if singleton {
            let result = commit_source(&mut driven, &rig, 10, cap).await;
            assert!(matches!(result.receipt, Receipt::Accepted(_)));
            assert_eq!(result.settlement, Ok(()));
            let result = commit_source(&mut driven, &rig, 12, result.capability).await;
            assert!(matches!(result.receipt, Receipt::Accepted(_)));
            assert_eq!(result.settlement, Ok(()));
            assert_eq!(wal_seq(&mut driven), before + 2);
            let (reply, response) = oneshot::channel();
            driven
                .supervisor
                .input_command(
                    SupervisorCmd::CompleteScan {
                        capability: result.capability,
                        reply,
                    },
                    true,
                )
                .await;
            assert!(
                response.await.unwrap().is_ok(),
                "unvalidated alias11 is not retained"
            );
        }
        assert!(driven.release());
    }
}

fn external(rig: &Rig, text: &str, origin: &str) -> Box<Source> {
    use crate::services::discord::input_runtime::supervisor::external::source;
    Box::new(source(rig.channel, 7, text, Some("imessage"), origin).unwrap())
}

type External = crate::services::discord::input_runtime::supervisor::external::ExternalReceipt;

async fn submit_external(driven: &mut Driven, source: Box<Source>) -> External {
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(SupervisorCmd::SubmitExternal { source, reply }, true)
        .await;
    response.await.unwrap()
}

#[tokio::test]
async fn submit_external_same_origin_keeps_first_body_and_seq() {
    let rig = Rig::new(6_325_940);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    let mut supervisor = admitted(&rig, &[]).await;
    let (commands, received) = tokio::sync::mpsc::channel(4);
    let script = async {
        // The first reply is dropped; the receipt it asked for still lands once.
        let (reply, response) = oneshot::channel();
        let source = external(&rig, "first", "guid-1");
        let command = SupervisorCmd::SubmitExternal { source, reply };
        commands.send(command).await.unwrap();
        drop(response);
        let (reply, response) = oneshot::channel();
        let source = external(&rig, "second", "guid-1");
        let command = SupervisorCmd::SubmitExternal { source, reply };
        commands.send(command).await.unwrap();
        let answer = response.await.unwrap();
        drop(commands);
        answer
    };
    let ((), answer) = tokio::join!(supervisor.run(DriveFake(screen.clone()), received), script);
    let External::Received {
        receipt,
        duplicate: true,
        state: RowState::Received,
    } = answer
    else {
        panic!("not a duplicate receipt: {answer:?}")
    };
    let rows = supervisor.slot().reopen().unwrap().rows().unwrap();
    let row = rows.row(receipt.key).unwrap();
    assert_eq!(row.input["text"], "first");
    assert_eq!(row.received_seq, Some(receipt.received_seq));
    // The retry appended nothing after the one receipt.
    assert_eq!(rows.folded_seq(), receipt.received_seq);
    assert_eq!(rows.open_rows().count(), 1);
    assert_eq!(screen.sent.lock().unwrap().len(), 0);
    assert!(supervisor.release());
}

#[tokio::test]
async fn external_receipt_does_not_require_discord_scan() {
    let rig = Rig::new(6_325_941);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    // An unscanned Discord notice is pending and no fetch ticket exists.
    let (reply, response) = oneshot::channel();
    let pending = SupervisorCmd::PendingSource {
        sources: vec![10],
        reply,
    };
    driven.supervisor.input_command(pending, true).await;
    let before = response.await.unwrap().unwrap();
    let answer = submit_external(&mut driven, external(&rig, "hi", "guid-1")).await;
    assert!(
        matches!(
            answer,
            External::Received {
                duplicate: false,
                ..
            }
        ),
        "{answer:?}"
    );
    let after = driven.supervisor.pending_snapshot();
    assert_eq!(after.sources, before.sources);
    assert_eq!(after.dirty_generation, before.dirty_generation);
    assert!(driven.release());
}

#[tokio::test]
async fn external_receipt_busy_waits_for_existing_actor_without_reactions() {
    let rig = Rig::new(6_325_942);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    *screen.record.lock().unwrap() = Some(a.path.clone());
    screen.busy.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let answer = submit_external(&mut driven, external(&rig, "from imessage", "guid-1")).await;
    let External::Received { receipt, .. } = answer else {
        panic!("not received: {answer:?}")
    };
    driven.idle(3).await;
    assert_eq!(driven.sent(), 0, "a busy pane takes no write");
    assert_eq!(state(&rig, receipt.key), RowState::Received);
    screen.busy.store(false, Ordering::SeqCst);
    driven.idle(2).await;
    assert_eq!(driven.sent(), 1);
    assert!(screen.sent.lock().unwrap()[0].contains("from imessage"));
    assert_eq!(state(&rig, receipt.key), RowState::Running);
    turn_end(&a);
    driven.idle(1).await;
    let done = RowState::Done(DoneReason::Completed);
    assert_eq!(state(&rig, receipt.key), done);
    assert!(
        screen.reactions.lock().unwrap().is_empty(),
        "an external key has no Discord message to react on"
    );
    assert!(driven.release());
}

#[tokio::test(start_paused = true)]
async fn hm1_normal_boot_has_no_external_receipts() {
    // Discord, voice and headless keys move in and hand back as before; none reads as external.
    let keys = [REAL, 9_000_000_000_000_000_002, 9_100_000_000_000_000_003];
    let rig = Rig::new(6_325_943);
    let mut supervisor = rig.supervisor(Request::Ledger, keys.to_vec());
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    let rows = supervisor.slot().get().unwrap().rows().unwrap();
    let moved: Vec<u64> = rows.open_rows().map(|(key, _)| key).collect();
    assert_eq!(moved, keys);
    assert!(
        rows.open_rows()
            .all(|(_, row)| row.input.get("http_origin").is_none())
    );
    assert!(supervisor.release());
    let mut legacy = rig.supervisor(Request::Legacy, vec![]);
    assert_eq!(legacy.boot().await, Landing::HandedBack);
    assert_eq!(*rig.world.enqueued.lock().unwrap(), keys);
}
