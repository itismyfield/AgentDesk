use super::*;
use crate::db::{auto_queue::test_support::TestPostgresDb, intake_outbox as outbox};
use crate::services::cluster::intake_router_hook::owner_record::{
    custody, input_start, transfer_owner_in_tx,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(crate) fn payload(channel: &str, message: &str, gateway: &str) -> outbox::InsertPendingPayload {
    outbox::InsertPendingPayload {
        target_instance_id: gateway.into(),
        forwarded_by_instance_id: gateway.into(),
        required_labels: json!([]),
        execution_requirements: json!({}),
        attachment_refs: json!([]),
        channel_id: channel.into(),
        user_msg_id: message.into(),
        replay_source_message_ids: Some(vec![message.into()]),
        request_owner_id: "100".into(),
        request_owner_name: Some("operator".into()),
        user_text: format!("original {message}"),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "custody-agent".into(),
        provider: "claude".into(),
    }
}
struct RuntimeRoot {
    _root: tempfile::TempDir,
    _config: crate::config::TestEnvVarGuard,
    previous: Option<std::ffi::OsString>,
    _live: crate::config_live_reload::TestLiveGuard,
}
impl RuntimeRoot {
    fn new() -> Self {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let live = crate::config_live_reload::test_live_guard(lock);
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.yaml");
        std::fs::write(&config_path, "server: {}\n").unwrap();
        let config_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &config_path,
        );
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        // SAFETY: the process-wide test environment lock is held until Drop restores the root.
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
        }
        let mut config = crate::config::Config::default();
        config.data.dir = root.path().join("data");
        crate::config_live_reload::install(config);
        Self {
            _live: live,
            _root: root,
            _config: config_guard,
            previous,
        }
    }
}
impl Drop for RuntimeRoot {
    fn drop(&mut self) {
        // SAFETY: the environment lock still belongs to this fixture.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

pub(crate) struct FixtureDb {
    _db: TestPostgresDb,
    _root: RuntimeRoot,
}

impl FixtureDb {
    pub(crate) async fn single_pool(&self) -> PgPool {
        self._db.connect_and_migrate_with_max_connections(1).await
    }
}

pub(crate) async fn fixture(channel: &str) -> (FixtureDb, PgPool, Origin) {
    let root = RuntimeRoot::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider) VALUES('custody-agent','custody','claude')")
        .execute(&pool)
        .await
        .unwrap();
    let p = payload(channel, "90001", "gateway");
    let delivery = custody::stage(
        &pool,
        custody::StageRequest {
            payload: &p,
            home_revision: 0,
            wait_generation: None,
            grace_seconds: 0,
        },
    )
    .await
    .unwrap();
    assert!(
        custody::promote(
            &pool,
            delivery,
            "executor",
            None,
            custody::SwitchReadiness::Wait
        )
        .await
        .unwrap()
    );
    let row =
        outbox::claim_pending_for_target_except(&pool, "executor", "claude", "worker-token", &[])
            .await
            .unwrap()
            .unwrap();
    assert_eq!(row.id, delivery);
    assert!(
        outbox::mark_accepted(&pool, delivery, "worker-token")
            .await
            .unwrap()
    );
    assert!(
        outbox::mark_spawned(&pool, delivery, "worker-token")
            .await
            .unwrap()
    );
    let start = input_start::prepare(
        &pool,
        input_start::StartRequest {
            delivery_id: delivery,
            executor: "executor",
            claim_owner: "worker-token",
            incarnation: "origin-runtime",
            frame: "input",
            native_binding: &json!({"native":"original","nonce":"fixed"}),
        },
    )
    .await
    .unwrap();
    let origin = start.output_origin();
    start
        .submit(
            "input",
            &json!({"native":"original","nonce":"fixed"}),
            |_| async {},
        )
        .await
        .unwrap();
    (
        FixtureDb {
            _db: db,
            _root: root,
        },
        pool,
        origin,
    )
}

pub(crate) async fn revoke(pool: &PgPool, origin: &Origin) {
    let mut tx = pool.begin().await.unwrap();
    let id = OwnerIdentity::new(&origin.provider, &origin.channel);
    custody::lock(&mut tx, &id).await.unwrap();
    let result = transfer_owner_in_tx(
        &mut tx,
        &id,
        &origin.executor,
        origin.generation,
        "returned-home",
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        super::super::TransferOutcome::Transferred { .. }
    ));
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn output_origin_original_admission_survives_owner_return_and_rejects_old_native_pg() {
    let (_db, pool, origin) = fixture("7336901").await;
    let artifact = origin.seal("answer");
    let scope = Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    assert!(
        scope
            .acquire(&origin.channel, "answer", Some(&artifact))
            .await
            .unwrap()
            .run(async { 1 })
            .await
            .is_ok()
    );
    let single = _db._db.connect_and_migrate_with_max_connections(1).await;
    let one = Scope {
        pool: single,
        provider: "claude".into(),
    };
    one.acquire(&origin.channel, "answer", Some(&artifact))
        .await
        .unwrap()
        .run(async {})
        .await
        .unwrap();
    let mut generation = artifact.clone();
    generation.origin.generation += 1;
    assert!(
        scope
            .acquire(&origin.channel, "answer", Some(&generation))
            .await
            .is_err()
    );
    let mut changed = artifact.clone();
    changed.origin.binding = json!({"native":"successor"});
    assert!(
        scope
            .acquire(&origin.channel, "answer", Some(&changed))
            .await
            .is_err()
    );
    assert!(
        scope
            .acquire(&origin.channel, "changed", Some(&artifact))
            .await
            .is_err()
    );
    assert!(
        scope
            .acquire(&origin.channel, "answer", None)
            .await
            .is_err()
    );
    assert!(
        acquire(None, &origin.channel, "answer", Some(&artifact))
            .await
            .is_err()
    );
    revoke(&pool, &origin).await;
    assert_eq!(
        source_artifact(
            Some(&pool),
            "claude",
            &origin.channel,
            "90001",
            None,
            "answer"
        )
        .await
        .unwrap(),
        Some(artifact.clone())
    );
    assert!(
        scope
            .acquire(&origin.channel, "answer", Some(&artifact))
            .await
            .is_err()
    );
    pool.close().await;
    assert!(
        scope
            .acquire(&origin.channel, "answer", Some(&artifact))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn output_origin_withheld_source_never_authorizes_fresh_effect_pg() {
    let (_db, pool, origin) = fixture("7336906").await;
    let scope = Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    let artifact = origin.seal("answer");
    let mut tx = pool.begin().await.unwrap();
    custody::lock(&mut tx, &OwnerIdentity::new("claude", &origin.channel))
        .await
        .unwrap();
    crate::db::replay_disposition::write::withhold_owner_loss_on_tx(&mut tx, origin.receipt)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        scope
            .acquire(&origin.channel, "answer", Some(&artifact))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn output_origin_owner_switch_waits_for_actual_effect_guard_pg() {
    let (_db, pool, origin) = fixture("7336902").await;
    let scope = Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    };
    let artifact = origin.seal("answer");
    let hold = scope
        .acquire(&origin.channel, "answer", Some(&artifact))
        .await
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let (entered, inside) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let raw = tokio::spawn(hold.run(async move {
        calls.fetch_add(1, Ordering::SeqCst);
        entered.send(()).unwrap();
        wait.await.unwrap();
    }));
    inside.await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(OwnerIdentity::new("claude", &origin.channel).advisory_key())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(!locked, "owner switch must wait for the raw effect");
    tx.rollback().await.unwrap();
    release.send(()).unwrap();
    raw.await.unwrap().unwrap();
    revoke(&pool, &origin).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn output_origin_o_request_keeps_guard_after_writer_waiter_aborts_pg() {
    use crate::services::discord::outbound::o_writer_io::GatewayPort;
    use crate::services::tui_o::{
        ownership::OwnershipGate,
        shadow::{ShadowProvider, UnitKey, UnitKind},
        store::{Initialized, OStore, StoreConfig},
        writer::{
            AlarmSink, DeliveryLease, WriterAlarm,
            deliver::ChannelWriter,
            pieces::{Derived, PieceWork},
        },
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct Lease;
    impl DeliveryLease for Lease {
        type Held = ();
        fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
            Some(())
        }
    }
    struct Alarms;
    impl AlarmSink for Alarms {
        fn raise(&self, _: u64, _: WriterAlarm) {}
    }
    let (_db, pool, origin) = fixture("7336903").await;
    let channel = origin.channel.parse::<u64>().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (entered, inside) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 8192];
        let n = socket.read(&mut bytes).await.unwrap();
        assert!(n > 0);
        entered.send(()).unwrap();
        wait.await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 403 Forbidden\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            )
            .await
            .unwrap();
    });
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("fixture-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    );
    let port = Arc::new(GatewayPort::new(http, 42).with_owner_scope(Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    }));
    let root = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
        .unwrap()
        .unwrap();
    store
        .begin_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![],
                initial_anchor: 100,
                build_digest: "fixture".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let era = store.read_era().unwrap().unwrap();
    let channel_store = store.open_channel(&era, channel).unwrap().unwrap();
    let gate = Arc::new(OwnershipGate::default());
    gate.reacquired().unwrap();
    let mut writer = ChannelWriter::new(channel_store, gate, port, Lease, Alarms);
    let piece = Derived::Piece(PieceWork {
        notice_origin: None,
        unit_key: UnitKey {
            channel_id: channel,
            provider: ShadowProvider::Claude,
            native_key: "reply".into(),
            kind: UnitKind::Body,
        },
        index: 0,
        payload: "answer".into(),
        output_origin: Some(origin.seal("answer")),
    });
    let parent = tokio::spawn(async move { writer.deliver(&piece).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), inside)
        .await
        .unwrap()
        .unwrap();
    parent.abort();
    let _ = parent.await;
    let mut tx = pool.begin().await.unwrap();
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(OwnerIdentity::new("claude", &origin.channel).advisory_key())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(
        !free,
        "aborting the writer wait must not release the spawned request's PG guard"
    );
    tx.rollback().await.unwrap();
    release.send(()).unwrap();
    server.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let mut tx = pool.begin().await.unwrap();
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(OwnerIdentity::new("claude", &origin.channel).advisory_key())
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            if free {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let reopened = store.open_channel(&era, channel).unwrap().unwrap();
    assert_eq!(
        reopened.ledger().output_origin(0),
        Some(&origin.seal("answer"))
    );
    let oldroot = tempfile::tempdir().unwrap();
    let oldstore = OStore::open_if_enabled(&StoreConfig { enabled: true }, oldroot.path())
        .unwrap()
        .unwrap();
    oldstore
        .begin_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![],
                initial_anchor: 100,
                build_digest: "legacy".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let oldera = oldstore.read_era().unwrap().unwrap();
    let mut legacy = oldstore.open_channel(&oldera, channel).unwrap().unwrap();
    use crate::services::tui_o::store::ledger::{LedgerEntry, LocalRetryMark, PieceOutcome};
    legacy
        .append_ledger(LedgerEntry::Prepared {
            notice_origin: None,
            serial: 0,
            unit_key: UnitKey {
                channel_id: channel,
                provider: ShadowProvider::Claude,
                native_key: "legacy-reply".into(),
                kind: UnitKind::Body,
            },
            piece_index: 0,
            payload: "answer".into(),
            anchor_id: 100,
            epoch: 1,
            local_retry: Some(LocalRetryMark::Budget),
        })
        .unwrap();
    legacy
        .append_ledger(LedgerEntry::NotFound { serial: 0 })
        .unwrap();
    drop(legacy);
    let legacy = oldstore.open_channel(&oldera, channel).unwrap().unwrap();
    assert!(legacy.ledger().output_origin(0).is_none());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let observed = posts.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 8192];
        assert!(socket.read(&mut bytes).await.unwrap() > 0);
        observed.fetch_add(1, Ordering::SeqCst);
        socket
            .write_all(
                b"HTTP/1.1 403 Forbidden\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            )
            .await
            .unwrap();
    });
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("fixture-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    );
    let port = Arc::new(GatewayPort::new(http, 42).with_owner_scope(Scope {
        pool: pool.clone(),
        provider: "claude".into(),
    }));
    let gate = Arc::new(OwnershipGate::default());
    gate.reacquired().unwrap();
    let mut writer = ChannelWriter::new(legacy, gate, port, Lease, Alarms);
    writer
        .retry_local(chrono::Utc::now() + chrono::Duration::days(1))
        .await;
    assert_eq!(
        posts.load(Ordering::SeqCst),
        0,
        "old-format ledger retry must never create a POST"
    );
    assert!(matches!(
        writer.store().ledger().piece(1).unwrap().outcome,
        Some(PieceOutcome::Unresolved(_))
    ));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn output_origin_database_failure_never_hides_local_denial_pg() {
    let (_db, pool, origin) = fixture("7336952").await;
    let artifact = origin.seal("answer");
    pool.close().await;
    let scope = Scope {
        pool,
        provider: "claude".into(),
    };
    assert!(matches!(
        scope.acquire("7336952", "answer", Some(&artifact)).await,
        Err(AcquireError::Database(_))
    ));
    for (channel, body) in [("7336953", "answer"), ("7336952", "changed")] {
        assert!(matches!(
            scope.acquire(channel, body, Some(&artifact)).await,
            Err(AcquireError::Withheld(_))
        ));
    }
    for (field, value) in [
        ("binding", serde_json::json!(null)),
        ("delivery", serde_json::json!(0)),
        ("receipt", serde_json::json!(0)),
        ("generation", serde_json::json!(-1)),
        ("executor", serde_json::json!("")),
        ("nonce", serde_json::json!("")),
        ("incarnation", serde_json::json!("")),
    ] {
        let mut bad = serde_json::to_value(&artifact).unwrap();
        bad["origin"][field] = value;
        let bad: Artifact = serde_json::from_value(bad).unwrap();
        assert!(
            matches!(
                scope.acquire("7336952", "answer", Some(&bad)).await,
                Err(AcquireError::Withheld(_))
            ),
            "malformed {field} must not turn into a database retry"
        );
    }
}
