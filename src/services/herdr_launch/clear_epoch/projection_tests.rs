use super::*;

struct Release(Option<std::sync::mpsc::Sender<()>>);

impl Release {
    fn now(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        self.now();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clear_waits_for_pane_projection_after_authorized_create_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    let owner = owner(CHANNEL);
    let wait = std::time::Duration::from_secs(10);
    let (projecting, arrived) = std::sync::mpsc::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    let mut release = Release(Some(resume));
    let resumed = Mutex::new(resumed);
    let _watch =
        clear_epoch::tests::watch(&owner, clear_epoch::tests::Point::Projecting, move || {
            projecting.send(()).unwrap();
            resumed
                .lock()
                .unwrap()
                .recv_timeout(wait)
                .expect("projection barrier is released");
        });
    let host = Arc::new(FakeHost::created("projected-pane"));
    let task_host = host.clone();
    let task_pool = pool.clone();
    let launch = tokio::spawn(async move {
        launch_herdr_session(&task_pool, launch(Some(endpoint())), command, task_host).await
    });
    arrived
        .recv_timeout(wait)
        .expect("create returned before the pane projection");
    assert_eq!(host.creates.load(Ordering::SeqCst), 1);
    let HostedRecord::Known(pending) = decoded(stored(&pool).await) else {
        panic!("missing Pending")
    };
    assert_eq!(pending.state, HostedState::Pending);
    assert!(pending.location.is_none(), "the pane is not projected yet");
    let clear = Arc::new(clear_epoch::begin_clear(
        &owner.provider,
        &owner.discord_token_hash,
        owner.channel_id.parse().unwrap(),
    ));
    let (polled, first_poll) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        let idle = clear.wait_idle();
        tokio::pin!(idle);
        let pending = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(idle.as_mut(), cx).is_pending())
        })
        .await;
        let _ = polled.send(pending);
        if pending {
            idle.await;
        }
    });
    let first_pending = tokio::time::timeout(wait, first_poll)
        .await
        .expect("the independent clear waiter was polled")
        .unwrap();
    assert!(
        first_pending,
        "clear must remain pending after I/O until pane projection finishes"
    );
    assert!(!waiter.is_finished());
    release.now();
    assert!(
        tokio::time::timeout(wait, launch)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    tokio::time::timeout(wait, waiter)
        .await
        .expect("clear observes launch completion")
        .unwrap();
    let HostedRecord::Known(projected) = decoded(stored(&pool).await) else {
        panic!("missing projection")
    };
    assert_eq!(projected.execution_nonce, pending.execution_nonce);
    assert_eq!(projected.location.unwrap().pane_id, "projected-pane");
}
