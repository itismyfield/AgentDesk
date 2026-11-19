//! A takeover never starts on a turn a rejection closed, and never leaves out a turn owed before one.

use super::*;

/// Legacy holding no record of a generation that began an hour ago, and the turns rejections closed.
struct Closed(Vec<(SourceId, u64)>);

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
    ) -> futures::future::BoxFuture<'static, Result<Vec<(SourceId, u64)>, String>> {
        let closed = self.0.clone();
        Box::pin(async move { Ok(closed) })
    }
}

/// A host over a rollout whose turns ended before the baseline with no Legacy record;
/// the turn holding `anchor` was closed as a discard records it, by its native opener.
async fn hosted_closed(harness: &Harness, source: &SourceId, anchor: u64) -> (Arc<TestIo>, Hosts) {
    let late = crate::services::tui_o::writer::late_adopt::native_turn;
    let (opened, _) = late(source, anchor).unwrap();
    let (io, ready) = (TestIo::over(harness), Arc::new(Readiness::default()));
    io.late_enabled.store(true, Ordering::SeqCst);
    *io.custody.lock().unwrap() = Ok(Custody::PendingStart);
    *io.legacy.lock().unwrap() = Some(Arc::new(Closed(vec![(source.clone(), opened)])));
    let hosts = start_codex(harness, &io, &ready);
    polls(3).await;
    (io, hosts)
}

// A turn discarded with its reason is never taken: O starts after it, whether a next turn is
// already there or arrives later, and posts that next turn once.
#[tokio::test(start_paused = true)]
async fn a_turn_closed_with_its_reason_is_never_taken_and_the_next_turn_is_posted_once() {
    for (channel, next_first) in [(61_901_u64, true), (61_902, false)] {
        let harness = fresh_on(channel);
        harness.gate.acquired();
        let _reserved = reserve(channel);
        p5::set_test_root(Some(harness._runtime.path()));
        let (path, source) = rollout(&harness, "rollout-closed.jsonl", &metadata("s-codex"));
        bind_late(channel, &path);
        bound_ago(&harness, channel, chrono::TimeDelta::seconds(10));
        let opener = len(&path);
        append(&path, &dated_turn("t1", "discarded answer", 8, 6));
        let next = len(&path);
        if next_first {
            append(&path, &dated_turn("t2", "next answer", 5, 3));
        }
        let (io, _hosts) = hosted_closed(&harness, &source, (opener + next) / 2).await;
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

// A turn owed before a closed one is neither left out nor reached past: the takeover waits, so
// nothing of the closed turn is posted and the earlier turn stays owed.
#[tokio::test(start_paused = true)]
async fn an_owed_turn_before_a_closed_one_holds_the_takeover_instead_of_being_left_out() {
    const CODEX: u64 = 61_903;
    let harness = fresh_on(CODEX);
    harness.gate.acquired();
    let _reserved = reserve(CODEX);
    p5::set_test_root(Some(harness._runtime.path()));
    let (path, source) = rollout(&harness, "rollout-straddle.jsonl", &metadata("s-codex"));
    bind_late(CODEX, &path);
    bound_ago(&harness, CODEX, chrono::TimeDelta::seconds(10));
    append(&path, &dated_turn("t0", "owed answer", 9, 8));
    let opener = len(&path);
    append(&path, &dated_turn("t1", "discarded answer", 7, 6));
    let next = len(&path);
    append(&path, &dated_turn("t2", "next answer", 5, 3));
    let (io, _hosts) = hosted_closed(&harness, &source, (opener + next) / 2).await;
    polls(3).await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Deferred,
        "{:?}",
        io.alarms.halted()
    );
    assert!(harness.store.read_init(CODEX).unwrap().is_none());
    assert!(harness.port.posts().is_empty());
}
