use super::*;

type Run = tokio::task::JoinHandle<Option<crate::services::tui_o::writer::deliver::StopCause>>;

fn begin(runtime: &std::path::Path, path: &std::path::Path, logical: &str) -> (OStore, impl Sized) {
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native", path).unwrap();
    let binding = bind(
        runtime,
        CHANNEL,
        &source,
        logical,
        "73320000000000000000000000000095",
    );
    let o = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime)
        .unwrap()
        .unwrap();
    era(&o, &source);
    (o, binding)
}

/// Runs the production actor loop with the production deriver until `stop` is sent.
fn spawn(o: &OStore, port: Arc<Port>) -> (watch::Sender<bool>, Run) {
    let store = o
        .open_channel(&o.read_era().unwrap().unwrap(), CHANNEL)
        .unwrap()
        .unwrap();
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let writer = ChannelWriter::new(store, gate, port, Lease, Alarms::default());
    let bindings = Arc::new(ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude));
    let (stop, stopped) = watch::channel(false);
    let run = tokio::spawn(super::super::super::run_channel_at(
        writer,
        ShadowProvider::Claude,
        bindings,
        stopped,
        chrono::Utc::now(),
    ));
    (stop, run)
}

async fn settle(port: &Port, posts: usize) {
    for _ in 0..3000 {
        if port.posts.lock().unwrap().len() >= posts {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // Further polls must not add anything once the expected POSTs landed.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
}

async fn finish((stop, run): (watch::Sender<bool>, Run)) {
    stop.send_replace(true);
    run.await.unwrap();
}

fn reopen(o: &OStore) -> crate::services::tui_o::store::ChannelStore {
    o.open_channel(&o.read_era().unwrap().unwrap(), CHANNEL)
        .unwrap()
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn first_activation_freezes_the_gap_before_resume_and_notices_each_new_input_once() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    let old = [row("old", "old input"), body("old-body", "old answer")].concat();
    std::fs::write(&path, &old).unwrap();
    let (o, _binding) = begin(runtime.path(), &path, "activation-pane");
    let port = Arc::new(Port::default());
    let legacy = |prompt: &str| {
        let mut messages = port.messages.lock().unwrap();
        let id = 1000 + messages.len() as u64;
        messages.push(SeenMessage {
            id,
            author_id: 42,
            content: crate::services::discord::input_notice_content(prompt).unwrap(),
        });
        id
    };
    legacy("old input");
    // The old hook announced a queued input while the previous turn ran, before its answer.
    let announced = legacy("announced gap input");
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    // The previous build captured without a notice projection.
    actor.deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    resume(&mut actor);
    actor.read_sources();
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    actor.deliver_owed(&progress).await;
    drop(actor);
    assert_eq!(*port.posts.lock().unwrap(), ["old answer"]);
    // The queued input's row lands after the old writer stopped; the next one had no hook at all.
    append(&path, &row("gap", "announced gap input"));
    append(&path, &row("silent", "silent gap input"));
    append(&path, &body("gap-body", "gap answer"));
    let frozen_upper = std::fs::metadata(&path).unwrap().len();

    let first = spawn(&o, port.clone());
    settle(&port, 3).await;
    append(&path, &row("after", "input after activation"));
    settle(&port, 4).await;
    finish(first).await;
    let posted = port.posts.lock().unwrap().clone();
    let notice = |prompt: &str| crate::services::discord::input_notice_content(prompt).unwrap();
    assert_eq!(posted.len(), 4, "{posted:?}");
    assert_eq!(posted[0], "old answer");
    assert_eq!(
        posted
            .iter()
            .filter(|p| **p == notice("silent gap input"))
            .count(),
        1
    );
    assert_eq!(posted.iter().filter(|p| **p == "gap answer").count(), 1);
    assert_eq!(
        posted
            .iter()
            .filter(|p| **p == notice("input after activation"))
            .count(),
        1
    );
    assert_eq!(
        posted
            .iter()
            .filter(|p| **p == notice("announced gap input"))
            .count(),
        0,
        "the legacy notice of the frozen gap is imported, never posted again"
    );
    let store = reopen(&o);
    let state = store.ledger().input_notices().clone();
    drop(store);
    let snapshot = state.migration.clone().unwrap();
    assert_eq!(
        snapshot.sources,
        [MigrationSource {
            source: snapshot.sources[0].source.clone(),
            lower: old.len() as u64,
            upper: frozen_upper,
            prefix_hash: hex::encode(Sha256::digest(&old)),
        }]
    );
    assert_eq!(state.imported.get(&input_key("gap")), Some(&announced));
    assert!(state.unresolved.is_empty());

    let again = spawn(&o, port.clone());
    settle(&port, 5).await;
    finish(again).await;
    assert_eq!(
        port.posts.lock().unwrap().len(),
        4,
        "a restart re-posts no notice or Body"
    );
    let store = reopen(&o);
    assert_eq!(
        store.ledger().input_notices().migration.as_ref(),
        Some(&snapshot),
        "a later boot keeps the first frozen cohort"
    );
}

async fn pending_in_frozen_cohort(withdrawn: bool) {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    let prompt = "frozen pending input".to_string();
    let raw = row("frozen", &prompt);
    std::fs::write(&path, &raw).unwrap();
    let logical = "frozen-pending-pane";
    let (o, _binding) = begin(runtime.path(), &path, logical);
    history::register(("claude", logical, "attempt", "733201"), &path, 0, &prompt).unwrap();
    let port = Arc::new(Port::default());
    port.messages.lock().unwrap().push(SeenMessage {
        id: 11,
        author_id: 42,
        content: crate::services::discord::input_notice_content(&prompt).unwrap(),
    });
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    let source = actor
        .writer
        .store()
        .cursors()
        .next()
        .unwrap()
        .source
        .clone();
    actor
        .writer
        .record(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: vec![MigrationSource {
                        source,
                        lower: 0,
                        upper: raw.len() as u64,
                        prefix_hash: hex::encode(Sha256::digest(b"")),
                    }],
                    binding_checkpoint: 1,
                    history: MigrationHistory::Fixed {
                        lower: 10,
                        upper: 11,
                    },
                },
            }),
        })
        .unwrap();
    resume(&mut actor);
    let fresh = actor.read_sources();
    actor.prepare_notices(&fresh).await;
    let key = input_key("frozen");
    assert_eq!(notice_evidence(&mut actor, &key), 1);
    assert_eq!(actor.notice_debt_extra(), 1);
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .input_notices()
            .unresolved
            .is_empty(),
        "undecided ownership is not a permanent migration verdict"
    );
    if withdrawn {
        history::withdraw("attempt", &path);
    } else {
        assert_eq!(
            history::receipt("attempt", &path, 0, "user", Some("frozen"), &prompt),
            Some(true)
        );
    }
    actor.prepare_notices(&[]).await;
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    actor.deliver_owed(&progress).await;
    let state = actor.writer.store().ledger().input_notices();
    assert!(state.unresolved.is_empty());
    if withdrawn {
        assert_eq!(
            state.imported.get(&key),
            Some(&11),
            "a withdrawn registration takes the ordinary legacy judgment"
        );
    } else {
        assert_eq!(
            actor.writer.store().ledger().excluded(&key),
            Some("input_notice_source")
        );
    }
    assert_eq!(actor.notice_debt_extra(), 0);
    assert!(port.posts.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn frozen_pending_ownership_is_excluded_after_its_receipt_without_a_migration_verdict() {
    pending_in_frozen_cohort(false).await;
}

#[tokio::test(start_paused = true)]
async fn frozen_pending_ownership_withdrawn_takes_the_ordinary_legacy_judgment() {
    pending_in_frozen_cohort(true).await;
}
