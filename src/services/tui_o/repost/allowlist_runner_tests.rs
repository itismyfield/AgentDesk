use super::*;

#[tokio::test]
async fn d4b_channel_turn_reopen_keeps_admission_and_budget_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::new(&dir);
    let mut seed = sidecar(&dir);
    let key = admit_on(&pool, &mut seed, &[(3, "allowlist-ready")], "piece")
        .await
        .pop()
        .unwrap();
    let io = Arc::new(Io::new());
    let opened = Arc::new(AtomicUsize::new(0));
    let path = dir.path().join("actual-turn.jsonl");
    let mut turn = ChannelTurn::new(
        Runner::new(pool.clone(), BOT, run()),
        {
            let (io, opened) = (Arc::clone(&io), Arc::clone(&opened));
            Box::new(move || {
                opened.fetch_add(1, Ordering::SeqCst);
                Some(Arc::clone(&io))
            })
        },
        CHANNEL,
        path.clone(),
    );
    turn.turn(&mut writer).await;
    assert!(path.exists());
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    assert_eq!(io.posts.load(Ordering::SeqCst), 0);
    let before = o_piece_attempts::attempts(&pool, &key).await.unwrap();
    turn.log = None;
    turn.turn(&mut writer).await;
    assert_eq!(
        o_piece_attempts::attempts(&pool, &key).await.unwrap(),
        before
    );
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    assert_eq!(io.posts.load(Ordering::SeqCst), 0);
    let reads = io.reader.reads();
    turn.runner
        .probes
        .insert(key.clone(), ready_probe(&pool, &key, &io).await);
    turn.turn(&mut writer).await;
    assert!(path.exists());
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    assert!(io.reader.reads() > reads);
    assert_eq!(io.posts.load(Ordering::SeqCst), 1);
    assert_eq!(writer.prepared, 1);
    assert_eq!(
        o_piece_attempts::attempts(&pool, &key).await.unwrap().len(),
        2
    );
    pool.close().await;
    db.drop().await;
}
