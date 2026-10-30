use super::*;
use crate::services::cluster::intake_router_hook::owner_record::output;
use crate::services::tui_o::repost::dispatch::RepostWriter;
use crate::services::tui_o::shadow::{UnitKey, UnitKind};
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, WriterAlarm, deliver::ChannelWriter,
};
use crate::services::tui_o::{
    ownership::OwnershipGate,
    shadow::ShadowProvider,
    store::ledger::{LedgerEntry, PieceOutcome},
    store::{Initialized, OStore, StoreConfig},
};

#[derive(Clone, Default)]
struct Alarms(Arc<StdMutex<Vec<WriterAlarm>>>);
impl AlarmSink for Alarms {
    fn raise(&self, _: u64, a: WriterAlarm) {
        self.0.lock().unwrap().push(a);
    }
}
struct Lease;
impl DeliveryLease for Lease {
    type Held = ();
    fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
        Some(())
    }
}

#[tokio::test]
async fn owner_repost_real_http_revocation_preserves_unresolved_and_stops_pg() {
    let (_db, pool, origin) = output::tests::fixture("73369490").await;
    let mock = Mock::start(vec![created(Some(MARKER))]).await;
    let scope = output::Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    let port = mock.port().with_owner_scope(Some(scope.clone()));
    let mut envelope =
        RepostEnvelope::additional(73369490, "x".into(), RepostIds::for_piece(MARKER).unwrap());
    envelope.output_origin = Some(origin.seal("x"));
    let good = send_within(&port, &envelope, always(), Duration::from_secs(5)).await;
    assert!(matches!(good.outcome, WireOutcome::Created(_)));
    assert_eq!(good.counted, 1);
    output::tests::revoke(&pool, &origin).await;
    let denied = send_within(&port, &envelope, always(), Duration::from_secs(5)).await;
    assert_eq!(denied.wire, 0);
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    let runtime = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
        .unwrap()
        .unwrap();
    let init = Initialized {
        channel: 73369490,
        sources: Vec::new(),
        initial_anchor: 0,
        build_digest: "fixture".into(),
        at: chrono::Utc::now(),
    };
    store
        .begin_era_as(&init, crate::services::tui_o::store::InitOrigin::Fresh)
        .unwrap();
    let era = store.read_era().unwrap().unwrap();
    let mut opened = store.open_channel(&era, 73369490).unwrap().unwrap();
    opened
        .append_ledger(LedgerEntry::OutputOrigin {
            serial: 0,
            artifact: origin.seal("x"),
        })
        .unwrap();
    opened
        .append_ledger(LedgerEntry::Prepared {
            notice_origin: None,
            serial: 0,
            unit_key: UnitKey {
                channel_id: 73369490,
                provider: ShadowProvider::Claude,
                native_key: "msg_original".into(),
                kind: UnitKind::Body,
            },
            piece_index: 0,
            payload: "x".into(),
            anchor_id: 0,
            epoch: 1,
            local_retry: None,
        })
        .unwrap();
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let alarms = Alarms::default();
    let http = Arc::new(serenity::Http::new("test-token"));
    let gateway = Arc::new(
        crate::services::discord::outbound::o_writer_io::GatewayPort::new(http, 42)
            .with_owner_scope(scope),
    );
    let mut writer = ChannelWriter::new(opened, gate, gateway, Lease, alarms.clone());
    RepostWriter::record(&mut writer, &denied).await;
    assert!(
        matches!(denied.outcome, WireOutcome::Withheld(_)),
        "origin refusal must not be an ordinary Unsent"
    );

    assert!(matches!(
        writer.store().ledger().piece(0).unwrap().outcome,
        Some(PieceOutcome::Unresolved(_))
    ));
    assert!(matches!(
        alarms.0.lock().unwrap().as_slice(),
        [WriterAlarm::Unresolved { .. }]
    ));
    assert!(writer.is_stopped());
    pool.close().await;
}

#[tokio::test]
async fn owner_repost_wire_mentions_override_unsafe_client_defaults() {
    let mock = Mock::start(vec![created(Some(MARKER))]).await;
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .default_allowed_mentions(
                serenity::CreateAllowedMentions::new()
                    .everyone(true)
                    .all_roles(true),
            )
            .build(),
    );
    let port = RepostHttp::at(http, mock.base.clone()).unwrap();
    let report = send_within(&port, &envelope(), always(), Duration::from_secs(5)).await;
    assert!(matches!(report.outcome, WireOutcome::Created(_)));
    assert_eq!(report.counted, 1);
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].1).unwrap();
    assert_eq!(body["allowed_mentions"]["parse"], json!(["users"]));
}
