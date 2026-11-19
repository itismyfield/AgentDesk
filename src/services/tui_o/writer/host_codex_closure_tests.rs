//! A takeover never starts on a turn a rejection closed, and never leaves out a turn owed before one.

use super::*;

/// Legacy holding no record of a generation that began an hour ago, the turns rejections closed,
/// and the ranges a takeover closed with their own reason.
#[derive(Default)]
struct Closed {
    closed: Mutex<Vec<(SourceId, u64, Option<u64>)>>,
    unplaced: Mutex<Vec<(u64, u64)>>,
}

impl LegacyView for Closed {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, _: &str) -> LegacyCursor {
        LegacyCursor::Unbound
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        None
    }

    fn tail_running(&self, _: &str) -> bool {
        false
    }

    fn codex_delivered(&self, _: u64, _: &str) -> Result<CodexDelivered, String> {
        let generation_since = Some(Utc::now() - chrono::TimeDelta::hours(1));
        let (frontier, receipts) = (None, Vec::new());
        Ok(CodexDelivered {
            generation_since,
            frontier,
            receipts,
        })
    }

    fn codex_closures(
        &self,
        _: u64,
    ) -> futures::future::BoxFuture<'static, crate::services::tui_o::writer::adoption::Closures>
    {
        let closed = self.closed.lock().unwrap().clone();
        Box::pin(async move { Ok(closed) })
    }

    fn close_unplaced(
        &self,
        _: u64,
        source: &SourceId,
        (from, until): (u64, u64),
    ) -> futures::future::BoxFuture<'static, Result<(), String>> {
        self.unplaced.lock().unwrap().push((from, until));
        let range = (source.clone(), from, Some(until));
        self.closed.lock().unwrap().push(range);
        Box::pin(async { Ok(()) })
    }
}

/// A host over a rollout bound ten seconds ago with no Legacy record; the turn holding `anchor`
/// was closed as a discard records it, by its native opener and end.
async fn hosted_closed(
    harness: &Harness,
    source: &SourceId,
    anchor: u64,
) -> (Arc<TestIo>, Hosts, Arc<Closed>) {
    let late = crate::services::tui_o::writer::late_adopt::native_turn;
    let (opened, until) = late(source, anchor).unwrap();
    let (io, ready) = (TestIo::over(harness), Arc::new(Readiness::default()));
    io.late_enabled.store(true, Ordering::SeqCst);
    *io.custody.lock().unwrap() = Ok(Custody::PendingStart);
    let legacy = Arc::new(Closed::default());
    legacy
        .closed
        .lock()
        .unwrap()
        .push((source.clone(), opened, until));
    *io.legacy.lock().unwrap() = Some(legacy.clone());
    let hosts = start_codex(harness, &io, &ready);
    polls(3).await;
    (io, hosts, legacy)
}

/// A reserved channel over a fresh rollout bound ten seconds ago, with no first admission.
fn bound_closed(harness: &Harness, channel: u64, name: &str) -> (PathBuf, SourceId) {
    harness.gate.acquired();
    p5::set_test_root(Some(harness._runtime.path()));
    let (path, source) = rollout(harness, name, &metadata("s-codex"));
    bind_late(channel, &path);
    bound_ago(harness, channel, chrono::TimeDelta::seconds(10));
    (path, source)
}

// A turn discarded with its reason is never taken: O starts after it, whether a next turn is
// already there or arrives later, and posts that next turn once.
#[tokio::test(start_paused = true)]
async fn a_turn_closed_with_its_reason_is_never_taken_and_the_next_turn_is_posted_once() {
    for (channel, next_first) in [(61_901_u64, true), (61_902, false)] {
        let harness = fresh_on(channel);
        let _reserved = reserve(channel);
        let (path, source) = bound_closed(&harness, channel, "rollout-closed.jsonl");
        let opener = len(&path);
        append(&path, &dated_turn("t1", "discarded answer", 8, 6));
        let next = len(&path);
        if next_first {
            append(&path, &dated_turn("t2", "next answer", 5, 3));
        }
        let (io, _hosts, _) = hosted_closed(&harness, &source, (opener + next) / 2).await;
        assert_eq!(
            adoption(channel),
            Adoption::Committed,
            "{:?}",
            io.alarms.halted()
        );
        assert_eq!(init_sources(&harness, channel), [(source, next)]);
        if !next_first {
            append(&path, &dated_turn("t2", "next answer", 2, 1));
        }
        polls(3).await;
        assert_eq!(posted(&harness, "discarded answer"), 0);
        assert_eq!(posted(&harness, "next answer"), 1);
    }
}

// An uncovered turn before a closed one is not left out silently: it is closed with its own
// reason once, the closed turn stays unposted, and the native session's next input is posted.
#[tokio::test(start_paused = true)]
async fn an_owed_turn_before_a_closed_one_gets_its_reason_and_the_next_input_is_posted() {
    const CODEX: u64 = 61_903;
    let harness = fresh_on(CODEX);
    let _reserved = reserve(CODEX);
    let (path, source) = bound_closed(&harness, CODEX, "rollout-straddle.jsonl");
    let earlier = len(&path);
    append(&path, &dated_turn("t0", "owed answer", 9, 8));
    let opener = len(&path);
    append(&path, &dated_turn("t1", "discarded answer", 7, 6));
    let next = len(&path);
    let candidate = test_override::with_channels(|boot| boot?.candidate(CODEX).cloned());
    assert!(
        candidate.unwrap().first_changes().borrow().is_none(),
        "no fresh admission"
    );
    let (io, _hosts, legacy) = hosted_closed(&harness, &source, (opener + next) / 2).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(*legacy.unplaced.lock().unwrap(), [(earlier, opener)]);
    assert_eq!(init_sources(&harness, CODEX), [(source, next)]);
    append(&path, &dated_turn("t2", "next answer", 2, 1));
    polls(3).await;
    assert_eq!(posted(&harness, "owed answer"), 0);
    assert_eq!(posted(&harness, "discarded answer"), 0);
    assert_eq!(posted(&harness, "next answer"), 1);
}

// A closed turn with no end record yet may still write: no init lands inside it, so its later
// records stay unposted, and the next input's opener is where O starts.
#[tokio::test(start_paused = true)]
async fn an_unended_closed_turn_takes_no_init_and_the_next_opener_is_taken() {
    const CODEX: u64 = 61_904;
    let harness = fresh_on(CODEX);
    let _reserved = reserve(CODEX);
    let (path, source) = bound_closed(&harness, CODEX, "rollout-open.jsonl");
    append(
        &path,
        &[opening(), answer("msg_x1", "discarded part")].concat(),
    );
    let (io, _hosts, _) = hosted_closed(&harness, &source, len(&path)).await;
    append(&path, &answer("msg_x2", "discarded tail"));
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Deferred,
        "{:?}",
        io.alarms.halted()
    );
    let next = len(&path);
    append(&path, &dated_turn("t2", "next answer", 2, 1));
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, next)]);
    polls(3).await;
    assert!(
        harness
            .port
            .posts()
            .iter()
            .all(|post| !post.starts_with("discarded"))
    );
    assert_eq!(posted(&harness, "next answer"), 1);
}
