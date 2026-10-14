//! Reachability of the queued-head paste from the automatic entry points alone: inputs arrive
//! through the deliver route, and only the entry points themselves ever call the kickoff.

use std::time::Duration;

use super::*;
use crate::services::discord::busy_followup_retry_store::{RetryClass, input_class};
use crate::services::discord::queue_io::cancel_backstop_test_support as backstop;

const CLAUDE: ProviderKind = ProviderKind::Claude;

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let wait = async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    // Far below the 60-second backstop: whatever this waits for did not come from that timer.
    tokio::time::timeout(Duration::from_secs(20), wait)
        .await
        .unwrap_or_else(|_| panic!("{what}"));
}

/// Lets work already spawned finish; nothing asserted with it depends on the exact time.
async fn quiet() {
    tokio::time::sleep(Duration::from_millis(400)).await;
}

impl Fixture {
    /// One input through the real route while the session transition is held, so it is queued
    /// rather than typed on arrival; the transition is free again when this returns.
    async fn arrive_held(&self, text: &str) -> MessageId {
        let transition = self
            .shared
            .session_transition_lock(self.channel)
            .lock_owned()
            .await;
        self.enqueue(text).await;
        drop(transition);
        self.queue().await.last().unwrap().message_id
    }

    /// The pane's composer after it submitted a paste, as the real TUI leaves it.
    fn submit_composer(&self) {
        let _ = std::fs::remove_file(self.pane.path("pasted"));
    }

    /// Bodies the pane's transcript acknowledged, in order.
    fn receipts(&self) -> Vec<String> {
        let body = |record: String| record.split_once('\n').unwrap().1.to_string();
        self.pane.records().into_iter().map(body).collect()
    }

    async fn until_receipts(&self, count: usize, what: &str) {
        until(what, || self.pane.records().len() >= count).await;
        assert_eq!(self.pane.records().len(), count, "{what}");
    }

    /// One real slow-backstop check: its 60-second timer elapses, nothing wakes it by hand.
    async fn slow_check(&self) {
        let channel = self.channel;
        until("the slow backstop is armed and waiting", || {
            backstop::backstop_waiting(channel)
        })
        .await;
        let cycles = backstop::completed_fires(channel);
        tokio::time::pause();
        let delay = queue_io::DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY;
        tokio::time::advance(delay + Duration::from_secs(1)).await;
        tokio::time::resume();
        until("the slow backstop finished its check", || {
            backstop::completed_fires(channel) > cycles
        })
        .await;
    }

    fn terminal(&self) -> Vec<u64> {
        let disk = self.disk();
        let Some(terminal) = disk.get("terminal_identities").and_then(Value::as_array) else {
            return Vec::new();
        };
        let primary = |identity: &Value| identity["primary"].as_u64().unwrap();
        terminal.iter().map(primary).collect()
    }

    fn rows(&self) -> Vec<queue_io::queued_head_inject::AttemptRow> {
        let rows = ATTEMPT_ROWS.lock().unwrap();
        let mine = rows
            .iter()
            .filter(|row| row.channel_id == self.channel.get());
        mine.cloned().collect()
    }

    /// The foreground turn still holds the channel and no queued turn was started in its place.
    async fn assert_still_busy(&self) {
        self.assert_holder_kept().await;
        let snapshot = self.shared.mailbox(self.channel).snapshot().await;
        assert!(snapshot.pending_user_dispatch.is_none());
        let transcript = self.pane.path("transcript.jsonl");
        let state = crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&transcript);
        assert!(state.is_busy(), "the pane stayed busy throughout");
    }

    fn retry_record(&self, id: MessageId) -> std::path::PathBuf {
        let root = runtime_store::discord_busy_followup_retries_root().unwrap();
        root.join(format!("claude/{}/{id}.json", self.channel))
    }
}

/// An enqueue alone reaches a busy pane: H is typed before any 60-second timer, a later input
/// queues behind what is still ready, and the tail is reached with no further input at all.
#[tokio::test(flavor = "current_thread")]
async fn s3_an_enqueue_reaches_the_busy_pane_and_the_tail_follows_without_new_input_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::build(6_813_501, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let h = fixture.arrive_held("H waits for the pane").await;
    // H is ready, so B queues behind it; B's own enqueue is the only trigger left.
    fixture.enqueue("B behind H").await;
    fixture
        .until_receipts(1, "the enqueue alone must reach the busy pane")
        .await;
    assert_eq!(fixture.receipts(), ["H waits for the pane"]);
    assert_eq!(
        backstop::completed_fires(fixture.channel),
        0,
        "no slow check had run"
    );
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    let queue = fixture.queue().await;
    assert_eq!(queue.len(), 1);
    let b = queue[0].message_id;
    fixture.assert_still_busy().await;

    // C arrives while B is ready: it queues behind B, and its enqueue types B, not itself.
    fixture.submit_composer();
    fixture.enqueue("C behind B").await;
    fixture
        .until_receipts(2, "the next enqueue reaches the next ready input")
        .await;
    assert_eq!(fixture.receipts()[1], "B behind H");
    until("B is settled", || fixture.terminal() == [h.get(), b.get()]).await;
    let c = fixture.queue().await[0].message_id;

    // Nothing else arrives: the slow backstop the last paste left armed reaches C.
    fixture.submit_composer();
    fixture.slow_check().await;
    fixture
        .until_receipts(3, "the tail is reached without a new input")
        .await;
    assert_eq!(fixture.receipts()[2], "C behind B");
    until("C is settled", || {
        fixture.terminal() == [h.get(), b.get(), c.get()]
    })
    .await;
    assert!(fixture.queue().await.is_empty());
    let typed: Vec<(u64, &str, &str)> = fixture
        .rows()
        .iter()
        .filter(|row| row.outcome == "injected")
        .map(|row| (row.head_message_id, row.outcome, row.head_state))
        .collect();
    assert_eq!(
        typed,
        [
            (h.get(), "injected", "terminal"),
            (b.get(), "injected", "terminal"),
            (c.get(), "injected", "terminal"),
        ]
    );
    fixture.assert_still_busy().await;
}

/// The first attempts are vetoed by a real transition contention. Once it ends, two slow checks
/// type H and then B with no new input and no kick from the test.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_vetoed_first_attempt_is_retried_by_the_slow_backstop_h_then_b_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::build(6_813_502, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let transition = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    fixture.enqueue("H vetoed first").await;
    fixture.enqueue("B behind vetoed H").await;
    until("the vetoed attempt leaves the slow backstop armed", || {
        backstop::backstop_waiting(fixture.channel)
    })
    .await;
    assert_eq!(
        fixture.pane.tmux_calls(),
        0,
        "the veto came before the pane"
    );
    let queue = fixture.queue().await;
    let (h, b) = (queue[0].message_id, queue[1].message_id);
    drop(transition);

    fixture.slow_check().await;
    fixture
        .until_receipts(1, "the first slow check types H")
        .await;
    assert_eq!(fixture.receipts(), ["H vetoed first"]);
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    fixture.submit_composer();
    fixture.slow_check().await;
    fixture
        .until_receipts(2, "the second slow check types B")
        .await;
    assert_eq!(fixture.receipts()[1], "B behind vetoed H");
    until("B is settled", || fixture.terminal() == [h.get(), b.get()]).await;
    assert!(fixture.queue().await.is_empty());
    fixture.assert_still_busy().await;
}

/// One spent retry six minutes ago is not a cap: the classifier reads both inputs ready, and
/// the busy pane takes them. The two facts are asserted apart.
#[tokio::test(flavor = "current_thread")]
async fn s3_time_spent_queued_never_keeps_a_ready_input_from_the_busy_pane_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::build(6_813_504, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let transition = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    fixture.enqueue("H retried once long ago").await;
    fixture.enqueue("B retried once long ago").await;
    let queue = fixture.queue().await;
    let six_minutes_ago = chrono::Utc::now().timestamp_millis() as u64 - 360_000;
    for item in &queue {
        let record = fixture.retry_record(item.message_id);
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        let budget = json!({ "notice_message_id": 9, "busy_retry_count": 1,
            "first_busy_retry_at_ms": six_minutes_ago });
        std::fs::write(record, budget.to_string()).unwrap();
    }
    let classes: Vec<RetryClass> = queue
        .iter()
        .map(|item| input_class(&CLAUDE, fixture.channel, item))
        .collect();
    assert_eq!(classes, [RetryClass::Ready, RetryClass::Ready]);
    until("the slow backstop is armed", || {
        backstop::backstop_waiting(fixture.channel)
    })
    .await;
    drop(transition);

    fixture.slow_check().await;
    fixture
        .until_receipts(1, "a ready input queued for six minutes is still typed")
        .await;
    fixture.submit_composer();
    fixture.slow_check().await;
    fixture
        .until_receipts(2, "and so is the one behind it")
        .await;
    assert_eq!(
        fixture.receipts(),
        ["H retried once long ago", "B retried once long ago"]
    );
    fixture.assert_still_busy().await;
}

/// A parked input alone is never typed and stays on disk. Ready inputs that arrive behind it are
/// typed in order, each once, and the parked one is still queued afterwards.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_parked_input_is_kept_and_passed_by_the_ready_inputs_behind_it_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::build(6_813_505, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let transition = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    fixture.enqueue("P spent its retries").await;
    let p = fixture.queue().await[0].message_id;
    fixture.park(&[p]);
    drop(transition);
    quiet().await;
    let parked = payloads(items(fixture.disk()));
    if backstop::backstop_waiting(fixture.channel) {
        fixture.slow_check().await;
    }
    assert_eq!(
        fixture.pane.tmux_calls(),
        0,
        "a parked input is never typed"
    );
    assert_eq!(payloads(items(fixture.disk())), parked);

    let h = fixture.arrive_held("H behind parked P").await;
    fixture.enqueue("B behind H").await;
    fixture
        .until_receipts(1, "a ready input behind a parked one is reached")
        .await;
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    let b = fixture.queue().await[0].message_id;
    assert_ne!(b, p, "the ready tail leads the queue once H is settled");
    fixture.submit_composer();
    fixture.slow_check().await;
    fixture.until_receipts(2, "the tail follows").await;
    until("B is settled", || fixture.terminal() == [h.get(), b.get()]).await;
    assert_eq!(fixture.receipts(), ["H behind parked P", "B behind H"]);
    let left = fixture.queue().await;
    assert_eq!(left.len(), 1);
    assert_eq!(
        (left[0].message_id, left[0].text.as_str()),
        (p, "P spent its retries")
    );
    assert!(!OFFERED_HEADS.lock().unwrap().contains(&p));
    assert_eq!(
        input_class(&CLAUDE, fixture.channel, &left[0]),
        RetryClass::Parked
    );
    fixture.assert_still_busy().await;
}

/// A slow check that fires while an enqueue's owner holds the reservation types nothing: the
/// input is pasted once and settled once.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_slow_check_overlapping_a_reserved_owner_types_the_input_once_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::build(6_813_506, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let transition = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    fixture.enqueue("H raced by two workers").await;
    let h = fixture.queue().await[0].message_id;
    until("the slow backstop is armed", || {
        backstop::backstop_waiting(fixture.channel)
    })
    .await;
    drop(transition);
    let (at_reserved, resume) = inject_hook::park_before_reserve(fixture.channel.get());
    // B's enqueue starts the owner that reserves H and parks before the pane.
    fixture.enqueue("B behind raced H").await;
    reached(&at_reserved).await;
    assert_eq!(fixture.disk()["head_injection"]["phase"], "reserved");
    let offered = OFFERED_HEADS
        .lock()
        .unwrap()
        .iter()
        .filter(|id| **id == h)
        .count();
    fixture.slow_check().await;
    let again = OFFERED_HEADS
        .lock()
        .unwrap()
        .iter()
        .filter(|id| **id == h)
        .count();
    assert!(again > offered, "the slow check did offer the same input");
    assert_eq!(
        fixture.pane.tmux_calls(),
        0,
        "the second worker reached no pane"
    );
    assert_eq!(fixture.disk()["head_injection"]["phase"], "reserved");
    resume.notify_one();
    fixture.until_receipts(1, "the owner types H once").await;
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    assert_eq!(fixture.receipts(), ["H raced by two workers"]);
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    assert_eq!(fixture.queue().await.len(), 1);
    fixture.assert_still_busy().await;
}

/// With the switch, the mode or the channel gate closed, or another provider, every automatic
/// entry point leaves the queue file's bytes, the holder and the pane exactly as the enqueue left
/// them, arms nothing of its own and emits no attempt.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_closed_gate_changes_nothing_at_any_automatic_entry_point_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, mode, list_open, queued_open) in [
        (6_813_511, "all", true, false),
        (6_813_512, "off", true, true),
        (6_813_513, "all", false, true),
    ] {
        let fixture = Fixture::build(channel, mode).await;
        inject_hook::set_queued_head(channel, queued_open);
        let _gate = list_open.then(|| router::busy_inject_support::open_gate(channel));
        let transition = fixture
            .shared
            .session_transition_lock(fixture.channel)
            .lock_owned()
            .await;
        fixture.enqueue("H behind a closed gate").await;
        fixture.enqueue("B behind a closed gate").await;
        drop(transition);
        quiet().await;
        let case = format!("mode={mode} list={list_open} queued={queued_open}");
        assert!(
            !backstop::backstop_waiting(fixture.channel),
            "{case}: the enqueue armed nothing"
        );
        let (bytes, gets) = (fixture.bytes(), fixture.gets());
        let queue = format!("{:?}", fixture.queue().await);

        // Startup and listener reconcile over every channel.
        queue_io::spawn_turn_completion_idle_queue_listener(fixture.shared.clone(), CLAUDE);
        until("the listener reconciled", || {
            backstop::listener_completed_reconciles(fixture.channel) > 0
        })
        .await;
        quiet().await;
        assert!(!backstop::backstop_waiting(fixture.channel), "{case}");
        // A slow backstop another producer armed runs its own rule and ends.
        let armed = queue_io::arm_slow_idle_queue_backstop_if_queue_nonempty(
            &fixture.shared,
            &CLAUDE,
            fixture.channel,
            "s3 gate-off producer",
        )
        .await;
        assert!(armed, "{case}: the idle rule still arms on its own backlog");
        fixture.slow_check().await;
        quiet().await;
        assert!(
            !backstop::backstop_waiting(fixture.channel),
            "{case}: a busy channel's backstop ends as before"
        );

        assert_eq!(fixture.pane.tmux_calls(), 0, "{case}");
        assert_eq!(fixture.bytes(), bytes, "{case}");
        assert_eq!(format!("{:?}", fixture.queue().await), queue, "{case}");
        assert!(fixture.rows().is_empty(), "{case}");
        let ids: Vec<MessageId> = fixture.queue().await.iter().map(|i| i.message_id).collect();
        assert!(
            !OFFERED_HEADS
                .lock()
                .unwrap()
                .iter()
                .any(|id| ids.contains(id)),
            "{case}"
        );
        if !queued_open || mode == "off" {
            assert_eq!(
                fixture.gets(),
                gets,
                "{case}: the closed switch reads nothing"
            );
        }
        let snapshot = fixture.shared.mailbox(fixture.channel).snapshot().await;
        let other = queue_io::auto_queue_eligibility::queued_reach(
            Some(&fixture.http),
            &fixture.shared,
            &ProviderKind::Codex,
            fixture.channel,
            &snapshot,
        )
        .await;
        assert_eq!(other, queue_io::auto_queue_eligibility::QueuedReach::Closed);
        fixture.assert_still_busy().await;
    }
}

/// A backlog restored with no new input is found by the listener's first reconcile while the
/// channel stays busy, and the tail follows at the slow check that reconcile left armed.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_restored_backlog_is_reached_by_the_reconcile_alone_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_521, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let queue = fixture.queue().await;
    let (h, b) = (queue[0].message_id, queue[1].message_id);
    assert!(!backstop::backstop_waiting(fixture.channel));
    queue_io::spawn_turn_completion_idle_queue_listener(fixture.shared.clone(), CLAUDE);
    fixture
        .until_receipts(1, "the reconcile reaches a busy channel's backlog")
        .await;
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    fixture.submit_composer();
    fixture.slow_check().await;
    fixture
        .until_receipts(2, "the reconcile left the tail a slow check")
        .await;
    until("B is settled", || fixture.terminal() == [h.get(), b.get()]).await;
    assert_eq!(fixture.receipts(), ["status?", "distinct tail B"]);
    fixture.assert_still_busy().await;
}

/// The attempt row names a delivery only where the actor committed it. A paste the pane took but
/// the queue could not settle reads unknown, an idle pane reads not sent, and no row carries text.
#[tokio::test(flavor = "current_thread")]
async fn s3_an_attempt_row_never_reports_a_delivery_the_actor_did_not_commit_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_531, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let h = fixture.queue().await[0].message_id;
    let (after_effect, resume) = inject_hook::park_queued(fixture.channel.get(), "after_effect");
    queue_io::schedule_post_enqueue_idle_queue_kick(
        fixture.shared.clone(),
        CLAUDE,
        fixture.channel,
    );
    reached(&after_effect).await;
    assert!(fixture.pane.transcript_recorded_the_paste());
    assert!(fixture.rows().is_empty(), "no row precedes the settlement");
    crate::services::turn_orchestrator::test_support::fail_queue_saves(fixture.channel, 1);
    resume.notify_one();
    until("the attempt was reported", || !fixture.rows().is_empty()).await;
    let rows = fixture.rows();
    assert_eq!(
        (rows[0].head_message_id, rows[0].outcome, rows[0].head_state),
        (h.get(), "unconfirmed", "unknown"),
        "the pane took the paste but the actor never committed it"
    );
    assert_eq!(fixture.disk()["head_injection"]["phase"], "unknown");

    // The unknown head is never typed again and nothing behind it is typed in its place.
    let calls = fixture.pane.tmux_calls();
    fixture.slow_check().await;
    quiet().await;
    assert_eq!(fixture.pane.tmux_calls(), calls);
    assert_eq!(fixture.receipts(), ["status?"]);
    let blocked = fixture.rows();
    let last = blocked.last().unwrap();
    assert_eq!((last.outcome, last.head_state), ("blocked", "unknown"));

    let idle = Fixture::new(6_813_532, "all").await;
    let _idle_gate = router::busy_inject_support::open_gate(idle.channel.get());
    let transcript = std::fs::read_to_string(idle.pane.path("transcript.jsonl")).unwrap();
    let ended = format!("{transcript}{{\"type\":\"result\"}}\n");
    idle.pane.set("transcript.jsonl", &ended);
    idle.pane.idle();
    queue_io::schedule_post_enqueue_idle_queue_kick(idle.shared.clone(), CLAUDE, idle.channel);
    until("the idle attempt was reported", || !idle.rows().is_empty()).await;
    let row = &idle.rows()[0];
    assert_eq!(
        (row.outcome, row.reason, row.head_state),
        ("not_sent", "not_busy", "queued")
    );
    let text = format!("{:?}{:?}", fixture.rows(), idle.rows());
    assert!(!text.contains("status?") && !text.contains("distinct tail B"));
}
