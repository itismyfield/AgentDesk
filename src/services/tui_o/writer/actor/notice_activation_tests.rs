use super::*;
use crate::services::tui_o::turn_mode::TestConfirmation;

type Run = tokio::task::JoinHandle<Option<crate::services::tui_o::writer::deliver::StopCause>>;

/// Production derives notices only on a confirmed channel, and these tests share `CHANNEL`.
static TURNS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn source_of(path: &std::path::Path) -> crate::services::tui_o::shadow::SourceId {
    crate::services::tui_o::shadow::binding_reader::source_id_for("native", path).unwrap()
}

fn begin(runtime: &std::path::Path, path: &std::path::Path, logical: &str) -> (OStore, impl Sized) {
    let source = source_of(path);
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

/// The previous build captured and delivered without any notice projection.
async fn old_build(o: &OStore, port: &Arc<Port>) {
    let mut actor = host(o, port.clone(), Alarms::default(), u64::MAX);
    actor.deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    resume(&mut actor);
    actor.read_sources();
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    actor.deliver_owed(&progress).await;
}

/// A notice the legacy hook posted for `prompt`.
fn legacy(port: &Port, prompt: &str) -> u64 {
    let mut messages = port.messages.lock().unwrap();
    let id = 1000 + messages.len() as u64;
    messages.push(SeenMessage {
        id,
        author_id: 42,
        content: crate::services::discord::input_notice_content(prompt).unwrap(),
    });
    id
}

fn notice(prompt: &str) -> String {
    crate::services::discord::input_notice_content(prompt).unwrap()
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

fn notices(o: &OStore) -> crate::services::tui_o::store::ledger::input_notices::NoticeState {
    let store = o
        .open_channel(&o.read_era().unwrap().unwrap(), CHANNEL)
        .unwrap()
        .unwrap();
    store.ledger().input_notices().clone()
}

fn debt_candidates(
    state: &crate::services::tui_o::store::ledger::input_notices::NoticeState,
    id: &str,
) -> Option<Vec<u64>> {
    let key = input_key(id);
    let (record, _) = (state.evidence.iter()).find(|(_, e)| e.unit_key.as_ref() == Some(&key))?;
    Some(state.unresolved.get(record)?.candidates.clone())
}

#[tokio::test(start_paused = true)]
async fn first_activation_freezes_the_gap_before_resume_and_notices_each_new_input_once() {
    let _turns = TURNS.lock().await;
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    let old = [row("old", "old input"), body("old-body", "old answer")].concat();
    std::fs::write(&path, &old).unwrap();
    let (o, _binding) = begin(runtime.path(), &path, "activation-pane");
    let port = Arc::new(Port::default());
    legacy(&port, "old input");
    // The old hook announced a queued input while the previous turn ran, before its answer.
    let announced = legacy(&port, "announced gap input");
    old_build(&o, &port).await;
    assert_eq!(*port.posts.lock().unwrap(), ["old answer"]);
    // The queued input's row lands after the old writer stopped; the next one had no hook at all.
    append(&path, &row("gap", "announced gap input"));
    append(&path, &row("silent", "silent gap input"));
    append(&path, &body("gap-body", "gap answer"));
    let frozen_upper = std::fs::metadata(&path).unwrap().len();

    let _confirmed = TestConfirmation::new(CHANNEL);
    let first = spawn(&o, port.clone());
    settle(&port, 3).await;
    append(&path, &row("after", "input after activation"));
    settle(&port, 4).await;
    finish(first).await;
    let posted = port.posts.lock().unwrap().clone();
    assert_eq!(posted.len(), 4, "{posted:?}");
    assert_eq!(posted[0], "old answer");
    for (expected, count) in [
        (notice("silent gap input"), 1),
        ("gap answer".into(), 1),
        (notice("input after activation"), 1),
        (notice("announced gap input"), 0),
    ] {
        assert_eq!(posted.iter().filter(|p| **p == expected).count(), count);
    }
    let state = notices(&o);
    let snapshot = state.migration.clone().unwrap();
    assert_eq!(
        snapshot.sources,
        [MigrationSource {
            source: source_of(&path),
            lower: old.len() as u64,
            upper: frozen_upper,
            prefix_hash: hex::encode(Sha256::digest(&old)),
        }]
    );
    assert_eq!(
        debt_candidates(&state, "gap"),
        Some(vec![announced]),
        "an announced gap input stays migration debt, never reposted nor settled"
    );
    assert_eq!(state.unresolved.len(), 1);
    assert!(state.imported.is_empty());

    let again = spawn(&o, port.clone());
    settle(&port, 5).await;
    finish(again).await;
    assert_eq!(
        port.posts.lock().unwrap().len(),
        4,
        "a restart re-posts no notice or Body"
    );
    assert_eq!(
        notices(&o).migration.as_ref(),
        Some(&snapshot),
        "a later boot keeps the first frozen cohort"
    );
}

#[tokio::test(start_paused = true)]
async fn a_frozen_input_matching_only_an_older_inputs_notice_stays_debt_and_keeps_its_body() {
    let _turns = TURNS.lock().await;
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    let old = [row("before", "same words"), body("old-body", "old answer")].concat();
    std::fs::write(&path, &old).unwrap();
    let (o, _binding) = begin(runtime.path(), &path, "older-match-pane");
    let port = Arc::new(Port::default());
    // Only the input before the frozen lower was ever announced.
    let older = legacy(&port, "same words");
    old_build(&o, &port).await;
    append(&path, &row("gap", "same words"));
    append(&path, &body("gap-body", "gap answer"));

    let _confirmed = TestConfirmation::new(CHANNEL);
    for _ in 0..2 {
        let run = spawn(&o, port.clone());
        settle(&port, 2).await;
        finish(run).await;
        assert_eq!(*port.posts.lock().unwrap(), ["old answer", "gap answer"]);
        let state = notices(&o);
        assert_eq!(debt_candidates(&state, "gap"), Some(vec![older]));
        assert!(state.imported.is_empty());
    }
    let store = o
        .open_channel(&o.read_era().unwrap().unwrap(), CHANNEL)
        .unwrap()
        .unwrap();
    assert!(store.ledger().excluded(&input_key("gap")).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_source_bound_before_the_freeze_joins_the_cohort_before_its_first_capture() {
    let _turns = TURNS.lock().await;
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    std::fs::write(
        &path,
        [row("old", "old input"), body("old-body", "old answer")].concat(),
    )
    .unwrap();
    let (o, _binding) = begin(runtime.path(), &path, "bound-pane");
    let port = Arc::new(Port::default());
    legacy(&port, "old input");
    old_build(&o, &port).await;
    // A new transcript is bound durably, its input announced, before any writer followed it.
    let next = runtime.path().join("next.jsonl");
    std::fs::write(&next, row("bound", "input on the new transcript")).unwrap();
    let _next_binding = bind(
        runtime.path(),
        CHANNEL,
        &source_of(&next),
        "bound-pane-next",
        "73320000000000000000000000000096",
    );
    let announced = legacy(&port, "input on the new transcript");

    let _confirmed = TestConfirmation::new(CHANNEL);
    let run = spawn(&o, port.clone());
    settle(&port, 1).await;
    append(&next, &row("later", "later input on the new transcript"));
    settle(&port, 2).await;
    finish(run).await;
    let posted = port.posts.lock().unwrap().clone();
    assert_eq!(
        posted,
        [
            "old answer".to_string(),
            notice("later input on the new transcript")
        ]
    );
    let state = notices(&o);
    let frozen = state.migration.clone().unwrap().sources;
    let bound = frozen.iter().find(|s| s.source == source_of(&next));
    assert_eq!(
        bound.map(|s| (s.lower, s.upper)),
        Some((0, row("bound", "input on the new transcript").len() as u64))
    );
    assert_eq!(debt_candidates(&state, "bound"), Some(vec![announced]));
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_binding_log_holds_the_first_freeze_and_resume_without_halting() {
    let _turns = TURNS.lock().await;
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    std::fs::write(
        &path,
        [row("old", "old input"), body("old-body", "old answer")].concat(),
    )
    .unwrap();
    let (o, _binding) = begin(runtime.path(), &path, "unreadable-log-pane");
    let port = Arc::new(Port::default());
    legacy(&port, "old input");
    old_build(&o, &port).await;
    append(&path, &row("gap", "gap input"));
    append(&path, &body("gap-body", "gap answer"));
    let log = (runtime.path())
        .join(crate::services::tui_prompt_dedupe::binding_events::BINDING_EVENTS_DIR)
        .join(format!("{CHANNEL}.log"));
    let intact = std::fs::read(&log).unwrap();
    append(&log, b"{unreadable\n");

    let _confirmed = TestConfirmation::new(CHANNEL);
    let run = spawn(&o, port.clone());
    settle(&port, 2).await;
    assert_eq!(
        *port.posts.lock().unwrap(),
        ["old answer"],
        "nothing past the old cursor is captured before the cohort is frozen"
    );
    std::fs::write(&log, &intact).unwrap();
    settle(&port, 3).await;
    finish(run).await;
    let posted = port.posts.lock().unwrap().clone();
    assert_eq!(posted.len(), 3, "{posted:?}");
    assert!(posted.contains(&notice("gap input")) && posted.contains(&"gap answer".into()));
    assert!(notices(&o).migration.is_some());
}

#[tokio::test(start_paused = true)]
async fn an_unconfirmed_channel_leaves_notices_to_its_live_owner_and_still_posts_body() {
    let _turns = TURNS.lock().await;
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    std::fs::write(&path, b"").unwrap();
    let (o, _binding) = begin(runtime.path(), &path, "unconfirmed-pane");
    let port = Arc::new(Port::default());
    let run = spawn(&o, port.clone());
    settle(&port, 0).await;
    append(&path, &row("legacy-turn", "input under legacy turns"));
    append(&path, &body("legacy-body", "legacy turn answer"));
    settle(&port, 1).await;
    finish(run).await;
    assert_eq!(*port.posts.lock().unwrap(), ["legacy turn answer"]);
    let state = notices(&o);
    assert!(state.migration.is_none() && state.evidence.is_empty());
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
        content: notice(&prompt),
    });
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    actor
        .writer
        .record(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: vec![MigrationSource {
                        source: source_of(&path),
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
    assert!(state.imported.is_empty());
    if withdrawn {
        assert_eq!(
            state
                .unresolved
                .values()
                .map(|debt| debt.candidates.clone())
                .collect::<Vec<_>>(),
            [vec![11]],
            "a withdrawn registration takes the ordinary legacy judgment"
        );
        assert_eq!(actor.notice_debt_extra(), 1);
    } else {
        assert!(state.unresolved.is_empty());
        assert_eq!(
            actor.writer.store().ledger().excluded(&key),
            Some("input_notice_source")
        );
        assert_eq!(actor.notice_debt_extra(), 0);
    }
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
