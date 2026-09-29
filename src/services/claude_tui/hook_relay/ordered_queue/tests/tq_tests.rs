//! A hook that waits for a reply goes through the real queue, worker and receiver: its reply
//! window ends with the caller's wait, while the observation keeps retrying until durable.

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard};

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::hook_registry::{self, RegistryKey};
use crate::services::claude_tui::hook_server::adoption_retry::reset_deferred_adoptions_for_tests;
use crate::services::claude_tui::hook_server::{
    HookEvent, HookEventKind, HookServerState, hook_receiver_router_with_state,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    APPEND_FAULT, BindingEvent, BindingTarget, binding_events_since, set_test_root,
};
use crate::services::tui_prompt_dedupe::{
    TEST_LOCK, TuiRuntimeBinding, lock_claude_session_rotations_for_tests,
    register_provider_session, register_tmux_channel, register_tmux_runtime_binding,
    reset_state_for_tests,
};

const REPLY_WAIT: Duration = Duration::from_millis(750);

/// `(request id, hook event, status, detached)` of every request the proxy handed over.
type Seen = Arc<Mutex<Vec<(String, String, u16, bool)>>>;

/// Process-wide state a TQ test owns; taken outside the async body that drives the receiver.
struct TqLocks {
    _runtime_root: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
    _rotations: MutexGuard<'static, ()>,
    _state: MutexGuard<'static, ()>,
}

impl TqLocks {
    fn take() -> Self {
        // The ROOT guard holds the shared env lock, so it comes before `TEST_LOCK` (env -> dedupe).
        let root = tempfile::tempdir().unwrap();
        let runtime_root = crate::config::set_agentdesk_root_for_test(root.path());
        let state = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let rotations = lock_claude_session_rotations_for_tests();
        Self {
            _runtime_root: runtime_root,
            _root: root,
            _rotations: rotations,
            _state: state,
        }
    }
}

/// One pane bound to transcript A, a receiver behind the proxy, and an in-process worker.
struct Tq {
    _log_root: tempfile::TempDir,
    dir: tempfile::TempDir,
    state: HookServerState,
    endpoint: String,
    seen: Seen,
    channel: u64,
    a: String,
    b: String,
    queue_dir: PathBuf,
    stop_worker: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Tq {
    async fn new(channel: u64, tmux: &str) -> Self {
        reset_state_for_tests();
        reset_deferred_adoptions_for_tests();
        let log_root = tempfile::tempdir().unwrap();
        set_test_root(Some(log_root.path()));
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let a_path = dir.path().join(format!("{a}.jsonl"));
        std::fs::write(&a_path, b"{}\n").unwrap();
        register_provider_session("claude", &a, tmux);
        register_tmux_channel(tmux, channel);
        register_tmux_runtime_binding(tmux, claude(&a_path, &a));

        let state = HookServerState::new();
        let app = hook_receiver_router_with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Seen::default();
        tokio::spawn(proxy_every_request(listener, app, seen.clone()));
        let queue_dir = relay_queue_dir("claude", &a).unwrap();
        let stop_worker = Arc::new(AtomicBool::new(false));
        let worker = spawn_worker_loop(queue_dir.clone(), stop_worker.clone());
        Self {
            _log_root: log_root,
            dir,
            state,
            endpoint,
            seen,
            channel,
            a,
            b,
            queue_dir,
            stop_worker,
            worker: Some(worker),
        }
    }

    fn b_path(&self) -> PathBuf {
        self.dir.path().join(format!("{}.jsonl", self.b))
    }

    fn payload(&self, session: &str, extra: Value) -> Value {
        let mut payload = json!({ "session_id": session, "transcript_path": self.b_path() });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        payload
    }

    fn non_wait(&self, event: &str, payload: Value) {
        handoff_non_wait_hook_event(&self.endpoint, "claude", event, &self.a, payload).unwrap();
    }

    /// Runs the waiting CLI handoff on its own thread, as the provider's hook process would.
    fn waiting_cli(
        &self,
        event: &str,
        payload: Value,
    ) -> std::thread::JoinHandle<(Duration, Result<Value, String>)> {
        let (endpoint, event, a) = (self.endpoint.clone(), event.to_owned(), self.a.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = handoff_ordered_hook_event_response_with_timeout(
                &endpoint, "claude", &event, &a, payload, REPLY_WAIT,
            );
            (started.elapsed(), result)
        })
    }

    /// Holds the producer lock like a slow concurrent enqueue, released after `hold`.
    fn delay_producers(&self, hold: Duration) -> std::thread::JoinHandle<()> {
        let lock = self.queue_dir.join("producer.lock");
        let held = lock_relay_queue_file_with_mode(&lock, false, true).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(hold);
            drop(held);
        })
    }

    fn accepted(&self, event: &str) -> Vec<(String, bool)> {
        let seen = self.seen.lock().unwrap();
        let hits = seen
            .iter()
            .filter(|(_, e, status, _)| e == event && *status == 202);
        hits.map(|(id, _, _, detached)| (id.clone(), *detached))
            .collect()
    }

    fn refused(&self, event: &str) -> usize {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .filter(|(_, e, status, _)| e == event && *status == 425)
            .count()
    }

    /// Waits up to 3s; the caller asserts the outcome so a mutant fails at its own assertion.
    async fn settled(&self, mut done: impl FnMut(&Self) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !done(self) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        done(self)
    }

    async fn until(&self, label: &str, done: impl FnMut(&Self) -> bool) {
        assert!(self.settled(done).await, "timed out waiting for {label}");
    }

    fn events(&self) -> Vec<BindingEvent> {
        binding_events_since(self.channel, 0).unwrap()
    }

    fn pending_b_seq(&self) -> Option<u64> {
        self.events().iter().find_map(|e| match &e.new {
            BindingTarget::Pending {
                payload_session_id, ..
            } if *payload_session_id == self.b => Some(e.seq),
            _ => None,
        })
    }

    fn quarantined(&self) -> usize {
        std::fs::read_dir(self.queue_dir.join("quarantine")).map_or(0, |d| d.count())
    }

    fn replies_left(&self) -> usize {
        std::fs::read_dir(self.queue_dir.join("responses")).map_or(0, |d| d.count())
    }

    fn feedback_pending(&self) -> usize {
        self.state.memento_feedback_pending_for_tests(&self.a)
    }

    /// A memento recall on A whose feedback the next attached Stop of A must flush.
    async fn seed_feedback(&self) {
        let recall = json!({
            "tool_name": "mcp__memento__recall",
            "tool_response": {"_meta": {"searchEventId": "4308"}}
        });
        let (status, _) = self.direct("PostToolUse", &self.a, recall).await;
        assert_eq!(status, 202);
        assert_eq!(self.feedback_pending(), 1);
    }

    async fn direct(&self, event: &str, command: &str, payload: Value) -> (u16, Value) {
        let app = hook_receiver_router_with_state(self.state.clone());
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/hooks/claude/{event}?session_id={command}"))
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }
}

impl Drop for Tq {
    fn drop(&mut self) {
        self.stop_worker.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        APPEND_FAULT.with(|fault| fault.set(None));
        set_test_root(None);
        reset_deferred_adoptions_for_tests();
        reset_state_for_tests();
    }
}

fn claude(path: &Path, session: &str) -> TuiRuntimeBinding {
    TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_owned()),
        last_offset: 0,
        relay_last_offset: None,
    }
}

async fn proxy_every_request(listener: tokio::net::TcpListener, app: Router, seen: Seen) {
    while let Ok((mut socket, _)) = listener.accept().await {
        let (request_id, path, status, body) =
            forward_actual_request(&mut socket, app.clone()).await;
        let event = path.split('?').next().unwrap_or_default();
        let event = event.rsplit('/').next().unwrap_or_default().to_owned();
        let detached = serde_json::from_slice::<Value>(&body).is_ok_and(|b| b["detached"] == true);
        seen.lock()
            .unwrap()
            .push((request_id, event, status.as_u16(), detached));
        answer_actual_request(&mut socket, status, &body).await;
    }
}

/// The production worker, kept alive for the test so no worker subprocess is needed.
fn spawn_worker_loop(queue_dir: PathBuf, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let request = OrderedHookRelayWorkerRequest {
                queue_dir: queue_dir.clone(),
            };
            let _ = run_ordered_hook_relay_worker(request);
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

fn buffered(session: &str) -> usize {
    let key = RegistryKey::new("claude", Some(session), None).unwrap();
    hook_registry::global().buffered_len(&key)
}

fn drained_kinds(rx: &mut tokio::sync::broadcast::Receiver<HookEvent>) -> Vec<HookEventKind> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|event| event.kind)
        .collect()
}

async fn join<T: Send + 'static>(handle: std::thread::JoinHandle<T>) -> T {
    join_within(handle, Duration::from_secs(10)).await.unwrap()
}

async fn join_within<T: Send + 'static>(
    handle: std::thread::JoinHandle<T>,
    limit: Duration,
) -> Option<T> {
    let deadline = Instant::now() + limit;
    while !handle.is_finished() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    handle.is_finished().then(|| handle.join().unwrap())
}

/// Pending B is logged, B's file appears, then Stop(B) is refused past the reply window.
async fn late_stop_after_refusals(tq: &Tq, producer_delay: Duration, fault_off_at: Duration) {
    tq.non_wait(
        "SessionStart",
        tq.payload(&tq.b, json!({ "source": "clear" })),
    );
    tq.until("Pending B", |tq| tq.pending_b_seq().is_some())
        .await;
    // SessionStart clears A's feedback, so it is seeded after the clear.
    tq.seed_feedback().await;
    std::fs::write(tq.b_path(), b"{}\n").unwrap();
    let (registry_before, mut rx) = ((buffered(&tq.a), buffered(&tq.b)), tq.state.subscribe());

    APPEND_FAULT.with(|fault| fault.set(Some("write")));
    let t0 = Instant::now();
    let producers = (!producer_delay.is_zero()).then(|| tq.delay_producers(producer_delay));
    let cli = tq.waiting_cli("Stop", tq.payload(&tq.b, json!({})));
    tokio::time::sleep(fault_off_at.saturating_sub(t0.elapsed())).await;
    APPEND_FAULT.with(|fault| fault.set(None));
    let cli = join_within(cli, Duration::from_secs(5)).await;
    if let Some(producers) = producers {
        join(producers).await;
    }
    tq.settled(|tq| !tq.accepted("Stop").is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (elapsed, result) = cli.expect("elapsed < 850ms (the CLI was still waiting after 5s)");
    assert!(
        elapsed < Duration::from_millis(850),
        "elapsed < 850ms (got {elapsed:?})"
    );
    assert!(
        result.is_err_and(|e| e.contains("timed out")),
        "CLI returns Err(timeout)"
    );
    assert_eq!(tq.quarantined(), 0, "quarantine empty");
    assert!(
        tq.refused("Stop") > 0,
        "the Stop was refused while the log failed"
    );
    let accepted = tq.accepted("Stop");
    assert_eq!(accepted.len(), 1, "Stop accepted once: {accepted:?}");
    let tail = tq.events().pop().unwrap().new;
    let pending_seq = tq.pending_b_seq().unwrap();
    assert!(
        matches!(tail, BindingTarget::Resolved { pending_seq: seq, .. } if seq == pending_seq),
        "log ends with Resolved of Pending B"
    );
    let (registry_after, kinds) = ((buffered(&tq.a), buffered(&tq.b)), drained_kinds(&mut rx));
    // A pending recall is reminded on two Stops, then dropped; a consumed late Stop spends one.
    let mut flushed = Vec::new();
    for _ in 0..2 {
        let (status, body) = tq.direct("Stop", &tq.a, json!({})).await;
        flushed.push(status == 202 && body.get("memento_tool_feedback_flush").is_some());
    }
    assert_eq!(
        flushed,
        [true, true],
        "feedback not consumed: both reminders stay for the next attached Stops"
    );
    assert_eq!(tq.replies_left(), 0, "response path absent");
    assert!(accepted[0].1, "the late Stop is delivered detached");
    assert_eq!(
        registry_after, registry_before,
        "late Stop reached no registry waiter"
    );
    assert!(
        !kinds.contains(&HookEventKind::Stop),
        "late Stop broadcast: {kinds:?}"
    );
}

#[test]
fn a_stop_refused_past_its_reply_window_is_delivered_detached_and_not_lost() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _locks = TqLocks::take();
    runtime.block_on(async {
        let tq = Tq::new(7_500, "tq1-late-stop").await;
        late_stop_after_refusals(&tq, Duration::ZERO, Duration::from_millis(1_000)).await;
    });
}

#[test]
fn the_reply_window_ends_with_the_callers_wait_even_when_enqueue_was_slow() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _locks = TqLocks::take();
    runtime.block_on(async {
        let tq = Tq::new(7_510, "tq1-slow-producer").await;
        let (delay, fault_off) = (Duration::from_millis(300), Duration::from_millis(900));
        late_stop_after_refusals(&tq, delay, fault_off).await;
    });
}

#[test]
fn a_refused_session_start_keeps_the_later_stop_behind_it_until_both_are_accepted() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _locks = TqLocks::take();
    runtime.block_on(async {
        let tq = Tq::new(7_520, "tq2-hol").await;
        APPEND_FAULT.with(|fault| fault.set(Some("write")));
        tq.non_wait(
            "SessionStart",
            tq.payload(&tq.b, json!({ "source": "clear" })),
        );
        tq.until("SessionStart refused", |tq| tq.refused("SessionStart") > 0)
            .await;
        let cli_started = Instant::now();
        let cli = tq.waiting_cli("Stop", tq.payload(&tq.b, json!({})));
        let fault_off = Duration::from_millis(1_000).saturating_sub(cli_started.elapsed());
        tokio::time::sleep(fault_off).await;
        APPEND_FAULT.with(|fault| fault.set(None));
        let (elapsed, result) = join(cli).await;
        tq.settled(|tq| !tq.accepted("Stop").is_empty()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            elapsed < Duration::from_millis(850),
            "CLI < 850ms (got {elapsed:?})"
        );
        assert!(
            result.is_err(),
            "the Stop outlives its reply window: {result:?}"
        );
        assert_eq!(
            tq.accepted("SessionStart").len(),
            1,
            "SessionStart accepted once"
        );
        assert_eq!(tq.accepted("Stop").len(), 1, "Stop accepted once");
        let seen = tq.seen.lock().unwrap().clone();
        let first = |event: &str| seen.iter().position(|(_, e, s, _)| e == event && *s == 202);
        assert!(
            first("SessionStart") < first("Stop"),
            "SessionStart then Stop"
        );
        assert_eq!(tq.quarantined(), 0, "quarantine empty");
        assert!(tq.pending_b_seq().is_some(), "Pending B logged");
    });
}

#[test]
fn a_late_stop_or_prompt_reaches_no_turn_consumer_while_an_attached_one_does() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _locks = TqLocks::take();
    runtime.block_on(async {
        let tq = Tq::new(7_530, "tq-f7-controls").await;
        let a = tq.a.clone();
        let prompt = json!({ "session_id": a, "prompt": "late prompt from an earlier turn" });
        let stop = json!({ "session_id": a });
        let mut rx = tq.state.subscribe();

        for (event, payload) in [("UserPromptSubmit", &prompt), ("Stop", &stop)] {
            // Enqueue outlives the caller's wait, so the reply window is already closed on arrival.
            let producers = tq.delay_producers(REPLY_WAIT + Duration::from_millis(50));
            let registry = buffered(&a);
            let (_, result) = join(tq.waiting_cli(event, payload.clone())).await;
            join(producers).await;
            assert!(result.is_err());
            tq.until("late hook accepted", |tq| !tq.accepted(event).is_empty())
                .await;
            assert!(tq.accepted(event)[0].1, "{event} delivered detached");
            assert_eq!(buffered(&a), registry, "late {event} reached the registry");
            assert!(
                drained_kinds(&mut rx).is_empty(),
                "late {event} was broadcast"
            );

            let result = join(tq.waiting_cli(event, payload.clone())).await.1;
            assert!(result.is_ok(), "attached {event} replies: {result:?}");
            assert_eq!(tq.accepted(event).len(), 2);
            assert!(!tq.accepted(event)[1].1, "{event} attached");
            let kinds = drained_kinds(&mut rx);
            assert_eq!(kinds.len(), 1, "attached {event} observed once: {kinds:?}");
        }
    });
}
