use super::*;

// Yield runnable workers without allowing the paused clock to jump to another tick.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn panicked_channel_respawns_on_next_tick_without_duplicating_healthy_worker() {
    let channel = 6_209_001;
    let healthy = channel + 1;
    let fake = Fake::new(&[channel, healthy], BUSY);
    let reads = Arc::new(Mutex::new(Vec::new()));
    let seen = reads.clone();
    *lock(&fake.reader) = Some(Arc::new(move |current| {
        let first = {
            let mut seen = lock(&seen);
            let first = !seen.contains(&current);
            seen.push(current);
            first
        };
        assert!(!(current == channel && first), "injected worker panic");
        Reading::unwatched_for_tests(BUSY, Some("pane"), Some("/t.jsonl"))
    }));
    let run = supervise(&fake);
    settle().await;
    let mut initial = lock(&reads).clone();
    initial.sort_unstable();
    assert_eq!(initial, [channel, healthy]);
    tokio::time::advance(TICK - Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(lock(&reads).len(), 2, "no restart before the tick");
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    let seen = lock(&reads).clone();
    assert_eq!(seen.iter().filter(|&&c| c == channel).count(), 2);
    assert_eq!(seen.iter().filter(|&&c| c == healthy).count(), 2);
    assert_eq!(fake.typed_secs(channel), [TICK.as_secs()]);
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn repeated_worker_panics_restart_only_once_per_tick() {
    let channel = 6_209_003;
    let fake = Fake::new(&[channel], BUSY);
    let reads = Arc::new(Mutex::new(Vec::new()));
    let seen = reads.clone();
    let start = fake.start;
    *lock(&fake.reader) = Some(Arc::new(move |_| {
        lock(&seen).push(Instant::now().duration_since(start));
        panic!("injected repeated worker panic");
    }));
    let run = supervise(&fake);
    settle().await;
    for tick in 1..=4 {
        tokio::time::advance(TICK - Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(lock(&reads).len(), tick, "no restart before the tick");
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(lock(&reads).len(), tick + 1, "restart on every tick");
    }
    assert_eq!(*lock(&reads), (0..=4).map(|n| TICK * n).collect::<Vec<_>>());
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn removed_channel_does_not_respawn_after_worker_panic() {
    let channel = 6_209_004;
    let fake = Fake::new(&[channel], BUSY);
    let reads = Arc::new(Mutex::new(0));
    let seen = reads.clone();
    *lock(&fake.reader) = Some(Arc::new(move |_| {
        *lock(&seen) += 1;
        panic!("injected removed worker panic");
    }));
    let run = supervise(&fake);
    settle().await;
    assert_eq!(*lock(&reads), 1);
    let original = lock(&PRESENCE)[&channel].clone();
    lock(&fake.channels).clear();
    for _ in 0..3 {
        tokio::time::advance(TICK).await;
        settle().await;
        assert_eq!(*lock(&reads), 1, "removed channel must stay stopped");
    }
    assert!(Arc::ptr_eq(&original, &lock(&PRESENCE)[&channel]));
    assert!(fake.typed_secs(channel).is_empty());
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn normal_worker_completion_reaps_only_its_channel() {
    let channel = 6_209_005;
    let healthy = channel + 1;
    let fake = Fake::new(&[channel, healthy], BUSY);
    let mut supervisor = Supervisor::new(fake);
    supervisor.presence(channel);
    let original = supervisor.presence(healthy);
    let mut workers = JoinSet::new();
    workers.spawn(async move { (channel, Ok(())) });
    workers.spawn(std::future::pending());
    settle().await;
    supervisor.reap_workers(&mut workers);
    assert!(!supervisor.channels.contains_key(&channel));
    assert!(Arc::ptr_eq(&original, &supervisor.channels[&healthy]));
    assert_eq!(workers.len(), 1, "the running worker is not reaped");
}

#[tokio::test(start_paused = true)]
async fn removing_and_restoring_live_channel_keeps_its_existing_worker() {
    let channel = 6_209_007;
    let fake = Fake::new(&[channel], BUSY);
    let reads = Arc::new(Mutex::new(0));
    let seen = reads.clone();
    *lock(&fake.reader) = Some(Arc::new(move |_| {
        *lock(&seen) += 1;
        Reading::unwatched_for_tests(BUSY, Some("pane"), Some("/t.jsonl"))
    }));
    let run = supervise(&fake);
    settle().await;
    assert_eq!(*lock(&reads), 1);
    let original = lock(&PRESENCE)[&channel].clone();
    lock(&fake.channels).clear();
    tokio::time::advance(TICK).await;
    settle().await;
    assert_eq!(*lock(&reads), 1);
    assert!(Arc::ptr_eq(&original, &lock(&PRESENCE)[&channel]));
    lock(&fake.channels).push(channel);
    tokio::time::advance(TICK).await;
    settle().await;
    assert_eq!(
        *lock(&reads),
        2,
        "exactly one existing worker resumes reads"
    );
    assert!(Arc::ptr_eq(&original, &lock(&PRESENCE)[&channel]));
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
}
