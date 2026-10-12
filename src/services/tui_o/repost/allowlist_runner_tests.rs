use super::*;

#[tokio::test]
async fn d4b_channel_turn_exclude_then_readd_keeps_probe_and_budget_pg() {
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
    let probe = ready_probe(&pool, &key, &io).await;
    let config = |include| {
        let mut config = crate::config::Config::default();
        config.tui_o = Some(crate::services::tui_o::shadow::tap::TuiOConfig {
            repost: super::super::super::config::RepostConfig {
                enabled: true,
                channels: if include {
                    [CHANNEL].into_iter().collect()
                } else {
                    Default::default()
                },
            },
            ..Default::default()
        });
        Arc::new(config)
    };
    let (tx, rx) = tokio::sync::watch::channel(Some(config(true)));
    let opened = Arc::new(AtomicUsize::new(0));
    let path = dir.path().join("actual-turn.jsonl");
    let mut turn = ChannelTurn::new(
        Runner::new(RepostSwitch::new(None, rx), pool.clone(), BOT, run()),
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
    let sidecar_before = std::fs::read(&path).unwrap();
    turn.runner.probes.insert(key.clone(), probe);
    let before = o_piece_attempts::attempts(&pool, &key).await.unwrap();
    let reads = io.reader.reads();
    tx.send_replace(Some(config(false)));
    turn.turn(&mut writer).await;
    assert_eq!(std::fs::read(&path).unwrap(), sidecar_before);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    assert_eq!(io.reader.reads(), reads);
    assert_eq!(io.posts.load(Ordering::SeqCst), 0);
    assert_eq!(
        o_piece_attempts::attempts(&pool, &key).await.unwrap(),
        before
    );
    tx.send_replace(Some(config(true)));
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
