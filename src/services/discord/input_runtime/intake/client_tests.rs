use super::*;
use crate::services::discord::input_runtime::intake::{Access, Applied, apply_from_scan, lookup};
use crate::services::discord::input_runtime::intake::{
    Classification, Origin, SourceDraft, prepare_source,
};
use crate::services::discord::input_runtime::supervisor::command::{SupervisorCmd, WakeCause};
use crate::services::discord::input_runtime::supervisor::receipt::{Deferred, Receipt};
use crate::services::tui_input::rows::receipt_identity::{ReceiptIdentity, Responsibility};
use tokio::sync::mpsc;

#[test]
fn c2_off_client_lookup_does_not_register_open_or_create_input_effects() {
    let registry = Box::leak(Box::new(Registry::new()));
    let before = crate::services::tui_input::ledger::OPENS.with(|count| count.get());
    assert!(matches!(
        lookup(registry, &ProviderKind::Claude, 6_325_201),
        Access::Legacy
    ));
    assert!(!registry.used());
    assert_eq!(registry.population_for_test(), (0, 0));
    assert_eq!(
        crate::services::tui_input::ledger::OPENS.with(|count| count.get()),
        before
    );
    assert!(fence::channel_gate(6_325_201).is_none());
}

#[tokio::test]
async fn c2_prebind_preadopt_dirty_and_weak_mailbox_share_the_existing_order() {
    let rig = Rig::new(6_325_202);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let overflow = supervisor.pending_overflow();
    assert!(matches!(
        lookup(rig.registry, &ProviderKind::Claude, rig.channel),
        Access::Deferred(Deferred::Closed)
    ));
    assert_eq!(overflow.generation(), 1);
    let (sender, mut receiver) = mpsc::channel(1);
    let client = supervisor.bind_intake(sender).unwrap();
    assert_eq!(
        client.pending(vec![10]).await.unwrap_err(),
        Deferred::Closed
    );
    assert_eq!(
        overflow.generation(),
        2,
        "binding must use the pre-bind overflow"
    );
    assert!(
        receiver.try_recv().is_err(),
        "pre-Adopt requests are not silently dispatched"
    );
    supervisor.publish_intake();
    assert!(matches!(
        lookup(rig.registry, &ProviderKind::Claude, rig.channel),
        Access::Deferred(Deferred::Closed)
    ));
    assert_eq!(overflow.generation(), 3);
    supervisor.adopt_task(rig.channel).unwrap();
    supervisor.publish_intake();
    assert!(matches!(
        lookup(rig.registry, &ProviderKind::Claude, rig.channel),
        Access::Ready(_)
    ));
    assert!(matches!(
        lookup(rig.registry, &ProviderKind::Codex, rig.channel),
        Access::Deferred(Deferred::Conflict)
    ));
    assert_eq!(
        overflow.generation(),
        3,
        "a foreign view cannot take execution authority"
    );
    drop(client);
    assert!(
        receiver.recv().await.is_none(),
        "registry weak refs cannot keep the mailbox alive"
    );
    assert!(matches!(
        lookup(rig.registry, &ProviderKind::Claude, rig.channel),
        Access::Deferred(Deferred::SupervisorLost)
    ));
    assert_eq!(overflow.generation(), 4);
    assert!(supervisor.release());
}

#[tokio::test]
async fn c2_mailbox_full_closed_reply_loss_and_cancellation_keep_catchup_obligations() {
    let rig = Rig::new(6_325_203);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let overflow = supervisor.pending_overflow();
    let (sender, mut receiver) = mpsc::channel(1);
    sender
        .try_send(SupervisorCmd::Wake {
            cause: WakeCause::Retry,
        })
        .unwrap();
    let client = supervisor.bind_intake(sender).unwrap();
    supervisor.adopt_task(rig.channel).unwrap();
    supervisor.publish_intake();
    assert_eq!(
        client.pending(vec![10]).await.unwrap_err(),
        Deferred::SupervisorLost
    );
    assert_eq!(overflow.generation(), 1);
    receiver.recv().await.unwrap();
    let identity = ReceiptIdentity::new(10, vec![10], 7, rig.channel, rig.channel).unwrap();
    let mut query = Box::pin(client.lookup_responsibility(identity.clone()));
    let command = tokio::select! {
        command = receiver.recv() => command.unwrap(),
        result = &mut query => panic!("reply was not produced: {result:?}"),
    };
    drop(command);
    assert_eq!(query.await.unwrap_err(), Deferred::SupervisorLost);
    assert_eq!(overflow.generation(), 2);
    let mut query = Box::pin(client.lookup_responsibility(identity));
    let command = tokio::select! {
        command = receiver.recv() => command.unwrap(),
        result = &mut query => panic!("reply was not produced: {result:?}"),
    };
    drop(query);
    assert_eq!(
        overflow.generation(),
        3,
        "a cancelled reply leaves the source discoverable"
    );
    drop(command);
    drop(receiver);
    assert_eq!(
        client.pending(vec![10]).await.unwrap_err(),
        Deferred::SupervisorLost
    );
    assert_eq!(overflow.generation(), 4);
    assert!(supervisor.release());
}

async fn roundtrip<T>(
    request: impl std::future::Future<Output = T>,
    supervisor: &mut Supervisor<Fake>,
    receiver: &mut mpsc::Receiver<SupervisorCmd>,
) -> T {
    let (result, ()) = tokio::join!(request, async {
        supervisor
            .input_command(receiver.recv().await.unwrap(), true)
            .await;
    });
    result
}

fn prepared(channel: u64) -> crate::services::discord::input_runtime::intake::PreparedSource {
    prepare_source(SourceDraft {
        origin: Origin::Message {
            message_id: 10,
            source_ids: vec![10, 11],
            author_id: 7,
            original_channel_id: channel,
            execution_channel_id: channel,
        },
        author_is_bot: false,
        original_text: "original".into(),
        prompt: "transformed".into(),
        reply_context: Some("original reply".into()),
        attachments: vec![],
        voice_announcement: None,
    })
    .unwrap()
}

#[tokio::test]
async fn c2_apply_from_real_scan_keeps_receipt_distinct_from_failed_settlement_and_retry() {
    let rig = Rig::new(6_325_204);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    let (sender, mut receiver) = mpsc::channel(4);
    let client = supervisor.bind_intake(sender).unwrap();
    supervisor.adopt_task(rig.channel).unwrap();
    supervisor.publish_intake();
    roundtrip(client.pending(vec![10, 11]), &mut supervisor, &mut receiver)
        .await
        .unwrap();
    let ticket = roundtrip(client.fetch_ticket(), &mut supervisor, &mut receiver)
        .await
        .unwrap();
    let capability = roundtrip(
        client.begin_scan(ticket, vec![10, 11], 11, true),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    supervisor.after_receipt_io = Some(supervisor.pending_overflow());
    let result = roundtrip(
        apply_from_scan(
            &client,
            Classification::Target,
            prepared(rig.channel),
            capability,
        ),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    let Applied::Receipt(committed) = result else {
        panic!("a receipt was expected")
    };
    assert!(matches!(committed.receipt, Receipt::Accepted(_)));
    assert!(
        committed.settlement.is_err(),
        "receipt success cannot synthesize settlement success"
    );
    assert_eq!(supervisor.pending_snapshot().sources, vec![10, 11]);
    assert_eq!(
        supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq(),
        1
    );
    let ticket = roundtrip(client.fetch_ticket(), &mut supervisor, &mut receiver)
        .await
        .unwrap();
    let capability = roundtrip(
        client.begin_scan(ticket, vec![10, 11], 11, true),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    let result = roundtrip(
        apply_from_scan(
            &client,
            Classification::Target,
            prepared(rig.channel),
            capability,
        ),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    let Applied::Receipt(committed) = result else {
        panic!("a duplicate was expected")
    };
    assert!(matches!(committed.receipt, Receipt::DuplicateQueued(_)));
    assert_eq!(committed.settlement, Ok(()));
    assert_eq!(
        supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq(),
        1
    );
    roundtrip(
        client.complete_scan(committed.capability),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    let result = roundtrip(
        client.lookup_responsibility(prepared(rig.channel).identity().clone()),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    assert!(matches!(result, Responsibility::Known { key: 10, .. }));
    assert!(supervisor.release());
}

#[tokio::test]
async fn c2_external_reply_loss_retains_original_and_does_not_cancel_the_received_loan() {
    let rig = Rig::new(6_325_205);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    let (sender, mut receiver) = mpsc::channel(1);
    let client = supervisor.bind_intake(sender).unwrap();
    supervisor.adopt_task(rig.channel).unwrap();
    supervisor.publish_intake();
    let original = prepare_source(SourceDraft {
        origin: Origin::SlashInteraction {
            interaction_id: 123,
            author_id: 7,
            original_channel_id: rig.channel,
            execution_channel_id: rig.channel,
        },
        author_is_bot: false,
        original_text: "/skill explain".into(),
        prompt: "expanded prompt".into(),
        reply_context: None,
        attachments: vec![],
        voice_announcement: None,
    })
    .unwrap();
    let identity = original.identity().clone();
    let input = original.input().clone();
    let mut request = Box::pin(client.submit_external(&original));
    let command = tokio::select! {
        command = receiver.recv() => command.unwrap(),
        result = &mut request => panic!("no receiver produced this result: {result:?}"),
    };
    drop(request);
    assert_eq!(supervisor.pending_overflow().generation(), 1);
    assert_eq!(original.identity(), &identity);
    assert_eq!(original.input(), &input);
    supervisor.input_command(command, true).await;
    let rows = supervisor.slot().get().unwrap().rows().unwrap();
    assert_eq!(rows.receipt_identity(original.key()), Some(identity));
    assert_eq!(rows.row(original.key()).unwrap().input, input);
    assert_eq!(rows.folded_seq(), 1);
    let result = roundtrip(
        client.submit_external(&original),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        external::ExternalReceipt::Received {
            duplicate: true,
            ..
        }
    ));
    assert_eq!(
        supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq(),
        1
    );
    assert!(supervisor.release());
}
