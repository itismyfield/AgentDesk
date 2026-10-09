use super::*;
use crate::services::discord::input_runtime::command::ScanCommit;
use crate::services::discord::input_runtime::receipt::{Deferred, Receipt};
use crate::services::discord::input_runtime::source::Source;
use crate::services::tui_input::receipt_identity::{ReceiptIdentity, Responsibility};
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
    for (index, running) in [true, false].into_iter().enumerate() {
        let rig = Rig::new(6_325_901 + index as u64);
        let a = if running {
            transcript(&rig, "a", false)
        } else {
            idle_transcript(&rig, "a", false)
        };
        if running {
            user(&a.path, "prior human input");
        }
        push(&rig, "n1", source(None, &a));
        let screen = Arc::new(Screen::default());
        screen.draft.store(!running, Ordering::SeqCst);
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
            assert_eq!(response.await.unwrap().sources, [10]);
            let (reply, response) = oneshot::channel();
            commands
                .send(SupervisorCmd::BeginScan {
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
) -> crate::services::discord::input_runtime::ordering::ScanCapability {
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
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
    capability: crate::services::discord::input_runtime::ordering::ScanCapability,
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
    let (reply, response) = oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
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
