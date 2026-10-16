use super::*;
use std::sync::Mutex;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

#[derive(Clone, Default)]
struct Reports(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Reports {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        #[derive(Default)]
        struct Fields(BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().into(), format!("{value:?}"));
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields
            .0
            .get("message")
            .is_some_and(|s| s.contains("re-post backlog"))
        {
            self.0.lock().unwrap().push(fields.0);
        }
    }
}

fn turn_with(
    switch: RepostSwitch,
    pool: PgPool,
    io: &Arc<Io>,
    path: &std::path::Path,
    opened: &Arc<AtomicUsize>,
) -> ChannelTurn<Io> {
    ChannelTurn::new(
        Runner::new(switch, pool, BOT, run()),
        {
            let (io, opened) = (Arc::clone(io), Arc::clone(opened));
            Box::new(move || {
                opened.fetch_add(1, Ordering::SeqCst);
                Some(Arc::clone(&io))
            })
        },
        CHANNEL,
        path.to_owned(),
    )
}

fn old_backlog(writer: &mut Writer) {
    writer.settled(
        "old-not-found",
        "old payload",
        LedgerEntry::NotFound { serial: 0 },
    );
    writer.settled(
        "old-unresolved",
        "other old payload",
        LedgerEntry::Unresolved {
            serial: 1,
            reason: "old read failed".into(),
        },
    );
}

#[tokio::test]
async fn d4act3_channel_turn_reports_old_backlog_once_across_reopen_and_reload_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::new(&dir);
    old_backlog(&mut writer);
    let ledger_path = dir
        .path()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("ledger.jsonl");
    let original_bytes = std::fs::read(&ledger_path).unwrap();
    let io = Arc::new(Io::new());
    let opened = Arc::new(AtomicUsize::new(0));
    let path = dir.path().join("backlog-provenance.jsonl");
    let reports = Reports::default();
    let subscriber = tracing::Dispatch::new(tracing_subscriber::registry().with(reports.clone()));
    let config = |enabled| {
        Arc::new(
            serde_yaml::from_str::<crate::config::Config>(&format!(
                "server:\n  port: 8791\ntui_o:\n  repost:\n    enabled: {enabled}\n    channels: [{CHANNEL}]\n"
            ))
            .unwrap(),
        )
    };
    let (tx, rx) = tokio::sync::watch::channel(Some(config(false)));
    let mut turn = turn_with(
        RepostSwitch::new(None, rx.clone()),
        pool.clone(),
        &io,
        &path,
        &opened,
    );
    turn.turn(&mut writer)
        .with_subscriber(subscriber.clone())
        .await;
    assert!(!path.exists(), "OFF creates no report sidecar");
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    assert!(reports.0.lock().unwrap().is_empty());

    tx.send_replace(Some(config(true)));
    turn.turn(&mut writer)
        .with_subscriber(subscriber.clone())
        .await;
    let state = ProvenanceLog::open(&path).unwrap();
    assert!(
        state.state().reported(0) && state.state().reported(1),
        "actual turn durably reports both old pieces"
    );
    assert_eq!(state.state().pending().count(), 0);
    let events = reports.0.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "first ON produces one operational report");
    assert_eq!(events[0]["count"], "2");
    assert_eq!(events[0]["channel"], CHANNEL.to_string());
    for name in ["old-not-found", "old-unresolved", "BeforeActivation"] {
        assert!(events[0]["pieces"].contains(name), "{events:?}");
    }
    assert!(
        !events[0]["pieces"].contains("old payload"),
        "report contains keys, not payloads"
    );
    turn.turn(&mut writer)
        .with_subscriber(subscriber.clone())
        .await;
    drop(turn);
    let mut turn = turn_with(
        RepostSwitch::new(None, rx),
        pool.clone(),
        &io,
        &path,
        &opened,
    );
    turn.turn(&mut writer)
        .with_subscriber(subscriber.clone())
        .await;
    let after_reopen = std::fs::read(&path).unwrap();
    tx.send_replace(Some(config(false)));
    turn.turn(&mut writer)
        .with_subscriber(subscriber.clone())
        .await;
    assert_eq!(
        std::fs::read(&path).unwrap(),
        after_reopen,
        "OFF adds no report or activation"
    );
    tx.send_replace(Some(config(true)));
    turn.turn(&mut writer).with_subscriber(subscriber).await;
    assert_eq!(
        reports.0.lock().unwrap().len(),
        1,
        "repeat/reopen/re-ON never duplicate a report"
    );
    assert_eq!(io.posts.load(Ordering::SeqCst), 0);
    assert_eq!(writer.prepared, 0);
    assert_eq!(
        std::fs::read(&ledger_path).unwrap(),
        original_bytes,
        "original ledger is preserved"
    );
    for table in ["o_piece_delivery", "o_piece_attempts", "o_piece_receipts"] {
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0, "old backlog never enters {table}");
    }
    assert_eq!(
        std::fs::read_to_string(&path)
            .unwrap()
            .matches("backlog_reported")
            .count(),
        1
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4act3_backlog_append_failure_blocks_ready_repost_and_retries_after_reopen_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::new(&dir);
    old_backlog(&mut writer);
    writer.settled(
        "proof",
        "permission proof",
        LedgerEntry::Posted {
            serial: 2,
            msg_id: 900,
        },
    );
    let ready =
        admit_on(&pool, &mut sidecar(&dir), &[(3, "ready")], "ready payload").await[0].clone();
    let io = Arc::new(Io::new());
    let opened = Arc::new(AtomicUsize::new(0));
    let path = dir.path().join("backlog-provenance.jsonl");
    let mut turn = turn_with(switch(true), pool.clone(), &io, &path, &opened);
    turn.runner
        .probes
        .insert(ready.clone(), ready_probe(&pool, &ready, &io).await);
    turn.log(writer.store.ledger().next_serial())
        .unwrap()
        .fail_after = Some(20);
    let reports = Reports::default();
    let subscriber = tracing::Dispatch::new(tracing_subscriber::registry().with(reports.clone()));
    turn.turn(&mut writer)
        .with_subscriber(subscriber.clone())
        .await;
    assert!(
        turn.log.is_none(),
        "failed report append drops the poisoned sidecar"
    );
    assert!(!writer.live.load(Ordering::SeqCst));
    assert_eq!(
        opened.load(Ordering::SeqCst),
        0,
        "report failure prevents transport opening"
    );
    assert_eq!(io.posts.load(Ordering::SeqCst), 0);
    assert_eq!(
        o_piece_attempts::attempts(&pool, &ready)
            .await
            .unwrap()
            .len(),
        1
    );
    let events = reports.0.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert!(events[0]["message"].contains("failed"));
    assert!(events[0].contains_key("error"));
    assert!(
        !events[0].contains_key("count"),
        "failure is never a successful zero report"
    );
    let reopened = ProvenanceLog::open(&path).unwrap();
    assert!(!reopened.state().reported(0) && !reopened.state().reported(1));
    drop(reopened);
    turn.turn(&mut writer).with_subscriber(subscriber).await;
    assert_eq!(
        io.posts.load(Ordering::SeqCst),
        1,
        "reopened report succeeds before ready dispatch"
    );
    let reopened = ProvenanceLog::open(&path).unwrap();
    assert!(reopened.state().reported(0) && reopened.state().reported(1));
    assert_eq!(
        reports.0.lock().unwrap().len(),
        2,
        "one failure, then one nonzero report"
    );
    pool.close().await;
    db.drop().await;
}
