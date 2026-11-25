use crate::services::cluster::intake_router_hook::owner_record::output::tests::*;
use crate::services::cluster::intake_router_hook::owner_record::output::*;
use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

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
            .acquire(origin.channel_for_test(), "answer", Some(&artifact))
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
    one.acquire(origin.channel_for_test(), "answer", Some(&artifact))
        .await
        .unwrap()
        .run(async {})
        .await
        .unwrap();
    let mut generation = artifact.clone();
    *generation.origin_generation_mut_for_test() += 1;
    assert!(
        scope
            .acquire(origin.channel_for_test(), "answer", Some(&generation))
            .await
            .is_err()
    );
    let mut changed = artifact.clone();
    *changed.origin_binding_mut_for_test() = json!({"native":"successor"});
    assert!(
        scope
            .acquire(origin.channel_for_test(), "answer", Some(&changed))
            .await
            .is_err()
    );
    assert!(
        scope
            .acquire(origin.channel_for_test(), "changed", Some(&artifact))
            .await
            .is_err()
    );
    assert!(
        scope
            .acquire(origin.channel_for_test(), "answer", None)
            .await
            .is_err()
    );
    assert!(
        acquire(None, origin.channel_for_test(), "answer", Some(&artifact))
            .await
            .is_err()
    );
    revoke(&pool, &origin).await;
    assert_eq!(
        source_artifact(
            Some(&pool),
            "claude",
            origin.channel_for_test(),
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
            .acquire(origin.channel_for_test(), "answer", Some(&artifact))
            .await
            .is_err()
    );
    pool.close().await;
    assert!(
        scope
            .acquire(origin.channel_for_test(), "answer", Some(&artifact))
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
    custody::lock(
        &mut tx,
        &OwnerIdentity::new("claude", origin.channel_for_test()),
    )
    .await
    .unwrap();
    crate::db::replay_disposition::write::withhold_owner_loss_on_tx(
        &mut tx,
        origin.receipt_for_test(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        scope
            .acquire(origin.channel_for_test(), "answer", Some(&artifact))
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
        .acquire(origin.channel_for_test(), "answer", Some(&artifact))
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
        .bind(OwnerIdentity::new("claude", origin.channel_for_test()).advisory_key())
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
    let channel = origin.channel_for_test().parse::<u64>().unwrap();
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
        .begin_fixture_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                policy_parent: None,
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
        .bind(OwnerIdentity::new("claude", origin.channel_for_test()).advisory_key())
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
                .bind(OwnerIdentity::new("claude", origin.channel_for_test()).advisory_key())
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
        .begin_fixture_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                policy_parent: None,
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
