use super::*;

/// The startup history succeeds; only later full-history reads fail until released.
struct LegacyReads {
    bindings: Arc<FakeBindings>,
    released: AtomicBool,
    full_reads: AtomicUsize,
    calls: Mutex<Vec<(u64, bool)>>,
}

impl LegacyReads {
    fn new(bindings: Arc<FakeBindings>) -> Self {
        Self {
            bindings,
            released: AtomicBool::new(false),
            full_reads: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl BindingEvents for LegacyReads {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        let blocked = after == 0
            && self.full_reads.fetch_add(1, Ordering::SeqCst) > 0
            && !self.released.load(Ordering::SeqCst);
        let result = if blocked {
            Err("legacy history unavailable".into())
        } else {
            self.bindings.binding_events_since(channel, after)
        };
        self.calls.lock().unwrap().push((after, result.is_ok()));
        result
    }

    fn subscribe(&self, channel: u64) -> watch::Receiver<u64> {
        self.bindings.subscribe(channel)
    }
}

fn checkpoint(harness: &Harness) -> Option<u64> {
    harness.channel().binding_checkpoint().unwrap()
}

fn outage(checkpoint: u64, detail: &str) -> WriterAlarm {
    WriterAlarm::BindingLogUnavailable {
        checkpoint: Some(checkpoint),
        detail: detail.into(),
    }
}

#[tokio::test(start_paused = true)]
async fn legacy_hop_retry_holds_checkpoint_delivers_tails_and_recovers_once_per_outage() {
    let Legacy {
        harness,
        a_path,
        a,
        b,
        bindings,
    } = legacy_rotation(|a, b| rotate(2, a, b, BindingCause::Clear));
    let reads = Arc::new(LegacyReads::new(bindings.clone()));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, reads.clone());
    polls(3).await;
    assert_eq!(*reads.calls.lock().unwrap(), [(0, true), (0, false)]);
    assert_eq!(checkpoint(&harness), Some(2));
    assert_eq!(successor(&harness, &a).proof, None);
    assert_eq!(harness.port.posts(), ["a tail", "b first"]);
    let first_alarms = harness.alarms.taken();

    let (c_path, c) = transcript(&a_path, "c.jsonl", "s3", &row("k1", "c first"));
    bindings.commit(resumed(3, "tmux", &b));
    bindings.commit(rotate(4, &b, &c, BindingCause::Clear));
    append(&a_path, &row("m2", "a during outage"));
    polls(4).await;
    assert_eq!(
        checkpoint(&harness),
        Some(2),
        "unread history fences new events"
    );
    assert_eq!(successor(&harness, &a).proof, None);
    assert_eq!(first_alarms, [outage(2, "legacy history unavailable")]);
    assert!(
        harness.channel().cursor(&c).is_none(),
        "new bind is deferred"
    );
    assert_eq!(
        harness.port.posts(),
        ["a tail", "b first", "a during outage"]
    );
    assert!(!retired(&harness, &a));
    let calls = reads.calls.lock().unwrap().clone();
    assert!(
        calls.len() >= 5,
        "multiple actor polls retry the legacy read: {calls:?}"
    );
    assert!(calls[1..].iter().all(|call| *call == (0, false)));
    assert_eq!(
        harness.alarms.taken(),
        [],
        "one alarm for the continuing outage"
    );

    reads.released.store(true, Ordering::SeqCst);
    polls(3).await;
    assert_eq!(checkpoint(&harness), Some(4));
    assert_eq!(successor(&harness, &a).proof, Some(3));
    assert_eq!(
        harness.channel().cursor(&c).unwrap().captured_through,
        len(&c_path)
    );
    assert_eq!(
        harness.port.posts(),
        ["a tail", "b first", "a during outage", "c first"]
    );
    bindings.commit(resumed(5, "tmux", &c));
    polls(2).await;
    assert_eq!(checkpoint(&harness), Some(5));
    assert_eq!(
        successor(&harness, &a).proof,
        Some(3),
        "first proof is retained"
    );
    assert_eq!(harness.alarms.taken(), []);

    bindings.fail(Some("second outage"));
    bindings.commit(resumed(6, "tmux", &c));
    append(&c_path, &row("k2", "c during outage"));
    polls(4).await;
    assert_eq!(checkpoint(&harness), Some(5));
    assert_eq!(harness.port.posts().last().unwrap(), "c during outage");
    assert_eq!(harness.alarms.taken(), [outage(5, "second outage")]);
    polls(3).await;
    assert_eq!(harness.alarms.taken(), []);
    bindings.fail(None);
    polls(3).await;
    assert_eq!(checkpoint(&harness), Some(6));
    assert_eq!(successor(&harness, &a).proof, Some(3));
    halt(stop, task).await;

    let posts = harness.port.posts();
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, reads.clone());
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        posts,
        "restart replays no delivered units"
    );
    assert_eq!(successor(&harness, &a).proof, Some(3));
    let store = harness.channel();
    assert_eq!(store.cursors().count(), 3);
    assert_eq!(store.rotation().unwrap().links.len(), 2);
    drop(store);
    append(&c_path, &row("k3", "c after restart"));
    polls(3).await;
    assert_eq!(harness.port.posts().len(), posts.len() + 1);
    assert_eq!(harness.port.posts().last().unwrap(), "c after restart");
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn legacy_hop_retry_without_legacy_successors_does_not_read_extra_history() {
    let (harness, path, a, bindings) = started(&row("m0", "before"));
    harness.channel().set_binding_checkpoint(1).unwrap();
    let reads = Arc::new(LegacyReads::new(bindings.clone()));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, reads.clone());
    polls(2).await;
    assert_eq!(reads.full_reads.load(Ordering::SeqCst), 1);
    reads.released.store(true, Ordering::SeqCst);
    let (_, b) = transcript(&path, "b.jsonl", "s2", &row("n1", "new first"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    polls(3).await;
    assert_eq!(reads.full_reads.load(Ordering::SeqCst), 2);
    assert_eq!(checkpoint(&harness), Some(2));
    assert_eq!(harness.port.posts(), ["new first"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn legacy_hop_retry_readable_history_without_a_matching_hop_does_not_block() {
    let Legacy {
        harness,
        a_path,
        a,
        b,
        bindings,
    } = legacy_rotation(|_, _| {
        event(
            2,
            BindingRecord::Rejected {
                detail: "no hop".into(),
            },
            Utc::now(),
        )
    });
    let reads = Arc::new(LegacyReads::new(bindings.clone()));
    reads.released.store(true, Ordering::SeqCst);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, reads.clone());
    polls(3).await;
    assert_eq!(reads.full_reads.load(Ordering::SeqCst), 2);
    let (_, c) = transcript(&a_path, "c.jsonl", "s3", &row("k1", "new bind"));
    bindings.commit(resumed(3, "tmux", &b));
    bindings.commit(hop_at(4, "other", None, &c, BindingCause::Startup, ""));
    polls(3).await;
    assert_eq!(checkpoint(&harness), Some(4));
    assert_eq!(successor(&harness, &a).proof, None);
    assert!(!retired(&harness, &a));
    assert_eq!(reads.full_reads.load(Ordering::SeqCst), 3);
    assert_eq!(harness.port.posts(), ["a tail", "b first", "new bind"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn legacy_hop_retry_tail_after_durable_proof_invalidates_previous_eof() {
    let (harness, path, a, bindings) = started(&row("m0", "before"));
    let (_, b) = transcript(&path, "b.jsonl", "s2", &row("n1", "b first"));
    bindings.commit(hop_at(2, "tmux", Some(&a), &b, BindingCause::Startup, ""));
    let (mut sources, mut writer, mut deriver, mut owed) =
        reader_engine(&harness, bindings.clone(), None);
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    sources.tend(&mut writer).unwrap();
    assert_eq!(
        writer.store().cursor(&a).unwrap().captured_through,
        len(&path)
    );
    while let Some(item) = owed.pop_front() {
        writer.deliver(&item).await;
    }

    bindings.commit(resumed(3, "tmux", &b));
    sources.follow(&mut writer).unwrap();
    assert_eq!(
        successor(&harness, &a).proof,
        Some(3),
        "proof is durable before tail append"
    );
    let mut split = row("m1", "a split after proof");
    split.pop();
    append(&path, &split);
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    sources.tend(&mut writer).unwrap();
    tokio::time::advance(std::time::Duration::from_secs(15)).await;
    sources.tend(&mut writer).unwrap();
    assert!(
        !retired(&harness, &a),
        "prior EOF cannot retire the partial tail"
    );
    assert!(owed.is_empty());
    assert_eq!(harness.port.posts(), ["b first"]);
    append(&path, b"\n");
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    while let Some(item) = owed.pop_front() {
        writer.deliver(&item).await;
    }
    assert_eq!(harness.port.posts(), ["b first", "a split after proof"]);
    sources.tend(&mut writer).unwrap();
    tokio::time::advance(std::time::Duration::from_secs(12)).await;
    sources.tend(&mut writer).unwrap();
    assert!(retired(&harness, &a));
}
