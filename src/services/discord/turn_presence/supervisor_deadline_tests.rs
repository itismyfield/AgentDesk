use super::*;

fn successes(fake: &Fake, channel: u64) -> Vec<Duration> {
    lock(&fake.succeeded)
        .iter()
        .filter_map(|&(c, at)| (c == channel).then_some(at))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn slow_activity_reads_do_not_starve_successful_typing_deadlines() {
    let channel = 6_200_017;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    let run = supervise(&fake);
    wait(1).await;
    lock(&fake.read_delay).insert(channel, Duration::from_secs(20));
    wait(TYPING_EVERY.as_secs() * 2).await;
    run.abort();
    assert_eq!(
        successes(&fake, channel),
        [0, 1, 2].map(|n| TYPING_EVERY * n)
    );
    assert_eq!(typing_counts(channel), (3, 0));
}

#[tokio::test(start_paused = true)]
async fn a_slow_channel_does_not_delay_another_channels_idle_or_generation() {
    let (slow, fast) = (6_200_018, 6_200_019);
    let _confirmed = (TestConfirmation::new(slow), TestConfirmation::new(fast));
    let fake = Fake::new(&[slow, fast], BUSY);
    let run = supervise(&fake);
    wait(1).await;
    lock(&fake.read_delay).insert(slow, Duration::from_secs(20));
    wait(9).await;
    fake.set(fast, IDLE);
    wait(3).await;
    assert_eq!(status(fast).unwrap()["activity"], "idle");
    fake.set(fast, BUSY);
    wait(TYPING_EVERY.as_secs() * 2 + 1).await;
    run.abort();
    assert_eq!(
        successes(&fake, slow),
        (0..=fake.since().as_secs() / TYPING_EVERY.as_secs())
            .map(|n| Duration::from_secs(TYPING_EVERY.as_secs() * n))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        successes(&fake, fast),
        [
            Duration::ZERO,
            TYPING_EVERY,
            Duration::from_secs(15),
            Duration::from_secs(15) + TYPING_EVERY
        ]
    );
    assert_eq!(status(fast).unwrap()["generation"], 3);
    assert_eq!(typing_counts(fast), (4, 0));
}

#[tokio::test(start_paused = true)]
async fn exactly_ten_seconds_between_successes_is_accepted_without_overlapping_sends() {
    let channel = 6_200_020;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    let run = supervise(&fake);
    wait(1).await;
    lock(&fake.typing_delay).insert(channel, Duration::from_secs(10) - TYPING_EVERY);
    wait(8).await;
    assert_eq!(typing_counts(channel), (1, 0));
    wait(2).await;
    run.abort();
    let times = successes(&fake, channel);
    assert_eq!(times, [0, 10].map(Duration::from_secs));
    assert!(
        times
            .windows(2)
            .all(|p| p[1] - p[0] <= Duration::from_secs(10))
    );
    assert_eq!(fake.typed_secs(channel), [0, TYPING_EVERY.as_secs()]);
    assert_eq!(typing_counts(channel), (2, 0));
}

#[tokio::test(start_paused = true)]
async fn three_second_http_delay_keeps_same_busy_generation_success_gaps_within_ten_seconds() {
    let channel = 6_200_024;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    let run = supervise(&fake);
    wait(1).await;
    assert_eq!(successes(&fake, channel), [Duration::ZERO]);
    let generation = status(channel).unwrap()["generation"].clone();
    lock(&fake.typing_delay).insert(channel, Duration::from_secs(3));
    wait(30).await;
    let times = successes(&fake, channel);
    assert!(
        times.len() >= 3,
        "must observe consecutive HTTP completions"
    );
    assert_eq!(status(channel).unwrap()["activity"], "busy");
    assert_eq!(status(channel).unwrap()["generation"], generation);
    assert!(
        times
            .windows(2)
            .all(|p| p[1] - p[0] <= Duration::from_secs(10)),
        "HTTP completion gaps exceed ten seconds: {times:?}"
    );
    assert_eq!(times[1], TYPING_EVERY + Duration::from_secs(3));
    fake.set(channel, IDLE);
    wait(4).await;
    assert_eq!(status(channel).unwrap()["activity"], "idle");
    let stopped = (fake.typed_secs(channel), successes(&fake, channel));
    wait(30).await;
    run.abort();
    assert_eq!(
        (fake.typed_secs(channel), successes(&fake, channel)),
        stopped
    );
    assert_eq!(typing_counts(channel), (stopped.1.len() as u64, 0));
}

#[tokio::test(start_paused = true)]
async fn idle_fences_a_due_mailbox_wait_and_old_generation_cannot_restart_typing() {
    let channel = 6_200_021;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    let mut supervisor = Supervisor::new(fake.clone());
    let presence = supervisor.presence(channel);
    let supervisor = Arc::new(supervisor);
    supervisor.tick(Instant::now()).await;
    wait(TYPING_EVERY.as_secs()).await;
    lock(&fake.mailbox_delay).insert(channel, Duration::from_secs(5));
    let due = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.send_due(Instant::now()).await }
    });
    tokio::task::yield_now().await;
    assert!(!lock(&fake.mailbox_delay).contains_key(&channel));
    fake.set(channel, IDLE);
    supervisor.tick(Instant::now()).await;
    assert_eq!(status(channel).unwrap()["activity"], "idle");
    assert_eq!(successes(&fake, channel), [Duration::ZERO]);
    fake.set(channel, BUSY);
    supervisor.tick(Instant::now()).await;
    wait(5).await;
    due.await.unwrap();
    assert_eq!(successes(&fake, channel), [Duration::ZERO, TYPING_EVERY]);
    assert_eq!(
        lock(&presence).next_typing_at,
        Some(fake.start + TYPING_EVERY * 2)
    );
    wait(TYPING_EVERY.as_secs() - 5).await;
    supervisor.send_due(Instant::now()).await;
    wait(1).await;
    assert_eq!(
        successes(&fake, channel),
        [0, 1, 2].map(|n| TYPING_EVERY * n)
    );
    assert_eq!(status(channel).unwrap()["generation"], 3);
    assert_eq!(typing_counts(channel), (3, 0));
}

#[tokio::test(start_paused = true)]
async fn a_429_keeps_the_retry_backoff_and_idle_cancels_it() {
    let (retry, stop) = (6_200_022, 6_200_023);
    let _confirmed = (TestConfirmation::new(retry), TestConfirmation::new(stop));
    let fake = Fake::new(&[retry, stop], BUSY);
    lock(&fake.typing_errors).extend([(retry, "429".into()), (stop, "429".into())]);
    let run = supervise(&fake);
    wait(10).await;
    assert_eq!(typing_counts(retry), (0, 0));
    assert_eq!(status(retry).unwrap()["typing"]["last_error"], "429");
    fake.set(stop, IDLE);
    lock(&fake.typing_errors).clear();
    wait(19).await;
    assert_eq!(fake.typed_secs(retry), [0]);
    wait(2).await;
    run.abort();
    assert_eq!(fake.typed_secs(retry), [0, 30]);
    assert_eq!(fake.typed_secs(stop), [0]);
    assert_eq!(successes(&fake, retry), [Duration::from_secs(30)]);
    assert_eq!(typing_counts(retry), (1, 0));
}
