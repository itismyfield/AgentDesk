//! A Codex channel's first activation through the real host: with no source bound yet, over a
//! source holding only metadata, or refused over a source that already holds output.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use super::*;
use crate::services::tui_o::cutover::o_owns_tui_output_for_channel_reserving;
use crate::services::tui_o::store::no_channel_store;
use crate::services::tui_o::writer::activation::test_hook::{self, Step};

/// A channel of its own per test, since activation hooks are keyed by channel process-wide.
fn fresh_on(channel: u64) -> Harness {
    let harness = Harness::build_channel(channel, |_| Vec::new());
    let store_dir = harness._runtime.path().join("o_store");
    std::fs::remove_dir_all(store_dir.join(channel.to_string())).unwrap();
    std::fs::remove_file(store_dir.join("o_era")).unwrap();
    p5_log(harness._runtime.path(), channel, b"");
    harness
}

fn codex_line(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

/// What a new rollout holds before its first turn: session, turn context and a token count.
fn metadata(session: &str) -> Vec<u8> {
    let at = Utc::now().to_rfc3339();
    [
        codex_line(serde_json::json!({"type": "session_meta", "timestamp": at,
            "payload": {"id": session, "timestamp": at, "cwd": "/", "originator": "codex_cli_rs"}})),
        codex_line(serde_json::json!({"type": "turn_context", "timestamp": at,
            "payload": {"cwd": "/", "model": "m"}})),
        codex_line(serde_json::json!({"type": "event_msg", "timestamp": at,
            "payload": {"type": "token_count", "info": null}})),
    ]
    .concat()
}

fn answer(id: &str, text: &str) -> Vec<u8> {
    codex_line(
        serde_json::json!({"type": "response_item", "timestamp": Utc::now().to_rfc3339(),
        "payload": {"type": "message", "role": "assistant", "id": id,
        "content": [{"type": "output_text", "text": text}]}}),
    )
}

/// A rollout under the harness root holding `bytes`, and its source as the hook names it.
fn rollout(harness: &Harness, name: &str, bytes: &[u8]) -> (PathBuf, SourceId) {
    let path = harness._runtime.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    let source = source_id_for("s-codex", &path).unwrap();
    (path, source)
}

/// Appends the hook's bind of `path` through the binding log's own writer.
fn bind_late(channel: u64, path: &Path) {
    let hook =
        p5::HookSignal::from_payload("session_start", &serde_json::json!({"source": "startup"}));
    let proposal = p5::Proposal {
        channel_id: channel,
        provider: "codex",
        tmux_session: "tmux-codex",
        session_id: Some("s-codex"),
        path: path.to_str().unwrap(),
        replaced: None,
        cause: p5::CauseSource::Hook(hook.cause()),
        hook: Some(&hook),
    };
    assert_eq!(
        p5::record_source(&proposal).unwrap(),
        p5::Committed::Appended
    );
}

fn start_codex(harness: &Harness, io: &Arc<TestIo>, ready: &Arc<Readiness>) -> Hosts {
    hosted_as(ShadowProvider::Codex, harness, io, (true, ready))
}

/// Restarts the host over the committed store: nothing new is posted and nothing is activated.
async fn restart_posts_nothing(harness: &Harness, tasks: Hosts, source: &SourceId) {
    let channel = harness.channel_id;
    let posts = harness.port.posts();
    abort(tasks);
    polls(2).await;
    let written = std::fs::read(init_path(harness, channel)).unwrap();
    let era = harness.store.read_era().unwrap();
    let cursor = harness.channel().cursor(source).cloned();
    let (io, ready) = (TestIo::over(harness), Arc::new(Readiness::default()));
    let _tasks = start_codex(harness, &io, &ready);
    polls(3).await;
    assert!(ready.is_ready(channel), "{:?}", io.alarms.halted());
    assert_eq!(harness.port.posts(), posts, "a restart posts nothing new");
    assert!(
        !io.calls().contains(&("facts", channel)) && !io.calls().contains(&("adopted", channel)),
        "a restart recovers, it does not activate"
    );
    assert_eq!(std::fs::read(init_path(harness, channel)).unwrap(), written);
    assert_eq!(harness.store.read_era().unwrap(), era);
    assert_eq!(harness.channel().cursor(source).cloned(), cursor);
}

#[tokio::test(start_paused = true)]
async fn an_unbound_codex_channel_commits_an_empty_init_and_posts_a_late_source_once() {
    const CODEX: u64 = 61_801;
    let harness = fresh_on(CODEX);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CODEX, CodexTui)]);
    let (share, claimer) = (test_override::shared_channels(), Arc::new(Mutex::new(None)));
    let (blocked, slot) = (Arc::new(AtomicBool::new(false)), Arc::clone(&claimer));
    let waited = Arc::clone(&blocked);
    // Legacy's body claim arrives while the activation holds the adoption lock before its write.
    test_hook::set(CODEX, Step::BeforeWrite, move || {
        let (at_claim, reached_claim) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _channels = share();
            at_claim.send(()).unwrap();
            o_owns_tui_output_for_channel_reserving(CODEX, Some(CodexTui)).map(|c| c.owned)
        });
        reached_claim.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        waited.store(!thread.is_finished(), Ordering::SeqCst);
        *slot.lock().unwrap() = Some(thread);
        Ok(())
    });
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(io.alarms.released(), [], "{:?}", io.alarms.halted());
    let claimed = claimer
        .lock()
        .unwrap()
        .take()
        .expect("the activation reached its write");
    assert!(
        blocked.load(Ordering::SeqCst),
        "the claim waited on the adoption lock"
    );
    assert_eq!(
        claimed.join().unwrap(),
        Ok(true),
        "the body is O's: Legacy sends nothing"
    );
    assert_eq!(adoption(CODEX), Adoption::Committed);
    let init = harness.store.read_init(CODEX).unwrap().unwrap();
    assert!(init.sources.is_empty(), "{:?}", init.sources);
    assert!(ready.is_ready(CODEX), "{:?}", io.alarms.halted());
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(0));

    let (path, source) = rollout(&harness, "rollout-late.jsonl", &metadata("s-codex"));
    bind_late(CODEX, &path);
    polls(3).await;
    append(&path, &answer("msg_1", "the answer"));
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["the answer"],
        "{:?}",
        io.alarms.halted()
    );
    restart_posts_nothing(&harness, tasks, &source).await;
}

#[tokio::test(start_paused = true)]
async fn a_legacy_claim_before_the_activation_lock_leaves_no_init_and_legacy_sends() {
    const CODEX: u64 = 61_802;
    let harness = fresh_on(CODEX);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CODEX, CodexTui)]);
    let legacy_sends = Arc::new(AtomicUsize::new(0));
    let sends = Arc::clone(&legacy_sends);
    test_hook::set(CODEX, Step::BeforeLock, move || {
        let claimed = o_owns_tui_output_for_channel_reserving(CODEX, Some(CodexTui)).unwrap();
        if !claimed.owned {
            sends.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    });
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let _tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(legacy_sends.load(Ordering::SeqCst), 1);
    assert_eq!(adoption(CODEX), Adoption::Released);
    assert_eq!(harness.store.read_era().unwrap(), None);
    assert!(!harness.store.has_channel_dir(CODEX));
    assert!(!ready.is_ready(CODEX) && harness.port.posts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_bound_codex_source_holding_only_metadata_starts_at_its_end() {
    const CODEX: u64 = 61_803;
    let prefix = metadata("s-codex");
    let harness = fresh_on(CODEX);
    let (path, source) = rollout(&harness, "rollout-bound.jsonl", &prefix);
    let bind = p5_event(CODEX, "codex", p5::BindingTarget::Source(source.clone()));
    p5_log(harness._runtime.path(), CODEX, &bind);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CODEX, CodexTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(io.alarms.released(), [], "{:?}", io.alarms.halted());
    assert_eq!(adoption(CODEX), Adoption::Committed);
    let init = harness.store.read_init(CODEX).unwrap().unwrap();
    let start = InitSource {
        source_id: source.clone(),
        delivery_start: prefix.len() as u64,
        prefix_hash: hex::encode(Sha256::digest(&prefix)),
    };
    assert_eq!(init.sources, [start]);
    append(&path, &answer("msg_1", "the answer"));
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["the answer"],
        "{:?}",
        io.alarms.halted()
    );
    restart_posts_nothing(&harness, tasks, &source).await;
}

#[tokio::test(start_paused = true)]
async fn a_bound_codex_source_that_already_holds_output_gets_no_init() {
    const CODEX: u64 = 61_804;
    let unit = answer("msg_0", "an old answer");
    let blocked = codex_line(
        serde_json::json!({"type": "response_item", "payload": {"type": "unknown_item"}}),
    );
    let announced = codex_line(serde_json::json!({"type": "event_msg", "payload": {
        "type": "item_completed", "item": {"type": "AgentMessage", "id": "msg_k"}}}));
    for (why, record) in [
        ("unit", unit),
        ("blocked", blocked),
        ("announced", announced),
    ] {
        let harness = fresh_on(CODEX);
        let bytes = [metadata("s-codex"), record].concat();
        let (_, source) = rollout(&harness, "rollout-old.jsonl", &bytes);
        let bind = p5_event(CODEX, "codex", p5::BindingTarget::Source(source));
        p5_log(harness._runtime.path(), CODEX, &bind);
        harness.gate.acquired();
        let _selected = test_override::force_candidates(&[(CODEX, CodexTui)]);
        let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
        let _tasks = start_codex(&harness, &io, &ready);
        polls(3).await;
        let released = io.alarms.released();
        assert!(
            matches!(released.as_slice(), [(CODEX, d)] if d.contains("holds more than metadata")),
            "{why}: {released:?}"
        );
        assert_eq!(adoption(CODEX), Adoption::Released, "{why}");
        assert!(!init_path(&harness, CODEX).exists(), "{why}");
        assert_eq!(harness.store.read_era().unwrap(), None, "{why}");
        assert!(!harness.store.has_channel_dir(CODEX), "{why}");
        assert!(harness.port.posts().is_empty(), "{why}");
    }
}

/// How long a cross-thread wait may take before the test fails instead of hanging.
const WATCHDOG: Duration = Duration::from_secs(10);

// The init is written under the gate's admission lock: a close racing the write returns only after
// it, so a store read once the close returned sees the init.
#[tokio::test(start_paused = true)]
async fn a_close_racing_the_first_init_returns_only_after_its_write() {
    const CODEX: u64 = 61_805;
    let harness = fresh_on(CODEX);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CODEX, CodexTui)]);
    let (observer, closing) = (Arc::clone(&harness.gate), Arc::clone(&harness.gate));
    let root = harness._runtime.path().to_path_buf();
    let locked = Arc::new(AtomicBool::new(false));
    let checked = Arc::clone(&locked);
    let (closed_tx, closed) = std::sync::mpsc::channel();
    test_hook::set(CODEX, Step::BeforeWrite, move || {
        let held = std::thread::spawn(move || observer.locked_for_test());
        checked.store(held.join().unwrap(), Ordering::SeqCst);
        std::thread::spawn(move || {
            closing.close();
            closed_tx.send(no_channel_store(&root, CODEX)).unwrap();
        });
        Ok(())
    });
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let _tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    assert!(
        locked.load(Ordering::SeqCst),
        "the write holds the gate lock"
    );
    let absent = closed.recv_timeout(WATCHDOG).expect("the close returned");
    assert!(!absent, "a close that returned saw no init");
    assert!(harness.store.read_init(CODEX).unwrap().is_some());
}

// A gate closed after the facts were judged and before the write gets no init, alarm or release;
// the activation waits for Owned again and commits once the gate reopens.
#[tokio::test(start_paused = true)]
async fn a_gate_closed_before_the_held_write_creates_nothing_until_owned_again() {
    const HELD: u64 = 61_806;
    let harness = Harness::build_channel(HELD, |_| Vec::new());
    let store_dir = harness._runtime.path().join("o_store");
    std::fs::remove_dir_all(store_dir.join(HELD.to_string())).unwrap();
    std::fs::remove_file(store_dir.join("o_era")).unwrap();
    let closed = serde_json::json!({"type":"system", "subtype":"turn_duration", "durationMs":5});
    let delivered = [row("m0", "delivered"), format!("{closed}\n").into_bytes()].concat();
    let path = harness._runtime.path().join("held.jsonl");
    std::fs::write(&path, &delivered).unwrap();
    let source = source_id_for("s1", &path).unwrap();
    let startup = p5_event(HELD, "claude", p5::BindingTarget::Source(source));
    p5_log(harness._runtime.path(), HELD, &startup);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(HELD, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let len = delivered.len() as u64;
    let legacy = Cursor {
        path,
        cursor: len,
        frontier: len,
    };
    *io.legacy.lock().unwrap() = Some(Arc::new(legacy));
    let closing = Arc::clone(&harness.gate);
    let reached = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&reached);
    test_hook::set(HELD, Step::Snapshot, move || {
        closing.close();
        seen.store(true, Ordering::SeqCst);
        Ok(())
    });
    let tasks = hosted(&harness, &io, true, &ready);
    polls(3).await;
    assert!(reached.load(Ordering::SeqCst), "the held activation pinned");
    assert!(
        !harness.store.has_channel_dir(HELD),
        "no init past a closed gate"
    );
    assert_eq!(io.alarms.halted(), []);
    assert_eq!(io.alarms.released(), []);
    assert_eq!(adoption(HELD), Adoption::Pending);
    harness.gate.acquired();
    polls(3).await;
    assert_eq!(
        adoption(HELD),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert!(harness.store.read_init(HELD).unwrap().is_some());
    abort(tasks);
}

// A lease transition on a gate whose first activation waits on another channel's activation waits
// too, and completes once that one lets the serial lock go and this one has written.
#[tokio::test(start_paused = true)]
async fn a_transition_waits_while_its_activation_waits_on_the_serial_lock() {
    const FIRST: u64 = 61_807;
    const NEXT: u64 = 61_808;
    let (held_tx, held) = std::sync::mpsc::channel();
    let (go_tx, go) = std::sync::mpsc::channel::<()>();
    // FIRST activates on its own runtime and holds the serial lock until told to go.
    let first = std::thread::spawn(move || {
        let mut runtime = tokio::runtime::Builder::new_current_thread();
        let runtime = runtime.enable_all().start_paused(true).build().unwrap();
        runtime.block_on(async move {
            let harness = fresh_on(FIRST);
            harness.gate.acquired();
            let _selected = test_override::force_candidates(&[(FIRST, CodexTui)]);
            test_hook::set(FIRST, Step::BeforeWrite, move || {
                held_tx.send(()).unwrap();
                go.recv().unwrap();
                Ok(())
            });
            let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
            let _tasks = start_codex(&harness, &io, &ready);
            polls(3).await;
            harness.store.read_init(FIRST).unwrap().is_some()
        })
    });
    held.recv_timeout(WATCHDOG)
        .expect("FIRST holds the serial lock");
    let harness = fresh_on(NEXT);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(NEXT, CodexTui)]);
    let (observer, changing) = (Arc::clone(&harness.gate), Arc::clone(&harness.gate));
    let (entered_tx, entered) = std::sync::mpsc::channel();
    test_hook::set(NEXT, Step::BeforeLock, move || {
        let locked = std::thread::spawn(move || observer.locked_for_test());
        entered_tx.send(locked.join().unwrap()).unwrap();
        Ok(())
    });
    // NEXT's activation blocks this runtime until FIRST lets go, so the transition runs off it.
    let transition = std::thread::spawn(move || {
        let locked = entered
            .recv_timeout(WATCHDOG)
            .expect("NEXT began its activation");
        let (asked_tx, asked) = std::sync::mpsc::channel();
        let lost = std::thread::spawn(move || {
            let at = std::time::Instant::now();
            asked_tx.send(()).unwrap();
            changing.lost();
            at.elapsed()
        });
        asked.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let waited = !lost.is_finished();
        go_tx.send(()).unwrap();
        (locked, waited, lost.join().unwrap())
    });
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let _tasks = start_codex(&harness, &io, &ready);
    polls(3).await;
    let (locked, waited, took) = transition.join().unwrap();
    assert!(locked, "NEXT activates under its gate's admission");
    assert!(
        waited,
        "the transition waited while FIRST held the serial lock"
    );
    eprintln!("[first_init] the lease transition waited {took:?}");
    assert!(first.join().unwrap(), "FIRST committed");
    assert!(harness.store.read_init(NEXT).unwrap().is_some());
    let lost = crate::services::tui_o::ownership::GatewayOwnership::Lost;
    assert_eq!(harness.gate.current(), lost);
}

#[path = "host_codex_takeover_tests.rs"]
mod codex_takeover;
