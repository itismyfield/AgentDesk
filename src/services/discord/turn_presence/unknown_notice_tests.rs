use super::*;

fn observation(
    identity: ExecutionIdentity,
    label: Option<&str>,
    reason: &'static str,
    leaf: Option<&'static str>,
) -> NoticeObservation {
    NoticeObservation {
        identity,
        target_label: label.map(str::to_string),
        reason,
        hold_leaf: leaf,
        progress: false,
    }
}

fn nonce(value: &str, label: &str) -> NoticeObservation {
    observation(
        ExecutionIdentity::Nonce(value.into()),
        Some(label),
        "binding_unreadable",
        None,
    )
}

fn isolated(channel: u64) -> (tempfile::TempDir, Arc<Owner>) {
    let root = tempfile::tempdir().unwrap();
    let owner = Owner::get(Scope {
        root: Some(root.path().to_owned()),
        provider: "claude".into(),
        token_hash: "notice-test-token".into(),
        channel,
    });
    (root, owner)
}

type Sends = Arc<Mutex<Vec<String>>>;

async fn update(owner: &Arc<Owner>, observed: Option<NoticeObservation>, sends: &Sends) {
    if owner.observe(observed, true, None, Instant::now()) {
        let sends = sends.clone();
        owner
            .drive(move |reason| async move {
                lock(&sends).push(reason);
                Ok(())
            })
            .await;
    }
}

async fn first_notice(owner: &Arc<Owner>, observed: &NoticeObservation, sends: &Sends) {
    update(owner, Some(observed.clone()), sends).await;
    assert!(lock(sends).is_empty());
    tokio::time::advance(Duration::from_secs(61)).await;
    update(owner, Some(observed.clone()), sends).await;
    assert_eq!(lock(sends).len(), 1);
    assert!(owner.scope.load().unwrap().is_some());
}

#[tokio::test(start_paused = true)]
async fn legacy_same_label_nonce_enriches_the_success_receipt_without_another_send() {
    let (_root, owner) = isolated(7_180_101);
    let sends = Sends::default();
    let legacy = observation(
        ExecutionIdentity::LegacySession("L1".into()),
        Some("L1"),
        "binding_unreadable",
        None,
    );
    first_notice(&owner, &legacy, &sends).await;
    let old = owner.scope.load().unwrap().unwrap();
    let epoch = lock(&owner.state).epoch;
    update(&owner, Some(nonce("A", "L1")), &sends).await;
    let enriched = owner.scope.load().unwrap().unwrap();
    assert_eq!(enriched.id, old.id);
    assert_eq!(enriched.posted_at, old.posted_at);
    assert_eq!(enriched.key.identity, ExecutionIdentity::Nonce("A".into()));
    assert_eq!(lock(&owner.state).epoch, epoch);
    tokio::time::advance(Duration::from_secs(3661)).await;
    update(&owner, Some(nonce("A", "L1")), &sends).await;
    assert_eq!(lock(&sends).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn legacy_different_label_nonce_replaces_before_identity_enrichment() {
    let (_root, owner) = isolated(7_180_102);
    let sends = Sends::default();
    let legacy = observation(
        ExecutionIdentity::LegacySession("L1".into()),
        Some("L1"),
        "binding_unreadable",
        None,
    );
    first_notice(&owner, &legacy, &sends).await;
    let old_epoch = lock(&owner.state).epoch;
    let old_receipt = owner.scope.load().unwrap().unwrap();
    update(
        &owner,
        Some(observation(
            ExecutionIdentity::Nonce("B".into()),
            None,
            "binding_unreadable",
            None,
        )),
        &sends,
    )
    .await;
    assert_eq!(
        owner.scope.load().unwrap().unwrap(),
        old_receipt,
        "a nonce without a validated target cannot enrich the legacy anchor"
    );
    assert_eq!(lock(&owner.state).epoch, old_epoch);
    assert_eq!(lock(&sends).len(), 1);
    let replacement = nonce("B", "L2");
    update(&owner, Some(replacement.clone()), &sends).await;
    assert!(lock(&owner.state).epoch > old_epoch);
    assert!(owner.posted_at().is_none());
    assert!(owner.scope.load().unwrap().is_none());
    tokio::time::advance(Duration::from_secs(59)).await;
    update(&owner, Some(replacement.clone()), &sends).await;
    assert_eq!(lock(&sends).len(), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    update(&owner, Some(replacement), &sends).await;
    assert_eq!(lock(&sends).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn nonce_temporarily_unavailable_preserves_the_same_execution_receipt() {
    let (_root, owner) = isolated(7_180_103);
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    first_notice(&owner, &actual, &sends).await;
    let old = owner.scope.load().unwrap().unwrap();
    let epoch = lock(&owner.state).epoch;
    update(
        &owner,
        Some(observation(
            ExecutionIdentity::Unresolved,
            None,
            actual.reason,
            None,
        )),
        &sends,
    )
    .await;
    tokio::time::advance(Duration::from_secs(3661)).await;
    update(&owner, Some(actual), &sends).await;
    assert_eq!(owner.scope.load().unwrap().unwrap(), old);
    assert_eq!(lock(&owner.state).epoch, epoch);
    assert_eq!(lock(&sends).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn actual_reason_or_hold_leaf_change_rearms_after_its_own_grace() {
    for (channel, previous, next) in [
        (
            7_180_104,
            nonce("A", "L1"),
            observation(
                ExecutionIdentity::Nonce("A".into()),
                Some("L1"),
                "facts_halted",
                None,
            ),
        ),
        (
            7_180_105,
            observation(
                ExecutionIdentity::Nonce("A".into()),
                Some("L1"),
                "session_identity_unresolved",
                Some("owner_unresolved"),
            ),
            observation(
                ExecutionIdentity::Nonce("A".into()),
                Some("L1"),
                "session_identity_unresolved",
                Some("hosted_active_or_unknown"),
            ),
        ),
    ] {
        let (_root, owner) = isolated(channel);
        let sends = Sends::default();
        first_notice(&owner, &previous, &sends).await;
        update(&owner, Some(next.clone()), &sends).await;
        assert!(owner.posted_at().is_none());
        tokio::time::advance(Duration::from_secs(59)).await;
        update(&owner, Some(next.clone()), &sends).await;
        assert_eq!(lock(&sends).len(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        update(&owner, Some(next), &sends).await;
        assert_eq!(lock(&sends).len(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn same_namespace_owners_share_one_reservation_and_other_tokens_are_independent() {
    let (_root, owner) = isolated(7_180_106);
    let same = Owner::get(owner.scope.clone());
    assert!(Arc::ptr_eq(&owner, &same));
    let mut other_scope = owner.scope.clone();
    other_scope.token_hash = "another-notice-token".into();
    let other = Owner::get(other_scope);
    assert!(!Arc::ptr_eq(&owner, &other));
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    update(&owner, Some(actual.clone()), &sends).await;
    update(&other, Some(actual.clone()), &sends).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(owner.observe(Some(actual.clone()), true, None, Instant::now()));
    assert!(!same.observe(Some(actual.clone()), true, None, Instant::now()));
    assert!(other.observe(Some(actual.clone()), true, None, Instant::now()));
    for reserved in [&owner, &other] {
        let sends = sends.clone();
        reserved
            .drive(move |reason| async move {
                lock(&sends).push(reason);
                Ok(())
            })
            .await;
    }
    assert_eq!(lock(&sends).len(), 2);
    assert!(owner.scope.load().unwrap().is_some());
    assert!(other.scope.load().unwrap().is_some());
    assert!(!same.observe(Some(actual), true, None, Instant::now()));
}

#[tokio::test(start_paused = true)]
async fn successful_http_with_save_failure_retries_only_the_dirty_receipt() {
    let (_root, owner) = isolated(7_180_107);
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    update(&owner, Some(actual.clone()), &sends).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(owner.observe(Some(actual.clone()), true, None, Instant::now()));
    let path = owner.scope.path().unwrap();
    let failed_path = path.clone();
    let sent = sends.clone();
    owner
        .drive(move |reason| async move {
            lock(&sent).push(reason);
            std::fs::create_dir_all(&failed_path).unwrap();
            Ok(())
        })
        .await;
    let receipt = lock(&owner.state)
        .receipt
        .clone()
        .expect("successful HTTP must be held in memory");
    assert!(lock(&owner.state).dirty);
    assert!(lock(&owner.state).store_error.is_some());
    assert_eq!(lock(&sends).len(), 1);
    std::fs::remove_dir(&path).unwrap();
    tokio::time::advance(Duration::from_secs(29)).await;
    update(&owner, Some(actual.clone()), &sends).await;
    assert!(!path.exists());
    tokio::time::advance(Duration::from_secs(1)).await;
    update(&owner, Some(actual), &sends).await;
    assert_eq!(owner.scope.load().unwrap(), Some(receipt));
    assert!(!lock(&owner.state).dirty);
    assert!(lock(&owner.state).store_error.is_none());
    assert_eq!(lock(&sends).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn old_http_success_after_known_cannot_resurrect_a_receipt_or_suppress_recurrence() {
    let (_root, owner) = isolated(7_180_108);
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    update(&owner, Some(actual.clone()), &sends).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(owner.observe(Some(actual.clone()), true, None, Instant::now()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let task = {
        let (owner, entered, resume, sends) = (
            owner.clone(),
            entered.clone(),
            resume.clone(),
            sends.clone(),
        );
        tokio::spawn(async move {
            owner
                .drive(move |reason| async move {
                    entered.notify_one();
                    resume.notified().await;
                    lock(&sends).push(reason);
                    Ok(())
                })
                .await;
        })
    };
    entered.notified().await;
    assert!(!owner.observe(None, true, None, Instant::now()));
    resume.notify_one();
    task.await.unwrap();
    assert_eq!(
        lock(&sends).len(),
        1,
        "the already-submitted HTTP succeeded"
    );
    assert!(owner.posted_at().is_none());
    assert!(owner.scope.load().unwrap().is_none());
    update(&owner, Some(actual.clone()), &sends).await;
    tokio::time::advance(Duration::from_secs(59)).await;
    update(&owner, Some(actual.clone()), &sends).await;
    assert_eq!(lock(&sends).len(), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    update(&owner, Some(actual), &sends).await;
    assert_eq!(lock(&sends).len(), 2);
}

#[derive(Clone, Default)]
struct HeartbeatCapture(Arc<Mutex<Vec<std::collections::BTreeMap<String, String>>>>);

#[derive(Default)]
struct HeartbeatFields(std::collections::BTreeMap<String, String>);

impl tracing::field::Visit for HeartbeatFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().into(), value.to_string());
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().into(), value.to_string());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for HeartbeatCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = HeartbeatFields::default();
        event.record(&mut fields);
        if fields
            .0
            .get("message")
            .is_some_and(|message| message.contains("[turn_presence] unknown_still_held"))
        {
            lock(&self.0).push(fields.0);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn heartbeat_keeps_canonical_periodic_evidence_while_discord_succeeds_only_once() {
    use tracing_subscriber::prelude::*;
    let (_root, owner) = isolated(7_180_109);
    let sends = Sends::default();
    let capture = HeartbeatCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    crate::logging::test_capture::pin_callsite_interest();
    let _capture = tracing::subscriber::set_default(subscriber);
    let actual = nonce("A", "L1");
    update(&owner, Some(actual.clone()), &sends).await;
    tokio::time::advance(Duration::from_secs(59)).await;
    owner.heartbeat(Instant::now());
    assert!(lock(&capture.0).is_empty());
    assert!(lock(&sends).is_empty());
    tokio::time::advance(Duration::from_secs(1)).await;
    update(&owner, Some(actual.clone()), &sends).await;
    owner.heartbeat(Instant::now());
    let mut progress = actual;
    progress.reason = "facts_resumed";
    progress.progress = true;
    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(1800)).await;
        update(&owner, Some(progress.clone()), &sends).await;
        owner.heartbeat(Instant::now());
    }
    let events = lock(&capture.0).clone();
    assert_eq!(events.len(), 3);
    for (index, elapsed) in [60, 1860, 3660].into_iter().enumerate() {
        let fields = &events[index];
        assert_eq!(
            fields.get("channel_id").map(String::as_str),
            Some("7180109")
        );
        assert!(
            !fields.contains_key("channel"),
            "canonical channel_id field is required"
        );
        assert_eq!(fields.get("provider").map(String::as_str), Some("claude"));
        assert_eq!(
            fields.get("reason").map(String::as_str),
            Some("binding_unreadable")
        );
        assert_eq!(fields.get("unknown_for_secs"), Some(&elapsed.to_string()));
        assert_eq!(fields.get("cause_for_secs"), Some(&elapsed.to_string()));
        assert_eq!(fields.get("notice_state").map(String::as_str), Some("sent"));
        assert_eq!(
            fields.get("observation_progress").map(String::as_str),
            Some(if index == 0 { "false" } else { "true" })
        );
        assert_eq!(
            fields.get("observed_reason").map(String::as_str),
            Some(if index == 0 {
                "binding_unreadable"
            } else {
                "facts_resumed"
            })
        );
    }
    assert_eq!(
        lock(&sends).len(),
        1,
        "periodic logs are independent of successful Discord sends"
    );
    assert!(owner.scope.load().unwrap().is_some());
}

#[tokio::test(start_paused = true)]
async fn unreadable_receipt_retries_after_30_seconds_and_clears_health_error_on_restore() {
    let (_root, original) = isolated(7_180_110);
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    first_notice(&original, &actual, &sends).await;
    let scope = original.scope.clone();
    let mut saved = scope.load().unwrap().unwrap();
    // Persisted UTC age is independent from this test's paused Tokio clock.
    saved.unknown_since = Utc::now() - chrono::Duration::seconds(120);
    saved.cause_since = saved.unknown_since;
    let old = Arc::downgrade(&original);
    drop(original);
    assert!(old.upgrade().is_none());
    let path = scope.path().unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let owner = Owner::get(scope.clone());
    update(&owner, Some(actual.clone()), &sends).await;
    let first_retry = lock(&owner.state).retry_at.unwrap();
    assert!(!lock(&owner.state).loaded);
    assert!(lock(&owner.state).store_error.is_some());
    assert!(owner.posted_at().is_none());
    tokio::time::advance(Duration::from_secs(29)).await;
    update(&owner, Some(actual.clone()), &sends).await;
    assert_eq!(lock(&owner.state).retry_at, Some(first_retry));
    tokio::time::advance(Duration::from_secs(1)).await;
    update(&owner, Some(actual.clone()), &sends).await;
    assert_eq!(lock(&owner.state).retry_at, Some(first_retry + RETRY));
    tokio::time::advance(Duration::from_secs(30)).await;
    update(&owner, Some(actual.clone()), &sends).await;
    let unhealthy = owner
        .snapshot(Instant::now())
        .expect("Unknown reached the health grace");
    assert_eq!(unhealthy.notice_state, "store_error");
    assert!(unhealthy.notice_store_error.is_some());
    assert_eq!(
        lock(&sends).len(),
        1,
        "load failure must not post an unrecorded duplicate"
    );
    std::fs::remove_dir(&path).unwrap();
    scope.save(Some(&saved)).unwrap();
    tokio::time::advance(Duration::from_secs(29)).await;
    update(&owner, Some(actual.clone()), &sends).await;
    assert!(
        !lock(&owner.state).loaded,
        "no early retry after repairing the file"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    update(&owner, Some(actual), &sends).await;
    let restored = owner
        .snapshot(Instant::now())
        .expect("restored UTC incident age remains degraded");
    assert_eq!(restored.notice_state, "sent");
    assert!(restored.notice_store_error.is_none());
    assert_eq!(owner.posted_at(), Some(saved.posted_at));
    assert_eq!(scope.load().unwrap(), Some(saved));
    assert_eq!(lock(&sends).len(), 1);
}

impl Owner {
    pub(in crate::services::discord::turn_presence::supervisor) async fn settled_for_tests(&self) {
        tokio::task::yield_now().await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !lock(&self.state).driving {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "notice drive did not settle within five wall-clock seconds"
            );
            tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(1)))
                .await
                .expect("notice settlement barrier worker");
            tokio::task::yield_now().await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn published_recovery_clears_a_corrupt_receipt_and_rearms_after_the_new_grace() {
    let (_root, owner) = isolated(7_180_111);
    let path = owner.scope.path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "corrupt receipt").unwrap();
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    update(&owner, Some(actual.clone()), &sends).await;
    assert!(!lock(&owner.state).loaded);
    assert!(lock(&owner.state).store_error.is_some());
    update(&owner, None, &sends).await;
    assert!(lock(&owner.state).loaded);
    assert!(lock(&owner.state).store_error.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), b"null");
    assert!(owner.scope.load().unwrap().is_none());
    update(&owner, Some(actual.clone()), &sends).await;
    tokio::time::advance(Duration::from_secs(59)).await;
    update(&owner, Some(actual.clone()), &sends).await;
    assert!(lock(&sends).is_empty());
    tokio::time::advance(Duration::from_secs(2)).await;
    update(&owner, Some(actual), &sends).await;
    assert_eq!(lock(&sends).len(), 1);
    assert!(owner.scope.load().unwrap().is_some());
}

#[tokio::test(start_paused = true)]
async fn late_http_cannot_suppress_a_changed_cause_or_a_recovered_same_cause() {
    for recovered in [false, true] {
        let (_root, owner) = isolated(if recovered { 7_180_114 } else { 7_180_113 });
        let sends = Sends::default();
        let actual = nonce("A", "L1");
        update(&owner, Some(actual.clone()), &sends).await;
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(owner.observe(Some(actual.clone()), true, None, Instant::now()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let task = {
            let (owner, entered, resume, sends) = (
                owner.clone(),
                entered.clone(),
                resume.clone(),
                sends.clone(),
            );
            tokio::spawn(async move {
                owner
                    .drive(move |reason| async move {
                        entered.notify_one();
                        resume.notified().await;
                        lock(&sends).push(reason);
                        Ok(())
                    })
                    .await;
            })
        };
        entered.notified().await;
        let mut next = actual;
        if recovered {
            assert!(!owner.observe(None, true, None, Instant::now()));
        } else {
            next.reason = "facts_halted";
        }
        assert!(!owner.observe(Some(next.clone()), true, None, Instant::now()));
        resume.notify_one();
        task.await.unwrap();
        assert!(
            owner.posted_at().is_none(),
            "an old success cannot belong to a newer notice epoch"
        );
        assert!(owner.scope.load().unwrap().is_none());
        assert_eq!(lock(&sends).len(), 1);
        tokio::time::advance(Duration::from_secs(59)).await;
        update(&owner, Some(next.clone()), &sends).await;
        assert_eq!(lock(&sends).len(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        update(&owner, Some(next.clone()), &sends).await;
        assert_eq!(lock(&sends).len(), 2);
        assert_eq!(owner.scope.load().unwrap().unwrap().key.reason, next.reason);
    }
}

struct StorePause {
    entered: std::sync::atomic::AtomicBool,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
}

static STORE_PAUSES: LazyLock<Mutex<HashMap<Scope, Arc<StorePause>>>> =
    LazyLock::new(Default::default);

pub(in crate::services::discord::turn_presence::supervisor) fn pause_store_for_tests(
    scope: &Scope,
) {
    let pause = lock(&STORE_PAUSES).remove(scope);
    if let Some(pause) = pause {
        pause
            .entered
            .store(true, std::sync::atomic::Ordering::Release);
        lock(&pause.resume)
            .recv_timeout(Duration::from_secs(10))
            .expect("notice store pause was not released");
    }
}

struct StorePauseGuard {
    scope: Scope,
    pause: Arc<StorePause>,
    resume: Option<std::sync::mpsc::Sender<()>>,
}

impl StorePauseGuard {
    fn arm(scope: &Scope) -> Self {
        let (send, receive) = std::sync::mpsc::channel();
        let pause = Arc::new(StorePause {
            entered: std::sync::atomic::AtomicBool::new(false),
            resume: Mutex::new(receive),
        });
        assert!(
            lock(&STORE_PAUSES)
                .insert(scope.clone(), pause.clone())
                .is_none()
        );
        Self {
            scope: scope.clone(),
            pause,
            resume: Some(send),
        }
    }

    async fn entered(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !self
            .pause
            .entered
            .load(std::sync::atomic::Ordering::Acquire)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "notice store never reached its pause"
            );
            tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(1)))
                .await
                .expect("notice store pause barrier worker");
            tokio::task::yield_now().await;
        }
    }

    fn release(&mut self) {
        self.resume
            .take()
            .expect("notice store pause already released")
            .send(())
            .expect("notice store pause receiver disappeared");
    }
}

impl Drop for StorePauseGuard {
    fn drop(&mut self) {
        let mut pauses = lock(&STORE_PAUSES);
        if pauses
            .get(&self.scope)
            .is_some_and(|pause| Arc::ptr_eq(pause, &self.pause))
        {
            pauses.remove(&self.scope);
        }
        drop(pauses);
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
        }
    }
}

fn drive_notice(owner: &Arc<Owner>, sends: &Sends) -> tokio::task::JoinHandle<()> {
    let (owner, sends) = (owner.clone(), sends.clone());
    tokio::spawn(async move {
        owner
            .drive(move |reason| async move {
                lock(&sends).push(reason);
                Ok(())
            })
            .await;
    })
}

#[tokio::test(start_paused = true)]
async fn old_receipt_write_then_recovery_clears_before_the_new_cause_succeeds() {
    let (_root, owner) = isolated(7_180_121);
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    update(&owner, Some(actual.clone()), &sends).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    let mut pause = StorePauseGuard::arm(&owner.scope);
    assert!(owner.observe(Some(actual.clone()), true, None, Instant::now()));
    let task = drive_notice(&owner, &sends);
    pause.entered().await;
    let old = lock(&owner.state)
        .receipt
        .clone()
        .expect("HTTP succeeded before the paused write");
    assert_eq!(lock(&sends).len(), 1);

    assert!(!owner.observe(None, true, None, Instant::now()));
    let mut next = actual;
    next.reason = "facts_halted";
    assert!(!owner.observe(Some(next.clone()), true, None, Instant::now()));
    pause.release();
    task.await.unwrap();
    assert!(
        owner.scope.load().unwrap().is_none(),
        "the old write must be followed by the pending clear"
    );
    assert!(owner.posted_at().is_none());

    tokio::time::advance(Duration::from_secs(59)).await;
    update(&owner, Some(next.clone()), &sends).await;
    assert_eq!(lock(&sends).len(), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    update(&owner, Some(next.clone()), &sends).await;
    let saved = owner.scope.load().unwrap().expect("new cause receipt");
    assert_eq!(saved.key.reason, next.reason);
    assert_ne!(saved.id, old.id);
    assert_eq!(*lock(&sends), ["binding_unreadable", "facts_halted"]);
}

#[tokio::test(start_paused = true)]
async fn old_clear_then_due_new_cause_preserves_the_latest_success_receipt() {
    let (_root, owner) = isolated(7_180_122);
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    first_notice(&owner, &actual, &sends).await;
    let old = owner.scope.load().unwrap().expect("old cause receipt");
    let mut pause = StorePauseGuard::arm(&owner.scope);
    assert!(owner.observe(None, true, None, Instant::now()));
    let task = drive_notice(&owner, &sends);
    pause.entered().await;
    assert_eq!(owner.scope.load().unwrap(), Some(old.clone()));

    let mut next = actual;
    next.reason = "facts_halted";
    assert!(!owner.observe(Some(next.clone()), true, None, Instant::now()));
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(!owner.observe(Some(next.clone()), true, None, Instant::now()));
    assert_eq!(
        lock(&sends).len(),
        1,
        "the newer send waits behind the old clear"
    );
    pause.release();
    task.await.unwrap();

    let saved = owner.scope.load().unwrap().expect("latest cause receipt");
    assert_eq!(saved.key.reason, next.reason);
    assert_ne!(saved.id, old.id);
    assert_eq!(owner.posted_at(), Some(saved.posted_at));
    assert_eq!(*lock(&sends), ["binding_unreadable", "facts_halted"]);
    tokio::time::advance(Duration::from_secs(3661)).await;
    update(&owner, Some(next), &sends).await;
    assert_eq!(owner.scope.load().unwrap(), Some(saved));
    assert_eq!(lock(&sends).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn receipt_loading_distinguishes_absence_from_invalid_schema_namespace_and_key() {
    let (_root, owner) = isolated(7_180_123);
    let sends = Sends::default();
    assert!(owner.scope.load().unwrap().is_none());
    first_notice(&owner, &nonce("A", "L1"), &sends).await;
    let valid = serde_json::to_value(owner.scope.load().unwrap().unwrap()).unwrap();
    let mut invalid = Vec::new();
    for (field, value) in [
        ("schema", serde_json::json!(2)),
        ("provider", serde_json::json!("codex")),
        ("token_hash", serde_json::json!("other")),
        ("channel", serde_json::json!(0)),
        ("id", serde_json::json!("not-a-receipt-id")),
        ("extra", serde_json::json!(true)),
    ] {
        let mut value_copy = valid.clone();
        value_copy[field] = value;
        invalid.push(value_copy);
    }
    for (field, value) in [
        ("reason", serde_json::json!("0_bad")),
        ("target_label", serde_json::json!("")),
        ("provisional", serde_json::json!(true)),
        ("identity", serde_json::json!({"Nonce":""})),
        (
            "identity",
            serde_json::json!({"LegacySession":"different-target"}),
        ),
        ("extra", serde_json::json!(true)),
    ] {
        let mut value_copy = valid.clone();
        value_copy["key"][field] = value;
        invalid.push(value_copy);
    }
    invalid.push(serde_json::Value::Array(
        [
            "schema",
            "provider",
            "token_hash",
            "channel",
            "key",
            "unknown_since",
            "cause_since",
            "posted_at",
            "id",
        ]
        .into_iter()
        .map(|field| valid[field].clone())
        .collect(),
    ));
    let mut sequence_key = valid.clone();
    sequence_key["key"] = serde_json::Value::Array(
        ["identity", "target_label", "reason", "leaf", "provisional"]
            .into_iter()
            .map(|field| valid["key"][field].clone())
            .collect(),
    );
    invalid.push(sequence_key);
    invalid.extend([
        serde_json::json!([]),
        serde_json::json!(0),
        serde_json::json!(true),
        serde_json::json!("text"),
    ]);
    let path = owner.scope.path().unwrap();
    for value in invalid {
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            owner.scope.load().is_err(),
            "malformed receipt was treated as a success: {value}"
        );
    }
    std::fs::write(&path, "{invalid JSON").unwrap();
    assert!(owner.scope.load().is_err());
    std::fs::remove_file(path).unwrap();
    assert!(owner.scope.load().unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn cleared_receipt_reload_does_not_suppress_the_next_unknown_spell() {
    let (_root, original) = isolated(7_180_124);
    let sends = Sends::default();
    let actual = nonce("A", "L1");
    first_notice(&original, &actual, &sends).await;
    update(&original, None, &sends).await;
    let scope = original.scope.clone();
    let cleared_bytes = std::fs::read(scope.path().unwrap()).unwrap();
    let old = Arc::downgrade(&original);
    drop(original);
    assert!(old.upgrade().is_none());
    let fresh = Owner::get(scope);
    update(&fresh, Some(actual.clone()), &sends).await;
    let armed_after_reload = fresh.posted_at().is_none();
    tokio::time::advance(Duration::from_secs(59)).await;
    update(&fresh, Some(actual.clone()), &sends).await;
    assert_eq!(lock(&sends).len(), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    update(&fresh, Some(actual), &sends).await;
    assert_eq!(
        lock(&sends).len(),
        2,
        "recovery must permit exactly one success in the next spell after reload"
    );
    assert_eq!(cleared_bytes, b"null");
    assert!(armed_after_reload);
    assert!(fresh.scope.load().unwrap().is_some());
}
