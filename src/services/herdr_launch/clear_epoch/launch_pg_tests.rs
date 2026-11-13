use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn clear_before_actual_launch_sends_no_create_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    let owner = owner(CHANNEL);
    let _clear = clear_epoch::begin_clear(
        &owner.provider,
        &owner.discord_token_hash,
        owner.channel_id.parse().unwrap(),
    );
    let host = Arc::new(FakeHost::created("clear-pane"));
    let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
    assert_eq!(
        host.creates.load(Ordering::SeqCst),
        0,
        "clear must precede physical create: {result:?}"
    );
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_after_launch_check_before_create_authorization_sends_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    let owner = owner(CHANNEL);
    let clear = Arc::new(Mutex::new(None));
    let saved = clear.clone();
    let key = owner.clone();
    let _watch = clear_epoch::tests::watch(&owner, clear_epoch::tests::Point::Checked, move || {
        *saved.lock().unwrap() = Some(clear_epoch::begin_clear(
            &key.provider,
            &key.discord_token_hash,
            key.channel_id.parse().unwrap(),
        ));
    });
    let host = Arc::new(FakeHost::created("never-created"));
    let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
    assert!(result.is_err());
    assert_eq!(host.creates.load(Ordering::SeqCst), 0);
    let HostedRecord::Known(record) = decoded(stored(&pool).await) else {
        panic!("missing retired row")
    };
    assert_eq!(record.state, HostedState::Retired);
    assert!(clear.lock().unwrap().as_ref().unwrap().overlapped_startup());
}

#[tokio::test(flavor = "multi_thread")]
async fn authorized_create_after_clear_still_blocks_clear_until_projection_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    let owner = owner(CHANNEL);
    let (authorized, observed) = std::sync::mpsc::channel();
    let (release, resumed) = std::sync::mpsc::channel();
    let resumed = Mutex::new(resumed);
    let _watch =
        clear_epoch::tests::watch(&owner, clear_epoch::tests::Point::Authorized, move || {
            authorized.send(()).unwrap();
            resumed
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        });
    let host = Arc::new(FakeHost::created("authorized-pane"));
    let task_host = host.clone();
    let task_pool = pool.clone();
    let task = tokio::spawn(async move {
        launch_herdr_session(&task_pool, launch(Some(endpoint())), command, task_host).await
    });
    observed
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let clear = clear_epoch::begin_clear(
        &owner.provider,
        &owner.discord_token_hash,
        owner.channel_id.parse().unwrap(),
    );
    assert_eq!(
        host.creates.load(Ordering::SeqCst),
        0,
        "authorization precedes actual create"
    );
    let waiting = clear.wait_idle();
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            waiting.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    release.send(()).unwrap();
    assert!(task.await.unwrap().is_ok());
    waiting.await;
    assert_eq!(
        host.creates.load(Ordering::SeqCst),
        1,
        "the already authorized create may finish"
    );
    let HostedRecord::Known(record) = decoded(stored(&pool).await) else {
        panic!("missing projection")
    };
    assert_eq!(record.location.unwrap().pane_id, "authorized-pane");
}
