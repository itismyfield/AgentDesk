//! A Claude SessionStart whose send fails is sent again as the same request until it is judged;
//! every other hook keeps the marker-and-complete outcome, and a receipt replay judges nothing.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;

use super::*;
use crate::services::claude_tui::hook_relay::transport_retry::retries;
use crate::services::tui_prompt_dedupe::binding_events::BindingCause;
use crate::services::tui_prompt_dedupe::{
    clear_claude_session_rotation, runtime_binding_for_tmux_session,
};

/// What the proxy does with one request: hand it to the receiver, drop the connection, or answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    Forward,
    Drop,
    Status(u16),
}

/// `(provider, event, attempt from 1)` of a request decides its fate.
type Rule = Arc<dyn Fn(&str, &str, usize) -> Fate + Send + Sync>;

/// `(request id, provider, event, published at, deadline, fate)` of every request the proxy read.
type Attempts = Arc<Mutex<Vec<(String, String, String, String, String, Fate)>>>;

/// A Claude pane on A behind a proxy whose rule can fail sends, with one worker per queue.
struct Retry {
    _log_root: tempfile::TempDir,
    dir: tempfile::TempDir,
    state: HookServerState,
    endpoint: String,
    attempts: Attempts,
    tmux: String,
    a: String,
    workers: Vec<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

impl Retry {
    async fn new(channel: u64, tmux: &str, rule: Rule) -> Self {
        reset_state_for_tests();
        reset_deferred_adoptions_for_tests();
        let log_root = tempfile::tempdir().unwrap();
        set_test_root(Some(log_root.path()));
        let dir = tempfile::tempdir().unwrap();
        let a = uuid::Uuid::new_v4().to_string();
        let marker = crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, uuid::Uuid::new_v4().simple().to_string()).unwrap();
        let retry = |dir: tempfile::TempDir, state, endpoint, attempts| Self {
            _log_root: log_root,
            dir,
            state,
            endpoint,
            attempts,
            tmux: tmux.to_owned(),
            a: a.clone(),
            workers: Vec::new(),
        };
        let state = HookServerState::new();
        let app = hook_receiver_router_with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let attempts = Attempts::default();
        tokio::spawn(proxy_with_rule(listener, app, attempts.clone(), rule));
        let mut retry = retry(dir, state, endpoint, attempts);
        let a_path = retry.transcript(&retry.a.clone());
        register_provider_session("claude", &retry.a, tmux);
        register_tmux_channel(tmux, channel);
        register_tmux_runtime_binding(tmux, claude(&a_path, &retry.a));
        retry.worker("claude", &retry.a.clone());
        retry
    }

    fn transcript(&self, session: &str) -> PathBuf {
        let path = self.dir.path().join(format!("{session}.jsonl"));
        std::fs::write(&path, format!("{{\"sessionId\":\"{session}\"}}\n")).unwrap();
        path
    }

    fn path(&self, session: &str) -> PathBuf {
        self.dir.path().join(format!("{session}.jsonl"))
    }

    fn worker(&mut self, provider: &str, command: &str) {
        let queue = relay_queue_dir(provider, command).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        self.workers
            .push((stop.clone(), spawn_worker_loop(queue, stop)));
    }

    fn send(&self, provider: &str, event: &str, command: &str, payload: Value) {
        handoff_non_wait_hook_event(&self.endpoint, provider, event, command, payload).unwrap();
    }

    fn hook(&self, event: &str, session: &str, source: Option<&str>) {
        let payload = json!({ "session_id": session, "transcript_path": self.path(session),
            "source": source });
        self.send("claude", event, &self.a, payload);
    }

    fn bound(&self) -> Option<String> {
        runtime_binding_for_tmux_session(&self.tmux).and_then(|b| b.session_id)
    }

    /// Waits for `session` to be bound, then drains the rotation as the delivery consumer would.
    async fn adopted(&self, session: &str) {
        self.until(session, |tq| tq.bound().as_deref() == Some(session))
            .await;
        clear_claude_session_rotation(&self.tmux);
    }

    fn attempts(&self, event: &str) -> Vec<(String, String, String, Fate)> {
        let attempts = self.attempts.lock().unwrap();
        let hits = attempts.iter().filter(|(_, _, e, ..)| e == event);
        hits.map(|(id, _, _, published, deadline, fate)| {
            (id.clone(), published.clone(), deadline.clone(), *fate)
        })
        .collect()
    }

    fn markers(&self, provider: &str) -> usize {
        let dir = failure_marker_dir(provider).unwrap();
        std::fs::read_dir(dir).map_or(0, |d| d.count())
    }

    /// Waits up to 5s; the caller asserts the outcome so a mutant fails at its own assertion.
    async fn settled(&self, mut done: impl FnMut(&Self) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(self) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        done(self)
    }

    async fn until(&self, label: &str, done: impl FnMut(&Self) -> bool) {
        assert!(self.settled(done).await, "timed out waiting for {label}");
    }
}

impl Drop for Retry {
    fn drop(&mut self) {
        for (stop, _) in &self.workers {
            stop.store(true, Ordering::Release);
        }
        for (_, worker) in self.workers.drain(..) {
            let _ = worker.join();
        }
        set_test_root(None);
        reset_deferred_adoptions_for_tests();
        reset_state_for_tests();
    }
}

async fn proxy_with_rule(
    listener: tokio::net::TcpListener,
    app: Router,
    seen: Attempts,
    rule: Rule,
) {
    let counts: Mutex<HashMap<String, usize>> = Mutex::default();
    while let Ok((mut socket, _)) = listener.accept().await {
        let encoded = read_async_http_request(&mut socket).await;
        let path = request_path(&encoded);
        let mut parts = path.split('?').next().unwrap_or_default().rsplit('/');
        let event = parts.next().unwrap_or_default().to_owned();
        let provider = parts.next().unwrap_or_default().to_owned();
        let header = |name| request_header(&encoded, name).unwrap_or_default();
        let id = header(RELAY_REQUEST_ID_HEADER);
        let attempt = {
            let mut counts = counts.lock().unwrap();
            let count = counts.entry(id.clone()).or_default();
            *count += 1;
            *count
        };
        let fate = rule(&provider, &event, attempt);
        let (published, deadline) = (
            header(RELAY_PUBLISHED_AT_HEADER),
            header(RELAY_DEADLINE_HEADER),
        );
        seen.lock()
            .unwrap()
            .push((id, provider, event, published, deadline, fate));
        match fate {
            Fate::Drop => drop(socket),
            Fate::Status(code) => {
                let status = axum::http::StatusCode::from_u16(code).unwrap();
                answer_actual_request(&mut socket, status, b"{}").await;
            }
            Fate::Forward => {
                let (status, body) = forward_read_request(&encoded, app.clone()).await;
                answer_actual_request(&mut socket, status, &body).await;
            }
        }
    }
}

/// `forward_actual_request` for a request the proxy already read to choose its fate.
async fn forward_read_request(
    encoded: &[u8],
    app: Router,
) -> (axum::http::StatusCode, axum::body::Bytes) {
    let path = request_path(encoded);
    let body_start = http_body_bounds(encoded).unwrap().0;
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(&path)
        .header("content-type", "application/json");
    for name in [
        RELAY_REQUEST_ID_HEADER,
        RELAY_PUBLISHED_AT_HEADER,
        RELAY_DEADLINE_HEADER,
        crate::services::claude_tui::hook_server::relay_receipts::RELAY_RESPOND_BY_HEADER,
    ] {
        if let Some(value) = request_header(encoded, name) {
            request = request.header(name, value);
        }
    }
    let body = Body::from(encoded[body_start..].to_vec());
    let response = app.oneshot(request.body(body).unwrap()).await.unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_session_start_whose_first_send_fails_is_retried_and_adopted() {
    let _locks = TqLocks::take();
    current_thread().block_on(async {
        let rule: Rule = Arc::new(|_, event, attempt| match (event, attempt) {
            ("SessionStart", 1) => Fate::Drop,
            _ => Fate::Forward,
        });
        let tq = Retry::new(7_540, "n2b-h18", rule).await;
        let (b, c) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        for session in [&b, &c] {
            tq.transcript(session);
            tq.hook("UserPromptSubmit", session, None);
            tq.adopted(session).await;
        }
        tq.hook("SessionStart", &b, Some("resume"));
        let resumed = tq
            .settled(|tq| tq.bound().as_deref() == Some(b.as_str()))
            .await;
        let sends = tq.attempts("SessionStart");
        assert!(
            resumed,
            "[H18:session_start_durable] not adopted after {sends:?}"
        );
        assert_eq!(sends.len(), 2, "[H18:session_start_durable] {sends:?}");
        assert_eq!((sends[0].3, sends[1].3), (Fate::Drop, Fate::Forward));
        assert_eq!(sends[0].0, sends[1].0, "same request id");
        assert_eq!(
            (&sends[0].1, &sends[0].2),
            (&sends[1].1, &sends[1].2),
            "same times"
        );
        let log = binding_events_since(7_540, 0).unwrap();
        let last = log.last().unwrap();
        let source = matches!(&last.new, BindingTarget::Source(s) if s.session_id == b);
        assert!(
            source && last.cause == BindingCause::Resume,
            "[H18:session_start_durable]"
        );
        assert!(
            last.evidence.hook_event.is_some(),
            "[H18:session_start_durable] hooked"
        );
        let line =
            std::fs::read_to_string(tq._log_root.path().join("binding_events").join("7540.log"))
                .unwrap();
        let line: Value = serde_json::from_str(line.lines().last().unwrap()).unwrap();
        let logged = chrono::DateTime::parse_from_rfc3339(line["published_at"].as_str().unwrap());
        let sent = chrono::DateTime::parse_from_rfc3339(&sends[1].1);
        assert_eq!(
            logged.unwrap(),
            sent.unwrap(),
            "[H18:session_start_durable] sidecar"
        );
        assert_eq!(tq.markers("claude"), 0, "[H18:no_marker]");
        assert_eq!(tq.bound(), Some(b), "[H18:binding_b]");
    });
}

#[test]
fn only_a_claude_session_start_retries_a_transport_failure() {
    let _locks = TqLocks::take();
    current_thread().block_on(async {
        let rule: Rule = Arc::new(|_, _, attempt| match attempt {
            1 => Fate::Drop,
            _ => Fate::Forward,
        });
        let mut tq = Retry::new(7_541, "n2b-h22", rule).await;
        let b = uuid::Uuid::new_v4().to_string();
        tq.transcript(&b);
        tq.hook("UserPromptSubmit", &b, None);
        tq.hook("PostToolUse", &b, None);
        let codex = uuid::Uuid::new_v4().to_string();
        tq.worker("codex", &codex);
        let payload = json!({ "session_id": codex, "source": "startup" });
        tq.send("codex", "SessionStart", &codex, payload);
        tq.settled(|tq| tq.markers("claude") + tq.markers("codex") >= 3)
            .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        for event in ["UserPromptSubmit", "PostToolUse", "SessionStart"] {
            assert_eq!(tq.attempts(event).len(), 1, "[H22:scope] {event} sent once");
        }
        assert_eq!(
            (tq.markers("claude"), tq.markers("codex")),
            (2, 1),
            "[H22:scope]"
        );
    });
}

#[test]
fn a_session_start_retries_a_gateway_status_but_not_a_permanent_one() {
    let _locks = TqLocks::take();
    current_thread().block_on(async {
        let rule: Rule = Arc::new(|_, event, attempt| match (event, attempt) {
            ("SessionStart", 1) => Fate::Status(503),
            ("Stop", _) => Fate::Status(500),
            _ => Fate::Forward,
        });
        let tq = Retry::new(7_542, "n2b-retry-status", rule).await;
        let b = uuid::Uuid::new_v4().to_string();
        tq.transcript(&b);
        tq.hook("SessionStart", &b, Some("clear"));
        tq.adopted(&b).await;
        assert_eq!(tq.attempts("SessionStart").len(), 2, "[retry:503]");
        let rule_500 = |error: &str| retries("claude", "SessionStart", error);
        assert!(!rule_500("hook receiver returned HTTP 500"), "[retry:500]");
    });
    for (error, retried) in [
        (
            "post hook event: Connection Failed: Connect error: refused",
            true,
        ),
        (
            "post hook event: Network Error: timed out reading response",
            true,
        ),
        ("hook receiver returned HTTP 502", true),
        ("hook receiver returned HTTP 503", true),
        ("hook receiver returned HTTP 504", true),
        ("hook receiver returned HTTP 425", true),
        ("hook receiver returned HTTP 400", false),
        ("hook receiver returned HTTP 404", false),
        ("hook receiver returned HTTP 409", false),
        ("hook receiver returned HTTP 410", false),
        ("hook receiver returned HTTP 413", false),
        ("hook receiver returned HTTP 500", false),
    ] {
        let got = retries("claude", "SessionStart", error);
        assert_eq!(got, retried, "[retry:classes] {error}");
        let other = retries("claude", "Stop", error);
        assert_eq!(other, error.contains("425"), "[retry:classes] Stop {error}");
    }
}

#[test]
fn an_old_session_start_redelivered_after_another_queue_moved_on_is_refused() {
    let _locks = TqLocks::take();
    let held = Arc::new(AtomicBool::new(true));
    let dropped = Arc::new(AtomicUsize::new(0));
    current_thread().block_on(async {
        let (hold, drops) = (held.clone(), dropped.clone());
        let rule: Rule = Arc::new(move |_, event, _| match event {
            "SessionStart" if hold.load(Ordering::Acquire) => {
                drops.fetch_add(1, Ordering::AcqRel);
                Fate::Drop
            }
            _ => Fate::Forward,
        });
        let mut tq = Retry::new(7_543, "n2b-cross-queue", rule).await;
        let x = uuid::Uuid::new_v4().to_string();
        let (y, other) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        tq.transcript(&x);
        tq.hook("SessionStart", &x, Some("clear"));
        tq.until("first send dropped", |_| {
            dropped.load(Ordering::Acquire) > 0
        })
        .await;
        // Hooks of the same pane under another command session go through another queue.
        register_provider_session("claude", &other, &tq.tmux);
        tq.worker("claude", &other);
        let payload = json!({ "session_id": y, "transcript_path": tq.transcript(&y) });
        tq.send("claude", "UserPromptSubmit", &other, payload);
        tq.adopted(&y).await;
        held.store(false, Ordering::Release);
        let accepted = |tq: &Retry| {
            tq.attempts("SessionStart")
                .iter()
                .any(|a| a.3 == Fate::Forward)
        };
        tq.until("X redelivered", accepted).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(tq.bound(), Some(y), "[cross:kept_y]");
        let log = binding_events_since(7_543, 0).unwrap();
        let refused = matches!(&log.last().unwrap().new,
            BindingTarget::Rejected { payload_session_id, reason, .. }
                if *payload_session_id == x && reason.as_str() == "regression");
        assert!(refused, "[cross:kept_y] {log:#?}");
    });
}

/// Posts `payload` through the router as the relay would, with request id `id` published at `at`.
async fn post(
    state: &HookServerState,
    command: &str,
    payload: &Value,
    id: &str,
    at: chrono::DateTime<Utc>,
) -> u16 {
    let deadline = at + chrono::Duration::hours(1);
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/hooks/claude/SessionStart?session_id={command}"))
        .header("content-type", "application/json")
        .header(RELAY_REQUEST_ID_HEADER, id)
        .header(RELAY_PUBLISHED_AT_HEADER, at.to_rfc3339())
        .header(RELAY_DEADLINE_HEADER, deadline.to_rfc3339())
        .body(Body::from(payload.to_string()))
        .unwrap();
    let app = hook_receiver_router_with_state(state.clone());
    app.oneshot(request).await.unwrap().status().as_u16()
}

/// SessionStart broadcasts under the command session; an alias copy under the payload one is
/// the same delivery.
fn starts(rx: &mut tokio::sync::broadcast::Receiver<HookEvent>, command: &str) -> usize {
    let events = std::iter::from_fn(|| rx.try_recv().ok());
    let starts =
        events.filter(|e| e.kind == HookEventKind::SessionStart && e.session_id == command);
    starts.count()
}

#[test]
fn a_resent_session_start_is_judged_once_while_its_receipt_lasts() {
    let _locks = TqLocks::take();
    current_thread().block_on(async {
        let rule: Rule = Arc::new(|_, _, _| Fate::Forward);
        let tq = Retry::new(7_544, "n2b-receipt", rule).await;
        let (b, c) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        for session in [&b, &c] {
            tq.transcript(session);
            tq.hook("UserPromptSubmit", session, None);
            tq.adopted(session).await;
        }
        let resume = json!({ "session_id": b, "transcript_path": tq.path(&b), "source": "resume" });
        let (id, at) = (uuid::Uuid::new_v4().to_string(), Utc::now());
        let mut rx = tq.state.subscribe();
        // A resend arriving while the first is judged gets 425 and judges nothing.
        let (state, payload, resend) = (tq.state.clone(), resume.clone(), id.clone());
        let inflight = Arc::new(Mutex::new(None));
        let seen = inflight.clone();
        let a = tq.a.clone();
        let command = a.clone();
        crate::services::tui_prompt_dedupe::BEFORE_AUTHORITY.set(Some(Box::new(move || {
            let resent = std::thread::spawn(move || {
                current_thread().block_on(post(&state, &command, &payload, &resend, at))
            });
            *seen.lock().unwrap() = Some(resent.join().unwrap());
        })));
        assert_eq!(post(&tq.state, &a, &resume, &id, at).await, 202);
        let records = binding_events_since(7_544, 0).unwrap().len();
        assert_eq!(tq.bound(), Some(b.clone()), "[receipt:inflight]");
        assert_eq!(starts(&mut rx, &a), 1, "[receipt:inflight] one broadcast");
        let inflight = inflight.lock().unwrap().take();
        assert!(
            matches!(inflight, Some(409 | 425)),
            "[receipt:inflight] {inflight:?}"
        );
        // Answered and lost: the same ledger replays the answer.
        assert_eq!(post(&tq.state, &a, &resume, &id, at).await, 202);
        assert_eq!(
            binding_events_since(7_544, 0).unwrap().len(),
            records,
            "[receipt:replay]"
        );
        assert_eq!(starts(&mut rx, &a), 0, "[receipt:replay] no broadcast");
        // A restart loses the ledger: the binding stays, but the start is broadcast again and
        // clears memento state made since. This is two effects, not one.
        let restarted = HookServerState::new();
        let mut rx = restarted.subscribe();
        let recall = json!({ "tool_name": "mcp__memento__recall",
            "tool_response": {"_meta": {"searchEventId": "5845"}} });
        let app = hook_receiver_router_with_state(restarted.clone());
        let seed = Request::builder()
            .method(Method::POST)
            .uri(format!("/hooks/claude/PostToolUse?session_id={a}"))
            .header("content-type", "application/json")
            .body(Body::from(recall.to_string()))
            .unwrap();
        assert_eq!(app.oneshot(seed).await.unwrap().status().as_u16(), 202);
        assert_eq!(restarted.memento_feedback_pending_for_tests(&a), 1);
        assert_eq!(post(&restarted, &a, &resume, &id, at).await, 202);
        assert_eq!(
            binding_events_since(7_544, 0).unwrap().len(),
            records,
            "[receipt:lost]"
        );
        assert_eq!(tq.bound(), Some(b), "[receipt:lost]");
        assert_eq!(starts(&mut rx, &a), 1, "[receipt:lost] broadcast again");
        assert_eq!(
            restarted.memento_feedback_pending_for_tests(&a),
            0,
            "[receipt:lost]"
        );
    });
}

// A withheld Herdr pane's switch goes out through the real relay queue, is refused by the
// receiver and resent as the same request until the pane is admitted, whatever tmux offers.
#[test]
fn a_withheld_herdr_pane_switch_is_resent_through_the_relay_until_admitted() {
    use crate::config::TestEnvVarGuard as Guard;
    use crate::services::tui_prompt_dedupe::{admit_herdr_execution, withhold_herdr_execution};
    use std::os::unix::fs::PermissionsExt;
    for condition in ["tmux", "exit-127 tmux", "no tmux server"] {
        // `TqLocks::take`'s locks, taken here since this test writes PATH and TMUX under them.
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _state = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _rotations = lock_claude_session_rotations_for_tests();
        let scratch = tempfile::tempdir().unwrap();
        let calls = scratch.path().join("tmux.calls");
        let mut env = Vec::new();
        match condition {
            "exit-127 tmux" => {
                let stub = format!("#!/bin/sh\necho \"$@\" >> {}\nexit 127\n", calls.display());
                let fake = scratch.path().join("tmux");
                std::fs::write(&fake, stub).unwrap();
                std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
                let path = format!(
                    "{}:{}",
                    scratch.path().display(),
                    std::env::var("PATH").unwrap()
                );
                env.push(Guard::set_value_after_shared_test_env_lock(
                    "PATH",
                    path.as_ref(),
                ));
            }
            "no tmux server" => {
                let sockets = scratch.path().join("sockets");
                std::fs::create_dir_all(&sockets).unwrap();
                env.push(Guard::set_path_after_shared_test_env_lock(
                    "TMUX_TMPDIR",
                    &sockets,
                ));
                env.push(Guard::capture_after_shared_test_env_lock("TMUX"));
                unsafe { std::env::remove_var("TMUX") };
            }
            _ => {}
        }
        current_thread().block_on(async {
            let rule: Rule = Arc::new(|_, _, _| Fate::Forward);
            let tq = Retry::new(7_545, "herdr-p7s-relay", rule).await;
            withhold_herdr_execution(&tq.tmux, Some("n1"));
            let b = uuid::Uuid::new_v4().to_string();
            tq.transcript(&b);
            tq.hook("UserPromptSubmit", &b, None);
            let resent = |tq: &Retry| tq.attempts("UserPromptSubmit").len() >= 3;
            tq.until(&format!("{condition}: resends"), resent).await;
            assert_eq!(tq.bound().as_deref(), Some(tq.a.as_str()), "{condition}");

            admit_herdr_execution(&tq.tmux, "n1");
            tq.adopted(&b).await;
            let attempts = tq.attempts("UserPromptSubmit");
            let ids: std::collections::HashSet<_> = attempts.iter().map(|a| &a.0).collect();
            assert_eq!(ids.len(), 1, "{condition}: one request resent unchanged");
        });
        assert!(!calls.exists(), "{condition}: tmux was run");
        drop(env);
    }
}
