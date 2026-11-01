//! A reserved store-less Claude channel through the real host: a transcript bound after the floor
//! commits before its first output; a moved source rebases on proof the old one was delivered.

use super::super::late_host::Legacy;
use super::*;
use crate::config::Config;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;

const SESSION: &str = "c1a0de00-0000-4000-8000-000000007618";

fn reserve(channel: u64) -> test_override::ChannelsGuard {
    let config: Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "c", "name": "C",
            "channels": {"claude": {"id": channel.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [channel], "late_adopt": true},
            "turn": {"channels": [channel]}}
    }))
    .unwrap();
    let pending = |_: &_, _| Ok(BTreeMap::from([(channel, Adoption::Pending)]));
    let boot = BootChannels::validate(&config).unwrap();
    let boot = boot.seeded(&config, pending).unwrap();
    assert!(boot.candidate(channel).unwrap().late_adopt_origin());
    test_override::force_boot(boot)
}

fn record(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

/// What Claude writes through a first prompt, before any output.
fn prompted() -> Vec<u8> {
    [
        record(serde_json::json!({"type": "mode", "mode": "normal", "sessionId": SESSION})),
        record(serde_json::json!({"type": "file-history-snapshot", "messageId": "m"})),
        record(
            serde_json::json!({"type": "user", "sessionId": SESSION, "uuid": "u1",
            "message": {"role": "user", "content": "hi"}}),
        ),
        record(
            serde_json::json!({"type": "attachment", "sessionId": SESSION,
            "attachment": {"type": "skill_listing"}}),
        ),
    ]
    .concat()
}

fn answer(id: &str, text: &str) -> Vec<u8> {
    record(
        serde_json::json!({"type": "assistant", "sessionId": SESSION, "uuid": format!("a-{id}"),
        "apiBlockIndex": 0, "message": {"id": id, "role": "assistant",
        "content": [{"type": "text", "text": text}]}}),
    )
}

fn turn_end() -> Vec<u8> {
    record(
        serde_json::json!({"type": "system", "subtype": "turn_duration", "durationMs": 5,
        "sessionId": SESSION}),
    )
}

/// The channel's binding log as the hook writes it: each line one more seq.
struct Binds {
    channel: u64,
    lines: Mutex<Vec<u8>>,
    seq: AtomicU64,
}

impl Binds {
    fn new(channel: u64) -> Self {
        Self {
            channel,
            lines: Mutex::default(),
            seq: AtomicU64::new(0),
        }
    }

    fn bind(&self, root: &Path, nonce: &str, new: p5::BindingTarget) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let event = p5::BindingEvent {
            seq,
            channel_id: self.channel,
            provider: "claude".into(),
            tmux_session: "tmux-claude".into(),
            execution_nonce: Some(nonce.into()),
            old: None,
            new,
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: None,
                received_at: Utc::now(),
            },
            committed_at: Utc::now(),
        };
        let mut lines = self.lines.lock().unwrap();
        lines.extend(serde_json::to_vec(&event).unwrap());
        lines.push(b'\n');
        p5_log(root, self.channel, &lines);
        seq
    }

    fn source(&self, root: &Path, nonce: &str, path: &Path) -> SourceId {
        let source = source_id_for(SESSION, path).unwrap();
        self.bind(root, nonce, p5::BindingTarget::Source(source.clone()));
        source
    }
}

struct Reserved {
    harness: Harness,
    io: Arc<TestIo>,
    legacy: Arc<Legacy>,
    binds: Binds,
    _tasks: Hosts,
    _reserved: test_override::ChannelsGuard,
}

impl Reserved {
    fn root(&self) -> &Path {
        self.harness._runtime.path()
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn init(&self) -> Vec<(SourceId, u64)> {
        let channel = self.harness.channel_id;
        let init = self.harness.store.read_init(channel).unwrap().unwrap();
        let origin = self.harness.store.read_init_origin(channel).unwrap();
        assert_eq!(
            origin,
            crate::services::tui_o::store::InitOrigin::LateClaudeTakeover
        );
        (init.sources.into_iter())
            .map(|source| (source.source_id, source.delivery_start))
            .collect()
    }
}

/// A reserved host over `before`, a log already written as the reservation reads it first.
async fn reserved(channel: u64, before: impl FnOnce(&Harness, &Binds) -> PathBuf) -> Reserved {
    let harness = fresh_on(channel);
    harness.gate.acquired();
    let reserved = reserve(channel);
    p5::set_test_root(Some(harness._runtime.path()));
    let binds = Binds::new(channel);
    let path = before(&harness, &binds);
    let io = TestIo::over(&harness);
    io.late_enabled.store(true, Ordering::SeqCst);
    let legacy = Arc::new(Legacy {
        path,
        frontier: AtomicU64::new(0),
    });
    *io.legacy.lock().unwrap() = Some(legacy.clone());
    let ready = Arc::new(Readiness::default());
    let tasks = hosted_as(ShadowProvider::Claude, &harness, &io, (true, &ready));
    polls(3).await;
    assert_eq!(adoption(channel), Adoption::Deferred);
    Reserved {
        harness,
        io,
        legacy,
        binds,
        _tasks: tasks,
        _reserved: reserved,
    }
}

async fn settle() {
    for _ in 0..4 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        polls(3).await;
    }
}

fn legacy_sends(channel: u64) -> bool {
    let claim = crate::services::tui_o::cutover::o_owns_tui_output_for_channel_reserving;
    !claim(channel, Some(ClaudeTui)).unwrap().owned
}

/// A fresh launch binds Pending until its transcript exists; once resolved with its first prompt
/// and no output, O commits it from its first byte and posts the answer once, and Legacy does not.
#[tokio::test(start_paused = true)]
async fn late_claude_first_turn_from_pending_bind_is_taken_over() {
    const CH: u64 = 63_761_901;
    let f = reserved(CH, |h, _| h._runtime.path().join("t.jsonl")).await;
    let pending = p5::BindingTarget::Pending {
        payload_session_id: SESSION.into(),
        payload_transcript_path: None,
    };
    let at = f.binds.bind(f.root(), "n1", pending);
    polls(3).await;
    assert_eq!(adoption(CH), Adoption::Deferred, "a Pending bind waits");
    let path = f.file("t.jsonl", &prompted());
    let source = source_id_for(SESSION, &path).unwrap();
    let resolved = p5::BindingTarget::Resolved {
        pending_seq: at,
        source: source.clone(),
    };
    f.binds.bind(f.root(), "n1", resolved);
    settle().await;
    assert_eq!(
        adoption(CH),
        Adoption::Committed,
        "{:?}",
        f.io.alarms.halted()
    );
    assert_eq!(f.init(), [(source, 0)]);
    assert!(!legacy_sends(CH), "Legacy sends no body once O committed");
    append(&path, &answer("msg_1", "the answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["the answer"]);
}

/// A Legacy body send after the floor may be this transcript's streamed turn: it is not taken.
#[tokio::test(start_paused = true)]
async fn late_claude_takeover_skips_when_legacy_sent_since_baseline() {
    const CH: u64 = 63_761_902;
    let f = reserved(CH, |h, _| h._runtime.path().join("t.jsonl")).await;
    test_hook::set(CH, Step::BeforeFence, move || {
        assert!(legacy_sends(CH));
        Ok(())
    });
    let path = f.file("t.jsonl", &prompted());
    f.binds.source(f.root(), "n1", &path);
    settle().await;
    assert_eq!(adoption(CH), Adoption::Deferred);
    assert!(
        !f.harness.store.has_channel_dir(CH),
        "the streamed turn stays Legacy's"
    );
}

/// Old transcript delivered and sampled; a new turn makes Legacy busy, the generation's frontier
/// drops to 0 and a cleared transcript binds: O takes it while busy, by the sample alone.
#[tokio::test(start_paused = true)]
async fn late_claude_rotation_rebases_with_new_discord_turn_busy() {
    const CH: u64 = 63_761_903;
    let old = [prompted(), answer("msg_0", "old"), turn_end()].concat();
    let f = reserved(CH, |h, binds| {
        let path = h._runtime.path().join("old.jsonl");
        std::fs::write(&path, &old).unwrap();
        binds.source(h._runtime.path(), "n-old", &path);
        path
    })
    .await;
    f.legacy.frontier.store(old.len() as u64, Ordering::SeqCst);
    let io = f.io.clone();
    test_hook::set(CH, Step::AfterFence, move || {
        io.busy.store(true, Ordering::SeqCst);
        Err("the new turn claimed the mailbox".into())
    });
    settle().await;
    assert_eq!(adoption(CH), Adoption::Deferred);
    assert!(
        f.io.busy.load(Ordering::SeqCst),
        "the quiet handoff was tried once"
    );
    f.legacy.frontier.store(0, Ordering::SeqCst);
    append(
        &f.legacy.path,
        &record(serde_json::json!({"type": "cost-state", "sessionId": SESSION})),
    );
    let path = f.file("new.jsonl", &prompted());
    let source = f.binds.source(f.root(), "n-new", &path);
    settle().await;
    assert_eq!(
        adoption(CH),
        Adoption::Committed,
        "{:?}",
        f.io.alarms.released()
    );
    assert_eq!(f.init(), [(source, 0)]);
}

/// Legacy's read cursor at EOF is not delivery: with its frontier before the answer, no sample
/// proves the old transcript, so the cleared one is not taken and the reservation stays Deferred.
#[tokio::test(start_paused = true)]
async fn late_claude_read_cursor_at_eof_without_delivery_does_not_rebase_into_a_takeover() {
    const CH: u64 = 63_761_904;
    let head = prompted();
    let old = [head.clone(), answer("msg_0", "undelivered"), turn_end()].concat();
    let f = reserved(CH, |h, binds| {
        let path = h._runtime.path().join("old.jsonl");
        std::fs::write(&path, &old).unwrap();
        binds.source(h._runtime.path(), "n-old", &path);
        path
    })
    .await;
    f.legacy.frontier.store(head.len() as u64, Ordering::SeqCst);
    settle().await;
    let path = f.file("new.jsonl", &prompted());
    f.binds.source(f.root(), "n-new", &path);
    settle().await;
    assert_eq!(
        adoption(CH),
        Adoption::Deferred,
        "{:?}",
        f.io.alarms.released()
    );
    assert!(!f.harness.store.has_channel_dir(CH));
}

/// A first read under Legacy custody fixes no deferred baseline, yet a transcript already bound
/// then is not new: it is not taken over.
#[tokio::test(start_paused = true)]
async fn late_claude_fallback_boundary_does_not_take_a_bind_from_before_the_first_read() {
    const CH: u64 = 63_761_905;
    let harness = fresh_on(CH);
    let binds = Binds::new(CH);
    let path = harness._runtime.path().join("t.jsonl");
    std::fs::write(&path, prompted()).unwrap();
    p5::set_test_root(Some(harness._runtime.path()));
    binds.source(harness._runtime.path(), "n1", &path);
    harness.gate.acquired();
    let _reserved = reserve(CH);
    let io = TestIo::over(&harness);
    io.late_enabled.store(true, Ordering::SeqCst);
    *io.custody.lock().unwrap() = Ok(Custody::Row);
    let legacy = Arc::new(Legacy {
        path,
        frontier: AtomicU64::new(0),
    });
    *io.legacy.lock().unwrap() = Some(legacy);
    let ready = Arc::new(Readiness::default());
    let _tasks = hosted_as(ShadowProvider::Claude, &harness, &io, (true, &ready));
    settle().await;
    let candidate = (cutover::boot_ownership().into_iter())
        .find_map(|(channel, _, candidate)| (channel == CH).then_some(candidate))
        .flatten();
    let boundary = candidate.unwrap().late_boundary().unwrap();
    assert_eq!((boundary.baseline, boundary.takeover_after), (0, 1));
    assert_eq!(adoption(CH), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CH));
}

/// The old transcript answered past Legacy's frontier, or delivered when `delivered` but with its
/// commit taken; Legacy then moves to a cleared transcript and delivers its whole turn.
async fn moved_on(channel: u64, delivered: bool) -> (Reserved, SourceId, u64) {
    let head = prompted();
    let old = [head.clone(), answer("msg_0", "old"), turn_end()].concat();
    let f = reserved(channel, |h, binds| {
        let path = h._runtime.path().join("old.jsonl");
        std::fs::write(&path, &old).unwrap();
        binds.source(h._runtime.path(), "n-old", &path);
        path
    })
    .await;
    let frontier = if delivered { old.len() } else { head.len() };
    f.legacy.frontier.store(frontier as u64, Ordering::SeqCst);
    // A new Discord turn takes the commit of the old transcript Legacy delivered and sampled.
    if delivered {
        let io = f.io.clone();
        test_hook::set(channel, Step::AfterFence, move || {
            io.busy.store(true, Ordering::SeqCst);
            Err("the new turn claimed the mailbox".into())
        });
    }
    settle().await;
    assert_eq!(adoption(channel), Adoption::Deferred);
    let new = [prompted(), answer("msg_1", "new"), turn_end()].concat();
    let path = f.file("new.jsonl", &new);
    let moved = Legacy {
        path: path.clone(),
        frontier: AtomicU64::new(new.len() as u64),
    };
    *f.io.legacy.lock().unwrap() = Some(Arc::new(moved));
    let source = f.binds.source(f.root(), "n-new", &path);
    f.io.busy.store(false, Ordering::SeqCst);
    settle().await;
    settle().await;
    (f, source, new.len() as u64)
}

/// With the old transcript's answer undelivered, the cleared one finishing its turn and Legacy
/// delivering it to EOF does not let O start the old one at EOF: the channel stays Legacy's.
#[tokio::test(start_paused = true)]
async fn late_claude_fallback_keeps_an_undelivered_old_transcript_owed_after_the_new_turn() {
    const CH: u64 = 63_761_906;
    let (f, _, _) = moved_on(CH, false).await;
    assert_eq!(
        adoption(CH),
        Adoption::Deferred,
        "{:?}",
        f.io.alarms.released()
    );
    assert!(!f.harness.store.has_channel_dir(CH));
}

/// The old transcript sampled delivered settles its obligation, so the quiet handoff commits the
/// cleared one at its EOF once Legacy delivered its turn.
#[tokio::test(start_paused = true)]
async fn late_claude_fallback_commits_once_the_old_transcript_was_proven_delivered() {
    const CH: u64 = 63_761_907;
    let (f, source, len) = moved_on(CH, true).await;
    assert_eq!(
        adoption(CH),
        Adoption::Committed,
        "{:?}",
        f.io.alarms.released()
    );
    let init = f.harness.store.read_init(CH).unwrap().unwrap();
    let new = init
        .sources
        .iter()
        .find(|pinned| pinned.source_id == source);
    assert_eq!(new.map(|pinned| pinned.delivery_start), Some(len));
}

/// The old transcript delivered and sampled, two rotations land before the next look: B with an
/// answer Legacy never delivered, then C delivered to EOF. B stays owed, so C is not committed.
#[tokio::test(start_paused = true)]
async fn late_claude_rotations_read_together_keep_the_middle_transcript_owed() {
    const CH: u64 = 63_761_908;
    let old = [prompted(), answer("msg_0", "old"), turn_end()].concat();
    let f = reserved(CH, |h, binds| {
        let path = h._runtime.path().join("old.jsonl");
        std::fs::write(&path, &old).unwrap();
        binds.source(h._runtime.path(), "n-old", &path);
        path
    })
    .await;
    f.legacy.frontier.store(old.len() as u64, Ordering::SeqCst);
    let io = f.io.clone();
    test_hook::set(CH, Step::AfterFence, move || {
        io.busy.store(true, Ordering::SeqCst);
        Err("the new turn claimed the mailbox".into())
    });
    settle().await;
    assert_eq!(adoption(CH), Adoption::Deferred);
    let turn = |id, text| [prompted(), answer(id, text), turn_end()].concat();
    let middle = f.file("middle.jsonl", &turn("msg_1", "undelivered"));
    let last = turn("msg_2", "delivered");
    let path = f.file("last.jsonl", &last);
    f.binds.source(f.root(), "n-middle", &middle);
    f.binds.source(f.root(), "n-last", &path);
    let moved = Legacy {
        path,
        frontier: AtomicU64::new(last.len() as u64),
    };
    *f.io.legacy.lock().unwrap() = Some(Arc::new(moved));
    f.io.busy.store(false, Ordering::SeqCst);
    settle().await;
    settle().await;
    assert_eq!(
        adoption(CH),
        Adoption::Deferred,
        "{:?}",
        f.io.alarms.released()
    );
    assert!(!f.harness.store.has_channel_dir(CH));
}
