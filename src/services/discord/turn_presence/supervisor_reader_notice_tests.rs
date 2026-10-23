use super::*;
use crate::services::discord::turn_presence::activity::tests::NoticeReaderProbe;

fn install_reader(fake: &Arc<Fake>, reader: &Arc<NoticeReaderProbe>) {
    let reader = reader.clone();
    *lock(&fake.reader) = Some(Arc::new(move |_| reader.read()));
}

fn manual(fake: &Arc<Fake>, channel: u64) -> Supervisor<Fake> {
    let mut inner = Supervisor::new(fake.clone());
    inner.presence(channel);
    inner
}

async fn tick(inner: &Supervisor<Fake>, channel: u64) {
    inner.tick(Instant::now()).await;
    let owner = lock(&inner.channels[&channel])
        .notice
        .clone()
        .expect("an accepted reader observation must install the notice owner");
    owner.settled_for_tests().await;
}

fn successes(fake: &Arc<Fake>) -> usize {
    lock(&fake.notice_ok).len()
}

fn receipt(fake: &Arc<Fake>, channel: u64) -> notice_store::Receipt {
    fake.notice_scope(channel)
        .load()
        .expect("notice store read")
        .expect("successful notice receipt must be durable")
}

async fn initial_failure(inner: &Supervisor<Fake>, reader: &NoticeReaderProbe, channel: u64) {
    tick(inner, channel).await;
    assert_eq!(reader.counts(), (0, 1));
    assert_eq!(reader.run_jobs(), 1);
    tick(inner, channel).await;
    assert_eq!(reader.counts(), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn actual_reader_122_retries_keep_the_first_grace_and_one_success_at_3661() {
    let channel = 6_218_711;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    *lock(&fake.head) = Some(7);
    let reader = Arc::new(NoticeReaderProbe::new(channel));
    install_reader(&fake, &reader);
    let inner = manual(&fake, channel);
    initial_failure(&inner, &reader, channel).await;
    let initial_counts = reader.counts();
    let times: BTreeSet<u64> = (30..=3660).step_by(30).chain([59, 61, 3661]).collect();
    let mut previous = 0;
    for at in times {
        tokio::time::advance(Duration::from_secs(at - previous)).await;
        previous = at;
        if at % 30 == 0 {
            reader.force_retry_due();
            tick(&inner, channel).await;
            assert_eq!(reader.run_jobs(), 1, "actual rebuild worker at t={at}");
        }
        tick(&inner, channel).await;
        if at == 59 {
            assert_eq!(successes(&fake), 0);
        } else if at >= 61 {
            assert_eq!(successes(&fake), 1, "same source failure at t={at}");
        }
    }
    let (folds, spawns) = reader.counts();
    assert_eq!(
        (folds - initial_counts.0, spawns - initial_counts.1),
        (122, 122)
    );
    assert_eq!((folds, spawns), (123, 123));
    assert_eq!(lock(&fake.notices).len(), 1);
    assert_eq!(receipt(&fake, channel).key.reason, "binding_unreadable");
}

#[tokio::test(start_paused = true)]
async fn fresh_reader_supervisor_and_owner_preserve_the_receipt_through_initial_rebuild() {
    let channel = 6_218_712;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    *lock(&fake.head) = Some(7);
    let reader = Arc::new(NoticeReaderProbe::new(channel));
    install_reader(&fake, &reader);
    let inner = manual(&fake, channel);
    initial_failure(&inner, &reader, channel).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 1);
    let before = receipt(&fake, channel);
    let before_bytes = std::fs::read(fake.notice_scope(channel).path().unwrap()).unwrap();
    let old_owner = Arc::downgrade(lock(&inner.channels[&channel]).notice.as_ref().unwrap());
    drop(inner);
    lock(&PRESENCE).remove(&channel);
    *lock(&fake.reader) = None;
    drop(reader);
    assert!(
        old_owner.upgrade().is_none(),
        "the restart must drop the old in-memory owner"
    );

    let fresh_reader = Arc::new(NoticeReaderProbe::new(channel));
    install_reader(&fake, &fresh_reader);
    let fresh = manual(&fake, channel);
    tick(&fresh, channel).await;
    assert_eq!(fresh_reader.counts(), (0, 1));
    assert_eq!(successes(&fake), 1);
    let loaded_owner = lock(&fresh.channels[&channel]).notice.clone().unwrap();
    assert_eq!(
        loaded_owner.posted_at(),
        Some(before.posted_at),
        "the fresh owner must actually load the receipt"
    );
    assert_eq!(receipt(&fake, channel), before);
    assert_eq!(
        std::fs::read(fake.notice_scope(channel).path().unwrap()).unwrap(),
        before_bytes
    );
    assert_eq!(fresh_reader.run_jobs(), 1);
    tick(&fresh, channel).await;
    assert_eq!(fresh_reader.counts(), (1, 1));
    tokio::time::advance(Duration::from_secs(3661)).await;
    tick(&fresh, channel).await;
    assert_eq!(successes(&fake), 1);
    assert_eq!(receipt(&fake, channel), before);
}

#[tokio::test(start_paused = true)]
async fn long_initial_rebuild_notices_then_first_actual_cause_enriches_and_later_cause_rearms() {
    let channel = 6_218_713;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    *lock(&fake.head) = Some(7);
    let reader = Arc::new(NoticeReaderProbe::new(channel));
    install_reader(&fake, &reader);
    let inner = manual(&fake, channel);
    tick(&inner, channel).await;
    assert_eq!(reader.counts(), (0, 1));
    tokio::time::advance(Duration::from_secs(59)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 0);
    tokio::time::advance(Duration::from_secs(2)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 1);
    let provisional = receipt(&fake, channel);
    assert!(provisional.key.provisional);
    assert_eq!(provisional.key.reason, "catching_up");
    assert_eq!(
        reader.counts(),
        (0, 1),
        "the initial worker remained blocked"
    );

    assert_eq!(reader.run_jobs(), 1);
    tick(&inner, channel).await;
    let actual = receipt(&fake, channel);
    assert!(!actual.key.provisional);
    assert_eq!(actual.key.reason, "binding_unreadable");
    assert_eq!(actual.id, provisional.id);
    assert_eq!(actual.posted_at, provisional.posted_at);
    assert_eq!(successes(&fake), 1);
    tokio::time::advance(Duration::from_secs(61)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 1, "first settled cause must only enrich");

    reader.pending();
    tick(&inner, channel).await;
    assert_eq!(reader.run_jobs(), 1);
    tick(&inner, channel).await;
    assert!(fake.notice_scope(channel).load().unwrap().is_none());
    tokio::time::advance(Duration::from_secs(59)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 2);
    assert_eq!(receipt(&fake, channel).key.reason, "binding_pending");
    assert_eq!(reader.counts(), (2, 2));
}

#[tokio::test(start_paused = true)]
async fn settled_failure_survives_rebuild_progress_but_actual_pending_opens_a_new_grace() {
    let channel = 6_218_714;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    *lock(&fake.head) = Some(7);
    let reader = Arc::new(NoticeReaderProbe::new(channel));
    install_reader(&fake, &reader);
    let inner = manual(&fake, channel);
    initial_failure(&inner, &reader, channel).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 1);
    let before = receipt(&fake, channel);

    reader.pending();
    tick(&inner, channel).await;
    assert_eq!(receipt(&fake, channel), before);
    assert_eq!(reader.run_jobs(), 1);
    tick(&inner, channel).await;
    assert!(fake.notice_scope(channel).load().unwrap().is_none());
    tokio::time::advance(Duration::from_secs(59)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    tick(&inner, channel).await;
    assert_eq!(successes(&fake), 2);
    let after = receipt(&fake, channel);
    assert_eq!(after.key.reason, "binding_pending");
    assert_ne!(after.id, before.id);
    assert_eq!(reader.counts(), (2, 2));
}
