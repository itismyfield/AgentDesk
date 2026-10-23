use super::*;

/// A receipt can outlive the local ledger. A grant for that old payload still cannot POST it.
#[tokio::test]
async fn shared_tool_result_without_local_history_is_excluded_before_repost_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = PieceKey::new(
        UnitKey {
            kind: UnitKind::ToolResult,
            ..unit("legacy-shared")
        },
        0,
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut log = ProvenanceLog::open(&dir.path().join("s.jsonl")).unwrap();
    let original = OriginalAttempt {
        serial: 3,
        key: key.clone(),
        payload: "private tool traceback".into(),
        anchor: 40,
    };
    Admitter::new(&pool, "node-a", BOT)
        .adopt_uncertain(&mut log, &original)
        .await
        .unwrap();

    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    writer.set_live(true);
    let mut runner = Runner::new(pool.clone(), BOT, run_scope());
    let intent = evidence(&pool, &key).await;
    let Admission::Granted { permit, lease } =
        runner.try_next_dispatch(&mut writer, intent).await.unwrap()
    else {
        panic!("old shared receipt reaches the final writer gate");
    };
    let io = Io::over(&harness.port);
    let started = dispatch::start(&mut writer, permit, lease, &*io).await;
    started.finish(&mut writer, &pool).await.unwrap();
    assert_eq!(io.wire(), 0);
    assert!(harness.port.posts().is_empty());
    let ledger = writer.store().ledger();
    assert_eq!(
        ledger.excluded(key.unit()),
        Some(pieces::TOOL_RESULT_INTERNAL)
    );
    assert_eq!(
        ledger.next_serial(),
        0,
        "no Prepared for the private payload"
    );
    assert_eq!(ledger.violation(), None);
    pool.close().await;
    db.drop().await;
}
