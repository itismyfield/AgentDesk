use super::*;

#[tokio::test(start_paused = true)]
async fn constant_unknown_is_successfully_noticed_once_through_3661_seconds() {
    let channel = 6_218_701;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    *lock(&fake.head) = Some(7);
    let run = supervise(&fake);
    wait(59).await;
    assert!(lock(&fake.notice_ok).is_empty());
    wait(2).await;
    assert_eq!(lock(&fake.notice_ok).len(), 1);
    let notices = fake.notices();
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].1.starts_with("-# ⚠️ ")
            && crate::services::discord::session_notice::is_inline(&notices[0].1),
        "the supervisor Unknown notice must reach the provider effect as one subtext line: {}",
        notices[0].1
    );
    assert!(notices[0].1.contains("facts\\_halted"));
    wait(3600).await;
    run.abort();
    assert_eq!(
        lock(&fake.notice_ok).len(),
        1,
        "persistent cause must not repeat"
    );
}

#[tokio::test(start_paused = true)]
async fn recovered_unknown_can_notice_again_before_half_an_hour() {
    let channel = 6_218_702;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    *lock(&fake.head) = Some(7);
    let run = supervise(&fake);
    wait(61).await;
    fake.set(channel, IDLE);
    wait(4).await;
    fake.set(channel, UNKNOWN);
    wait(64).await;
    run.abort();
    assert_eq!(
        lock(&fake.notice_ok).len(),
        2,
        "recovery re-arms a new spell"
    );
}

#[tokio::test(start_paused = true)]
async fn failed_unknown_notice_retries_without_recording_success() {
    let channel = 6_218_703;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    *lock(&fake.head) = Some(7);
    fake.notice_error.store(true, Ordering::SeqCst);
    let run = supervise(&fake);
    wait(61).await;
    assert_eq!(fake.notice_secs(), [60]);
    assert!(lock(&fake.notice_ok).is_empty());
    assert!(status(channel).unwrap()["unknown_notice"]["posted_at"].is_null());
    fake.notice_error.store(false, Ordering::SeqCst);
    wait(33).await;
    run.abort();
    assert_eq!(lock(&fake.notice_ok).len(), 1, "failed send must retry");
}
