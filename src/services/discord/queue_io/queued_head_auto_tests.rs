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
        let body = |record: String| {
            let (header, after) = crate::services::tui_input::human_frame::parse(&record)
                .expect("a receipt contains the owned author header");
            assert_eq!((header.name, header.author), (AUTHOR_NAME, "200"));
            after
                .strip_prefix(' ')
                .or_else(|| after.strip_prefix('\n'))
                .expect("one header/body separator")
                .to_string()
        };
        self.pane.records().into_iter().map(body).collect()
    }

    /// Waits for `count` receipts, clearing the composer after each as the TUI does. An input
    /// a veto deferred may take up to `checks` real slow checks.
    async fn typed(&self, count: usize, checks: usize, what: &str) {
        for check in 0..=checks {
            if check > 0 {
                self.slow_check().await;
            }
            let mut seen = 0;
            let wait = async {
                loop {
                    let now = self.pane.records().len();
                    if now > seen {
                        seen = now;
                        self.submit_composer();
                    }
                    if now >= count {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            if tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .is_ok()
            {
                assert_eq!(self.pane.records().len(), count, "{what}");
                return;
            }
        }
        let typed = self.pane.records().len();
        panic!(
            "{what}: {typed} of {count} typed; attempts {:?}",
            self.rows()
        );
    }

    /// Waits until the channel's automatic attempts have reported `count` rows.
    async fn attempts(&self, count: usize, what: &str) {
        until(what, || self.rows().len() >= count).await;
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
        let transcript = self.pane.path(InjectPane::TRANSCRIPT);
        let state = crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&transcript);
        assert!(state.is_busy(), "the pane stayed busy throughout");
    }

    fn retry_record(&self, id: MessageId) -> std::path::PathBuf {
        let root = runtime_store::discord_busy_followup_retries_root().unwrap();
        root.join(format!("claude/{}/{id}.json", self.channel))
    }
}

/// An enqueue alone reaches a busy pane before any 60-second timer, and inputs queued behind a
/// ready one are typed after it, in order, each once.
#[tokio::test(flavor = "current_thread")]
async fn s3_an_enqueue_alone_types_the_busy_channel_backlog_in_order_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::build(6_813_501, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let h = fixture.arrive_held("H waits for the pane").await;
    fixture
        .typed(1, 0, "the enqueue alone must reach the busy pane")
        .await;
    assert_eq!(fixture.receipts(), ["H waits for the pane"]);
    assert_eq!(
        backstop::completed_fires(fixture.channel),
        0,
        "no slow check had run"
    );
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    assert!(fixture.queue().await.is_empty());
    fixture.assert_still_busy().await;

    let transition = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    fixture.enqueue("B next").await;
    // C arrives while B is queued: it waits behind B rather than being typed ahead of it.
    fixture.enqueue("C behind B").await;
    let queue = fixture.queue().await;
    let (b, c) = (queue[0].message_id, queue[1].message_id);
    drop(transition);
    fixture
        .typed(3, 2, "the inputs behind a ready one follow it")
        .await;
    assert_eq!(
        fixture.receipts(),
        ["H waits for the pane", "B next", "C behind B"]
    );
    until("B and C are settled", || {
        fixture.terminal() == [h.get(), b.get(), c.get()]
    })
    .await;
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
    fixture
        .attempts(2, "both enqueues tried while the transition was held")
        .await;
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
    fixture.typed(1, 0, "the first slow check types H").await;
    assert_eq!(fixture.receipts(), ["H vetoed first"]);
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    fixture.slow_check().await;
    fixture.typed(2, 0, "the second slow check types B").await;
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
    fixture
        .attempts(2, "both enqueues tried while the transition was held")
        .await;
    drop(transition);

    fixture.slow_check().await;
    fixture
        .typed(1, 0, "a ready input queued for six minutes is still typed")
        .await;
    fixture.slow_check().await;
    fixture.typed(2, 0, "and so is the one behind it").await;
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

    let transition = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    fixture.enqueue("H behind parked P").await;
    fixture.enqueue("B behind H").await;
    let queue = fixture.queue().await;
    let (h, b) = (queue[1].message_id, queue[2].message_id);
    drop(transition);
    fixture
        .typed(
            2,
            2,
            "ready inputs behind a parked one are reached in order",
        )
        .await;
    assert_eq!(fixture.receipts(), ["H behind parked P", "B behind H"]);
    until("H and B are settled", || {
        fixture.terminal() == [h.get(), b.get()]
    })
    .await;
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

/// A slow check that fires while an enqueue's owner holds the reservation types nothing, and the
/// tail that enqueue left is reached by the next slow check with no new input.
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
    fixture.typed(1, 0, "the owner types H once").await;
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    assert_eq!(fixture.receipts(), ["H raced by two workers"]);
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    until("H's owner finished reporting", || {
        fixture.rows().iter().any(|row| row.outcome == "injected")
    })
    .await;

    // B's own enqueue only typed H; nothing new arrives, and the backstop left armed reaches B.
    let b = fixture.queue().await[0].message_id;
    fixture.slow_check().await;
    fixture
        .typed(2, 0, "the tail is reached without a new input")
        .await;
    until("B is settled", || fixture.terminal() == [h.get(), b.get()]).await;
    assert_eq!(
        fixture.receipts(),
        ["H raced by two workers", "B behind raced H"]
    );
    assert!(fixture.queue().await.is_empty());
    fixture.assert_still_busy().await;
}

/// With the switch, the mode or the channel gate closed, every automatic entry point leaves the
/// queue file's bytes, the holder and the pane as they were, arms nothing and emits no attempt.
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
        .typed(1, 0, "the reconcile reaches a busy channel's backlog")
        .await;
    until("H is settled", || fixture.terminal() == [h.get()]).await;
    fixture.slow_check().await;
    fixture
        .typed(2, 0, "the reconcile left the tail a slow check")
        .await;
    until("B is settled", || fixture.terminal() == [h.get(), b.get()]).await;
    assert_eq!(fixture.receipts(), ["status?", "distinct tail B"]);
    fixture.assert_still_busy().await;
}

/// A row names a delivery or a kept head only where the actor committed it: an unsettled paste is
/// unknown, an idle pane or a head settled before the claim is unavailable, and no row has text.
#[tokio::test(flavor = "current_thread")]
async fn s3_an_attempt_row_never_reports_a_delivery_the_actor_did_not_commit_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    // One fixture at a time: each owns the thread's REST mock and its own database.
    let unknown_rows = {
        let fixture = Fixture::new(6_813_531, "all").await;
        let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
        let h = fixture.queue().await[0].message_id;
        let (after_effect, resume) =
            inject_hook::park_queued(fixture.channel.get(), "after_effect");
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
        blocked
    };

    let idle = Fixture::new(6_813_532, "all").await;
    let _idle_gate = router::busy_inject_support::open_gate(idle.channel.get());
    let transcript = std::fs::read_to_string(idle.pane.path(InjectPane::TRANSCRIPT)).unwrap();
    let ended = format!("{transcript}{{\"type\":\"result\"}}\n");
    idle.pane.set(InjectPane::TRANSCRIPT, &ended);
    idle.pane.idle();
    queue_io::schedule_post_enqueue_idle_queue_kick(idle.shared.clone(), CLAUDE, idle.channel);
    until("the idle attempt was reported", || !idle.rows().is_empty()).await;
    let row = &idle.rows()[0];
    assert_eq!(
        (row.outcome, row.reason, row.head_state),
        ("not_sent", "not_busy", "unavailable"),
        "the idle pane vetoed before any claim"
    );
    // The queued-only lane ends at the paste even where an idle start would now be allowed: it
    // never reaches the dequeue an idle kickoff would take next.
    crate::services::discord::health::end_turn(&idle.shared, idle.channel.get()).await;
    let _ready = router::set_hosted_tui_promote_busy_for_tests(false);
    let dequeue =
        queue_dispatch::kickoff::cancel_backstop_test_support::DequeueRace::install(idle.channel);
    let before = idle.bytes();
    let reason = "idle_queue_backstop";
    let lane = queue_io::kick_queued_only_channel_if_context_available(
        &idle.shared,
        &CLAUDE,
        idle.channel,
        reason,
    );
    tokio::select! {
        () = lane => {}
        arrived = dequeue.wait_for_both(Duration::from_secs(20)) => {
            panic!("the queued-only lane reached the idle dequeue: {arrived:?}")
        }
    }
    assert_eq!(
        idle.bytes(),
        before,
        "the queued-only lane dequeued nothing"
    );
    let snapshot = idle.shared.mailbox(idle.channel).snapshot().await;
    assert_eq!(snapshot.intervention_queue.len(), 2);
    assert!(snapshot.pending_user_dispatch.is_none());
    let text = format!("{unknown_rows:?}{:?}", idle.rows());
    assert!(!text.contains("status?") && !text.contains("distinct tail B"));
    drop(idle);
    drop(_idle_gate);

    let raced = Fixture::build(6_813_533, "all").await;
    let _raced_gate = router::busy_inject_support::open_gate(raced.channel.get());
    let transition = raced
        .shared
        .session_transition_lock(raced.channel)
        .lock_owned()
        .await;
    queue_io::with_post_enqueue_idle_queue_kick_suppressed(raced.enqueue("H settled elsewhere"))
        .await;
    drop(transition);
    let h = raced.queue().await[0].clone();
    // The attempt chooses H, then waits on the core lock before its claim.
    let core = raced.shared.core.lock().await;
    let attempt = raced.spawn_kick();
    until("the attempt chose H", || {
        OFFERED_HEADS.lock().unwrap().contains(&h.message_id)
    })
    .await;
    let mailbox = raced.shared.mailbox(raced.channel);
    let context = || queue_dispatch::persistence_context(&raced.shared, &CLAUDE, raced.channel);
    let gate = router::queued_head_inject_covers;
    let claim = mailbox.claim_queued_head_if_enabled(
        QueueIdentity::of(&h),
        context(),
        raced.channel,
        None,
        gate,
    );
    let QueuedHeadClaim::Reserved(ticket) = claim.await else {
        panic!("the other worker reserves H");
    };
    use crate::services::turn_orchestrator::{HeadOutcome, HeadSettle};
    let may = mailbox.settle_queued_head(ticket, HeadOutcome::MayHaveSent, context());
    let HeadSettle::Committed(Some(ticket)) = may.await else {
        panic!("MayHaveSent was not committed");
    };
    let done = mailbox.settle_queued_head(ticket, HeadOutcome::Injected, context());
    assert!(matches!(done.await, HeadSettle::Committed(None)));
    assert_eq!(raced.terminal(), [h.message_id.get()]);
    drop(core);
    attempt.await.unwrap();
    let rows: Vec<_> = raced
        .rows()
        .iter()
        .map(|row| (row.head_message_id, row.outcome, row.head_state))
        .collect();
    assert_eq!(
        rows,
        [(h.message_id.get(), "not_sent", "unavailable")],
        "this attempt never kept the head the other worker settled"
    );
    assert_eq!(raced.pane.tmux_calls(), 0);
}

/// An input of a shape the pane is never given, queued as the first ready one.
fn untypable(channel: ChannelId, shape: &str) -> Intervention {
    use crate::services::turn_orchestrator::{
        InterventionMode, SourceMessageQueuedGeneration as Source, SourceMessageTextSegment,
    };
    let id = MessageId::new(channel.get() + 900);
    let generation = runtime_store::process_generation();
    let mut item = Intervention {
        author_id: UserId::new(200),
        author_is_bot: false,
        message_id: id,
        queued_generation: generation,
        source_message_ids: vec![id],
        source_message_queued_generations: vec![Source::user_instruction(id, generation)],
        source_text_segments: Vec::new(),
        text: format!("H shaped as {shape}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    };
    match shape {
        "command" => item.text = "/compact now".into(),
        "composite" => {
            let second = MessageId::new(id.get() + 1);
            item.source_message_ids.push(second);
            let source = Source::user_instruction(second, generation);
            item.source_message_queued_generations.push(source);
            item.text = "first\nsecond".into();
            let segment = |message_id, text: &str| SourceMessageTextSegment {
                message_id,
                text: text.into(),
            };
            item.source_text_segments = vec![segment(id, "first"), segment(second, "second")];
        }
        _ => item.pending_uploads = vec!["s3-upload.png".into()],
    }
    item
}

/// A command, merged or upload-bearing first ready input is never pasted and no plain input
/// behind it is pasted in its place; a slow check that finds it ends rather than staying armed.
#[tokio::test(flavor = "current_thread")]
async fn s3_an_untypable_first_ready_input_is_neither_pasted_nor_passed_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, shape) in [
        (6_813_551, "command"),
        (6_813_552, "composite"),
        (6_813_553, "upload"),
    ] {
        let fixture = Fixture::build(channel, "all").await;
        let _gate = router::busy_inject_support::open_gate(channel);
        let enqueue = crate::services::discord::mailbox_enqueue_intervention;
        let head = untypable(fixture.channel, shape);
        let outcome = enqueue(&fixture.shared, &CLAUDE, fixture.channel, head.clone()).await;
        assert!(
            outcome.enqueued && outcome.persistence_error.is_none(),
            "{shape}"
        );
        let b = fixture.arrive_held("plain B behind").await;
        quiet().await;
        let queue = fixture.queue().await;
        let ids: Vec<MessageId> = queue.iter().map(|item| item.message_id).collect();
        assert_eq!(ids, [head.message_id, b], "{shape}");
        assert_eq!(
            input_class(&CLAUDE, fixture.channel, &queue[0]),
            RetryClass::Ready,
            "{shape}: H is the first ready input"
        );
        assert!(
            !backstop::backstop_waiting(fixture.channel),
            "{shape}: the enqueues armed nothing"
        );
        // A slow check another producer armed finds nothing to paste and ends.
        let armed = queue_io::arm_slow_idle_queue_backstop_if_queue_nonempty(
            &fixture.shared,
            &CLAUDE,
            fixture.channel,
            "s3 untypable head producer",
        )
        .await;
        assert!(armed, "{shape}");
        fixture.slow_check().await;
        quiet().await;
        assert!(
            !backstop::backstop_waiting(fixture.channel),
            "{shape}: the slot ends"
        );
        assert_eq!(fixture.pane.tmux_calls(), 0, "{shape}: nothing was pasted");
        assert!(fixture.receipts().is_empty(), "{shape}");
        assert!(
            !OFFERED_HEADS.lock().unwrap().contains(&b),
            "{shape}: B never passed H"
        );
        let after: Vec<MessageId> = fixture.queue().await.iter().map(|i| i.message_id).collect();
        assert_eq!(after, ids, "{shape}");
        fixture.assert_still_busy().await;
    }
}

/// A thread gated by its parent alone stays watched while it cannot be read: the enqueue's look and
/// an armed check both wait, and the slow timer alone types each input once the read succeeds.
#[tokio::test(flavor = "current_thread")]
async fn s3_an_unreadable_thread_parent_keeps_the_backlog_watched_until_it_reads_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let parent = ChannelId::new(6_813_540);
    let fixture = Fixture::build_in(6_813_541, "all", Some(parent)).await;
    let _gate = router::busy_inject_support::open_gate(parent.get());
    assert!(!router::queued_head_inject_covers(fixture.channel, None));

    let refused = fixture.refuse_channel_reads(true);
    let h = fixture.arrive_held("H in a gated thread").await;
    until("the enqueue's look left the slow backstop armed", || {
        backstop::backstop_waiting(fixture.channel)
    })
    .await;
    assert!(
        fixture.refuse_channel_reads(true) > refused,
        "the look did fail to read"
    );
    assert_eq!(fixture.pane.tmux_calls(), 0);
    fixture.refuse_channel_reads(false);
    fixture.slow_check().await;
    fixture
        .typed(1, 0, "the slow timer types H once the thread reads")
        .await;
    until("H is settled", || fixture.terminal() == [h.get()]).await;

    // B is offered while the transition is held, so its veto leaves the backstop armed.
    let transition = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    fixture.enqueue("B in a gated thread").await;
    fixture.attempts(2, "B was offered and vetoed").await;
    until("the veto left the slow backstop armed", || {
        backstop::backstop_waiting(fixture.channel)
    })
    .await;
    let b = fixture.queue().await[0].message_id;
    let refused = fixture.refuse_channel_reads(true);
    drop(transition);
    fixture.slow_check().await;
    until("the armed check that could not read keeps its slot", || {
        backstop::backstop_waiting(fixture.channel)
    })
    .await;
    assert!(
        fixture.refuse_channel_reads(true) > refused,
        "the check did fail to read"
    );
    assert_eq!(fixture.receipts(), ["H in a gated thread"]);
    fixture.refuse_channel_reads(false);
    fixture.slow_check().await;
    fixture
        .typed(2, 0, "the slow timer types B once the thread reads")
        .await;
    until("B is settled", || fixture.terminal() == [h.get(), b.get()]).await;
    assert_eq!(
        fixture.receipts(),
        ["H in a gated thread", "B in a gated thread"]
    );
    let injected = fixture
        .rows()
        .into_iter()
        .filter(|row| row.outcome == "injected");
    let injected: Vec<u64> = injected.map(|row| row.head_message_id).collect();
    assert_eq!(injected, [h.get(), b.get()]);
    fixture.assert_still_busy().await;
}

#[path = "queued_head_mend_tests.rs"]
mod mend;
