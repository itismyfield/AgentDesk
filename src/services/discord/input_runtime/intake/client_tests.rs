use super::*;
use crate::services::discord::input_runtime::intake::{
    Access, Applied, Client, apply_from_scan, lookup,
};
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
    let channel = if crate::services::discord::input_runtime::supervisor::mutant(
        "c2b_client_fixture_reuses_fence_ids",
    ) {
        6_325_202
    } else {
        963_259_820_001
    };
    let rig = Rig::new(channel);
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
    let channel = if crate::services::discord::input_runtime::supervisor::mutant(
        "c2b_client_fixture_reuses_fence_ids",
    ) {
        6_325_203
    } else {
        963_259_820_002
    };
    let rig = Rig::new(channel);
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
        Box::pin(supervisor.input_command(receiver.recv().await.unwrap(), true)).await;
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
async fn c2c1_lookup_returns_persisted_canonical_receipt() {
    let rig = Rig::new(963_259_830_001);
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
    let original = prepared(rig.channel);
    let canonical = original.identity().clone();
    let committed = roundtrip(
        apply_from_scan(&client, Classification::Target, original, capability),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    let Applied::Receipt(committed) = committed else {
        panic!("the valid scan must produce a durable receipt")
    };
    assert!(matches!(committed.receipt, Receipt::Accepted(_)));
    assert_eq!(committed.settlement, Ok(()));
    let alias = ReceiptIdentity::new(11, vec![11], 7, rig.channel, rig.channel).unwrap();
    let response = roundtrip(
        client.lookup_responsibility(alias),
        &mut supervisor,
        &mut receiver,
    )
    .await
    .unwrap();
    assert!(supervisor.release());
    println!("C2C1_CANONICAL_LOOKUP_RESPONSE={response:?}");
    assert!(
        format!("{response:?}").contains(&format!("{canonical:?}")),
        "alias lookup must return persisted canonical provenance: {response:?}"
    );
}

#[tokio::test]
async fn c2_apply_from_real_scan_keeps_receipt_distinct_from_failed_settlement_and_retry() {
    let channel = if crate::services::discord::input_runtime::supervisor::mutant(
        "c2b_client_fixture_reuses_fence_ids",
    ) {
        6_325_204
    } else {
        963_259_820_003
    };
    let rig = Rig::new(channel);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    let before_receipt = supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq();
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
        before_receipt + 1
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
        before_receipt + 1
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
    assert!(matches!(
        result.responsibility,
        Responsibility::Known { key: 10, .. }
    ));
    assert!(supervisor.release());
}

#[tokio::test]
async fn c2_external_reply_loss_retains_original_and_does_not_cancel_the_received_loan() {
    let channel = if crate::services::discord::input_runtime::supervisor::mutant(
        "c2b_client_fixture_reuses_fence_ids",
    ) {
        6_325_205
    } else {
        963_259_820_004
    };
    let rig = Rig::new(channel);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    let before_receipt = supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq();
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
    assert_eq!(rows.folded_seq(), before_receipt + 1);
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
        before_receipt + 1
    );
    assert!(supervisor.release());
}

#[derive(Debug)]
pub(in crate::services::discord) struct ScanFixtureEvidence {
    pub(crate) original_input: serde_json::Value,
    pub(crate) original_identity: ReceiptIdentity,
    pub(crate) persisted_row: Option<crate::services::tui_input::rows::Row>,
    pub(crate) persisted_identity: Option<ReceiptIdentity>,
    pub(crate) open_rows: usize,
    pub(crate) pending: ordering::PendingSource,
}

pub(in crate::services::discord) struct MessageFixtureEvidence {
    pub(crate) rows: Vec<crate::services::tui_input::rows::Row>,
    pub(crate) pending: ordering::PendingSource,
    pub(crate) commands: Vec<&'static str>,
}

pub(in crate::services::discord) async fn run_messages<F, Fut, T>(
    registry: &'static Registry,
    channel: u64,
    root: &Path,
    fail_settlement: bool,
    callback: F,
) -> (T, MessageFixtureEvidence)
where
    F: FnOnce(Arc<Client>, ordering::PendingOverflow) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let world = Arc::new(World::default());
    let reserved = registry
        .reserve_for_test(&ProviderKind::Claude, channel, root)
        .unwrap();
    let config = Config {
        provider: ProviderKind::Claude,
        channel,
        root: root.to_owned(),
        request: Request::Ledger,
        refusal: None,
        binding: Binding::new(channel, &world),
    };
    let ports = Fake {
        channel,
        world,
        registry,
        inputs: vec![],
        mailbox: ChannelMailboxRegistry::default(),
    };
    let mut supervisor = Supervisor::start(reserved, config, ports).unwrap();
    assert_eq!(Box::pin(supervisor.boot()).await, Landing::Admitted);
    let (sender, mut receiver) = mpsc::channel(8);
    let client = supervisor.bind_intake(sender).unwrap();
    supervisor.adopt_task(channel).unwrap();
    supervisor.publish_intake();
    let overflow = supervisor.pending_overflow();
    if fail_settlement {
        supervisor.after_receipt_io = Some(overflow.clone());
    }
    let request = callback(client, overflow);
    tokio::pin!(request);
    let mut commands = vec![];
    let result = loop {
        tokio::select! {
            biased;
            result = &mut request => break result,
            command = receiver.recv() => {
                let command = command.expect("fixture retains the actual supervisor client");
                let pending = match &command {
                    SupervisorCmd::PendingSource { sources, .. } => Some(sources.clone()),
                    _ => None,
                };
                commands.push(match &command {
                    SupervisorCmd::PendingSource { .. } => "pending",
                    SupervisorCmd::LookupResponsibility { .. } => "lookup",
                    SupervisorCmd::FetchTicket { .. } => "ticket",
                    SupervisorCmd::BeginScan { .. } => "begin",
                    SupervisorCmd::CommitFromScan { .. } => "commit",
                    SupervisorCmd::SettleFromScan { .. } => "settle",
                    SupervisorCmd::CompleteScan { .. } => "complete",
                    _ => panic!("message fixture never starts or offers provider execution"),
                });
                Box::pin(supervisor.input_command(command, true)).await;
                if let Some(sources) = pending {
                    let actual = supervisor.pending_snapshot().sources;
                    assert!(sources.iter().all(|source| actual.contains(source)),
                        "fixture observes real pending ACK before reply/download IO");
                    println!("C2C2_MESSAGE_FIXTURE_PENDING_ACK sources={sources:?}");
                }
            }
        }
    };
    let rows = supervisor.slot().get().unwrap().rows().unwrap();
    let evidence = MessageFixtureEvidence {
        rows: rows.open_rows().map(|(_, row)| row.clone()).collect(),
        pending: supervisor.pending_snapshot(),
        commands,
    };
    assert!(supervisor.release());
    (result, evidence)
}

pub(in crate::services::discord) fn run_scan_fixture<F, Fut, T>(
    channel: u64,
    fail_settlement: bool,
    callback: F,
) -> std::pin::Pin<Box<impl std::future::Future<Output = (T, ScanFixtureEvidence)>>>
where
    F: FnOnce(
        &'static Registry,
        Arc<Client>,
        crate::services::discord::input_runtime::intake::PreparedSource,
        ordering::ScanCapability,
    ) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    Box::pin(async move {
        let rig = Rig::new(channel);
        let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
        assert_eq!(Box::pin(supervisor.boot()).await, Landing::Admitted);
        let (sender, mut receiver) = mpsc::channel(8);
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
            client.begin_scan(ticket, vec![10, 11, 12], 12, true),
            &mut supervisor,
            &mut receiver,
        )
        .await
        .unwrap();
        if fail_settlement {
            supervisor.after_receipt_io = Some(supervisor.pending_overflow());
        }
        let original = prepared(rig.channel);
        let original_input = original.input().clone();
        let original_identity = original.identity().clone();
        let request = callback(rig.registry, client, original, capability);
        tokio::pin!(request);
        let result = loop {
            tokio::select! {
                biased;
                result = &mut request => break result,
                command = receiver.recv() => {
                    Box::pin(supervisor.input_command(command.expect("fixture mailbox remains live"), true)).await;
                }
            }
        };
        let rows = supervisor.slot().get().unwrap().rows().unwrap();
        let evidence = ScanFixtureEvidence {
            original_input,
            original_identity,
            persisted_row: rows.row(10).cloned(),
            persisted_identity: rows.receipt_identity(10),
            open_rows: rows.open_rows().count(),
            pending: supervisor.pending_snapshot(),
        };
        assert!(supervisor.release());
        (result, evidence)
    })
}

async fn lookup_rows(
    channel: u64,
    inputs: Vec<(u64, serde_json::Value)>,
    handback: Option<u64>,
    queries: Vec<ReceiptIdentity>,
) -> (
    Vec<receipt::ResponsibilityLookup>,
    Vec<receipt::DurableReceipt>,
) {
    let rig = Rig::new(channel);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(Box::pin(supervisor.boot()).await, Landing::Admitted);
    let ledger = supervisor.slot().get().unwrap();
    for (key, input) in inputs {
        ledger
            .append_entry(&Entry::Received { key, input }, &[])
            .unwrap();
    }
    let rows = ledger.rows().unwrap();
    let expected = rows
        .open_rows()
        .filter_map(|(key, row)| {
            Some(receipt::DurableReceipt {
                key,
                received_seq: row.received_seq?,
                identity: rows.receipt_identity(key)?,
            })
        })
        .collect();
    if let Some(key) = handback {
        ledger
            .append_entry(
                &Entry::Transition {
                    key,
                    state: RowState::Abandoned(AbandonReason::Handback),
                    attempt: None,
                },
                &[],
            )
            .unwrap();
    }
    let (sender, mut receiver) = mpsc::channel(8);
    let client = supervisor.bind_intake(sender).unwrap();
    supervisor.adopt_task(rig.channel).unwrap();
    supervisor.publish_intake();
    let mut responses = Vec::new();
    for query in queries {
        responses.push(
            roundtrip(
                client.lookup_responsibility(query),
                &mut supervisor,
                &mut receiver,
            )
            .await
            .unwrap(),
        );
    }
    assert!(supervisor.release());
    (responses, expected)
}

#[tokio::test]
async fn c2c1_lookup_preserves_full_coverage_and_each_provenance_field() {
    let channel = 963_259_830_002;
    let original = prepared(channel);
    let query = |ids: Vec<u64>, author, source_channel, execution| {
        ReceiptIdentity::new(ids[0], ids, author, source_channel, execution).unwrap()
    };
    let queries = vec![
        query(vec![10, 11], 7, channel, channel),
        query(vec![11], 7, channel, channel),
        query(vec![10, 12], 7, channel, channel),
        query(vec![10, 11], 8, channel, channel),
        query(vec![10, 11], 7, channel + 1, channel),
        query(vec![10, 11], 7, channel, channel + 1),
        query(vec![12], 7, channel, channel),
    ];
    let (responses, expected) =
        lookup_rows(channel, vec![(10, original.input().clone())], None, queries).await;
    let receipt = expected[0].clone();
    let known = receipt::ResponsibilityLookup {
        responsibility: Responsibility::Known {
            key: receipt.key,
            received_seq: receipt.received_seq,
        },
        receipt: Some(receipt),
    };
    assert_eq!(responses[0], known);
    assert_eq!(
        responses[1], known,
        "a covered alias returns the entire canonical receipt"
    );
    for response in &responses[2..6] {
        assert_eq!(
            *response,
            receipt::ResponsibilityLookup {
                responsibility: Responsibility::Conflict,
                receipt: None
            }
        );
    }
    assert_eq!(
        responses[6],
        receipt::ResponsibilityLookup {
            responsibility: Responsibility::Absent,
            receipt: None
        }
    );
}

#[tokio::test]
async fn c2c1_lookup_rejects_foreign_execution_and_multiple_canonical_rows() {
    let channel = 963_259_830_003;
    let query = ReceiptIdentity::new(11, vec![11], 7, channel, channel).unwrap();
    let foreign = ReceiptIdentity::new(10, vec![10, 11], 7, channel, channel + 1).unwrap();
    let (responses, _) = lookup_rows(
        channel,
        vec![(10, serde_json::json!({ "receipt_identity": foreign }))],
        None,
        vec![query],
    )
    .await;
    assert_eq!(
        responses[0],
        receipt::ResponsibilityLookup {
            responsibility: Responsibility::Conflict,
            receipt: None
        }
    );

    let channel = 963_259_830_004;
    let original = prepared(channel);
    let other = ReceiptIdentity::new(12, vec![11, 12], 7, channel, channel).unwrap();
    let query = ReceiptIdentity::new(11, vec![11], 7, channel, channel).unwrap();
    let (responses, _) = lookup_rows(
        channel,
        vec![
            (10, original.input().clone()),
            (12, serde_json::json!({ "receipt_identity": other })),
        ],
        None,
        vec![query],
    )
    .await;
    assert_eq!(
        responses[0],
        receipt::ResponsibilityLookup {
            responsibility: Responsibility::Conflict,
            receipt: None
        }
    );
}

#[tokio::test]
async fn c2c1_lookup_keeps_unknown_and_legacy_responsibility_distinct() {
    let channel = 963_259_830_005;
    let invalid = serde_json::json!({ "receipt_identity": { "source_ids": [10, 11], "author_id": 0, "original_channel_id": channel, "execution_channel_id": channel } });
    let query = ReceiptIdentity::new(11, vec![11], 7, channel, channel).unwrap();
    let (responses, _) = lookup_rows(channel, vec![(10, invalid)], None, vec![query]).await;
    assert_eq!(
        responses[0],
        receipt::ResponsibilityLookup {
            responsibility: Responsibility::Unknown,
            receipt: None
        }
    );

    let channel = 963_259_830_006;
    let original = prepared(channel);
    let query = ReceiptIdentity::new(11, vec![11], 7, channel, channel).unwrap();
    let (responses, expected) = lookup_rows(
        channel,
        vec![(10, original.input().clone())],
        Some(10),
        vec![query],
    )
    .await;
    let receipt = expected[0].clone();
    assert_eq!(
        responses[0],
        receipt::ResponsibilityLookup {
            responsibility: Responsibility::Legacy {
                key: receipt.key,
                received_seq: receipt.received_seq
            },
            receipt: Some(receipt),
        }
    );
}
