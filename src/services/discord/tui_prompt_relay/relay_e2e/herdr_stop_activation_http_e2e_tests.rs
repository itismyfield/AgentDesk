//! A non-forced cancel through the production router on a bound Herdr turn answers "stopping",
//! keeps the row and token until the provider's own abort, and the queued input then starts once.

use std::time::Duration;

use serde_json::{Value, json};

use super::registered_bootstrap_e2e::{FALLBACK, kick_off, recent_message_id, register_channel};
use super::{CHANNEL_ID, RelayE2eHarness, wait_until};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::inflight;
use crate::services::provider::ProviderKind;
use crate::services::provider::cancel_token_claude_interrupt::{
    HERDR_CANCEL_OVERRIDE, herdr_cancel_enabled,
};
use crate::services::session_host::herdr_termination_rig as herdr;

const FIRST: &str = "first input the cancel ends [herdr-http-cancel]";
const SENTINEL: &str = "sentinel queued behind the cancelled turn [herdr-http-cancel]";
const AUTH: &str = "herdr-http-cancel-e2e";
const BUSY: &str = "\u{b7} Actioning\u{2026} (4m 7s \u{b7} esc to interrupt)";

/// Sits at the fixture's socket path: screen reads see a working Claude pane, and every other
/// request passes through to the fixture.
struct BusyPane {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    real: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl BusyPane {
    fn at(path: &std::path::Path) -> Self {
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::atomic::Ordering::SeqCst;
        let real = path.with_extension("real");
        std::fs::rename(path, &real).unwrap();
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (stopped, target) = (stop.clone(), real.clone());
        let thread = std::thread::spawn(move || {
            while !stopped.load(SeqCst) {
                let Ok((mut client, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                };
                let _ = client.set_nonblocking(false);
                let mut request = String::new();
                let _ = std::io::BufReader::new(&client).read_line(&mut request);
                let parsed: Value = serde_json::from_str(&request).unwrap_or(Value::Null);
                let mut reply = String::new();
                if parsed["method"] == "pane.read" {
                    let read = json!({"pane_id": parsed["params"]["pane_id"], "text": BUSY,
                        "source": parsed["params"]["source"], "revision": 7, "truncated": false});
                    let result = json!({"type": "pane_read", "read": read});
                    reply = json!({"id": parsed["id"], "result": result}).to_string() + "\n";
                } else if let Ok(mut server) = UnixStream::connect(&target) {
                    let _ = server.write_all(request.as_bytes());
                    let _ = std::io::BufReader::new(&server).read_line(&mut reply);
                }
                let _ = client.write_all(reply.as_bytes());
                let _ = client.shutdown(std::net::Shutdown::Write);
            }
        });
        let (thread, path) = (Some(thread), path.to_path_buf());
        Self {
            stop,
            thread,
            real,
            path,
        }
    }
}

impl Drop for BusyPane {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::rename(&self.real, &self.path);
    }
}

#[test]
fn an_http_cancel_settles_through_the_bridge_and_starts_the_queued_input_once_pg() {
    let path = concat!(
        module_path!(),
        "::an_http_cancel_settles_through_the_bridge_and_starts_the_queued_input_once_pg"
    );
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    // Test builds read and write the binding log only under a root set on the running thread.
    let events = tempfile::tempdir().unwrap();
    let (registry, root) = (rig.thread_installer(), events.path().to_path_buf());
    let install = move || {
        registry();
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(Some(&root));
    };
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(scenario(rig, events.path()));
}

/// The canonical Source the execution's SessionStart logged when it was launched.
fn log_session_start(events: &std::path::Path, tmux: &str, session: &str, path: &std::path::Path) {
    use crate::services::tui_prompt_dedupe::binding_events as log;
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).unwrap();
    let event = log::BindingEvent {
        seq: 1,
        channel_id: CHANNEL_ID,
        provider: ProviderKind::Claude.as_str().into(),
        tmux_session: tmux.into(),
        execution_nonce: Some(herdr::EXECUTION.into()),
        old: None,
        new: log::BindingTarget::Source(log::SourceId {
            session_id: session.into(),
            path: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
        }),
        cause: log::BindingCause::Startup,
        parent_hint: None,
        evidence: log::BindingEvidence {
            hook_event: Some("SessionStart".into()),
            received_at: chrono::Utc::now(),
        },
        committed_at: chrono::Utc::now(),
    };
    let dir = events.join(log::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let line = serde_json::to_string(&event).unwrap() + "\n";
    std::fs::write(dir.join(format!("{CHANNEL_ID}.log")), line).unwrap();
}

/// The production router's non-forced cancel of the channel, served by the harness runtime.
async fn http_cancel(harness: &RelayE2eHarness) -> (axum::http::StatusCode, Value) {
    let registry = std::sync::Arc::new(crate::services::discord::health::HealthRegistry::new());
    let provider = ProviderKind::Claude.as_str().into();
    registry.register(provider, harness.shared.clone()).await;
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some(AUTH.into());
    let pool = harness.shared.pg_pool.clone();
    let app = crate::server::routes::home_stop_test_app(config, pool, Some(registry), "e2e");
    let bearer = format!("Bearer {AUTH}");
    let uri = format!("/turns/{CHANNEL_ID}/cancel");
    let headers = [("authorization", bearer.as_str())];
    let post = crate::server::routes::home_stop_test_post;
    post(&app, &uri, &headers, r#"{"force":false}"#).await
}

async fn scenario(rig: herdr::P11TerminationRig, events: &std::path::Path) {
    use crate::services::claude_tui::hook_server::observation_ingress::tests::{claude, uuid};
    let mut database = None;
    let harness = RelayE2eHarness::start_unbound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let database = database.expect("an isolated PostgreSQL database");
    let pool = harness.shared.pg_pool.clone().expect("PG-backed runtime");
    let (shared, channel) = (harness.shared.clone(), harness.channel_id);
    register_channel(&harness);
    harness.answer_placeholders_immediately();
    let fake_tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux;
    let _tmux = fake_tmux(harness.root.path());
    let o_store = crate::services::herdr_launch::o_store_for_test;
    let (store, era) = o_store(harness.root.path(), &[CHANNEL_ID]);
    let mut seeded = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();
    let provider = ProviderKind::Claude;
    let tmux = provider.build_tmux_session_name(FALLBACK);
    let build = crate::services::discord::adk_session::build_namespaced_session_key;
    let session_key = build(&shared.token_hash, &provider, &tmux);
    let bound = herdr::HostedState::Bound;
    let _switches = rig
        .bind(&pool, &shared.token_hash, &session_key, bound)
        .await;
    crate::services::claude::herdr_turn::release_hold(herdr::EXECUTION);
    // Only the live config enables the Escape; no test override decides it.
    let mut config = (*crate::config_live_reload::current().unwrap()).clone();
    config.runtime.herdr_cancel_enabled = Some(true);
    crate::config_live_reload::install(config);
    assert_eq!(HERDR_CANCEL_OVERRIDE.with(std::cell::Cell::get), None);
    assert!(herdr_cancel_enabled(), "the configured switch alone");

    let session = uuid();
    let transcript = harness.root.path().join(format!("{session}.jsonl"));
    let append = |lines: &[Value]| {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&transcript)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    };
    let stamped = |mut line: Value| {
        line["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
        line["sessionId"] = json!(session);
        line
    };
    let done = json!({"type": "system", "subtype": "turn_duration"});
    append(&[
        stamped(json!({"type": "assistant", "message": {"role": "assistant",
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "earlier"}]}})),
        stamped(done.clone()),
    ]);
    let register = crate::services::tui_prompt_dedupe::register_tmux_runtime_binding;
    register(&tmux, claude(&transcript, &session));
    log_session_start(events, &tmux, &session, &transcript);

    let first = recent_message_id();
    harness.seed_channel_history(&[(first, FIRST, false)]);
    harness.run_catch_up().await;
    let sends = || -> Vec<(String, Value)> {
        let all = rig.writes_with_params();
        all.into_iter()
            .filter(|(method, _)| method.starts_with("pane.send"))
            .collect()
    };
    let prompts_of = |text: &str| {
        let pasted = |(method, params): &(String, Value)| {
            method == "pane.send_text"
                && params["text"]
                    .as_str()
                    .is_some_and(|sent| sent.contains(text))
        };
        sends().iter().filter(|send| pasted(send)).count()
    };
    let escapes = || {
        let escape = |(method, params): &(String, Value)| {
            method == "pane.send_keys" && params["keys"] == json!(["esc"])
        };
        sends().iter().filter(|send| escape(send)).count()
    };
    assert!(kick_off(&harness).await, "the first input starts");
    let prompted = wait_until(Duration::from_secs(60), || {
        let done = prompts_of(FIRST) == 1;
        Box::pin(async move { done })
    })
    .await;
    assert!(prompted, "no Herdr prompt: {:?}", rig.writes());

    // The sentinel arrives while the first turn runs and waits in the queue.
    harness
        .deliver_user_message(first + 1, SENTINEL)
        .await
        .unwrap();
    let queued: Vec<_> = harness
        .mailbox()
        .await
        .intervention_queue
        .iter()
        .map(|i| i.text.clone())
        .collect();
    assert!(
        queued.iter().any(|text| text.contains(SENTINEL)),
        "{queued:?}"
    );
    // The provider took the input and is generating.
    append(&[
        stamped(json!({"type": "user", "message": {"role": "user", "content": FIRST}})),
        stamped(json!({"type": "assistant", "message": {"role": "assistant", "content": []}})),
    ]);
    let live = harness.mailbox().await.cancel_token.expect("the live turn");

    // The turn start's status marker goes through the internal API this harness does not serve,
    // and the rig's row names a placeholder root where a launch records the turn owner's own.
    let root = live
        .herdr_interrupt_state()
        .unwrap()
        .owner
        .runtime_root
        .clone();
    let active = sqlx::query(
        "UPDATE sessions SET status = 'turn_active', hosted_execution = jsonb_set(jsonb_set(hosted_execution, '{owner,runtime_root}', to_jsonb($2::text)), '{source_ref,runtime_root}', to_jsonb($2::text)) WHERE session_key = $1",
    );
    let active = active.bind(&session_key).bind(&root);
    active.execute(&pool).await.unwrap();
    let socket = rig
        .record(&shared.token_hash, &session_key)
        .location
        .unwrap();
    let busy = BusyPane::at(std::path::Path::new(&socket.socket_addr));
    // The router answers the holder's stop: a stop on the turn, never its end.
    let (status, answer) = http_cancel(&harness).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{answer}");
    let claims = json!({
        "outcome": answer["outcome"], "intent": answer["intent"], "delivery": answer["delivery"],
        "turn_status": answer["turn_status"], "turn_completed_at": answer.get("turn_completed_at"),
        "inflight_cleared": answer["inflight_cleared"],
        "dispatch_cancelled": answer.get("dispatch_cancelled"),
    });
    let stopping = json!({
        "outcome": "herdr", "intent": "recorded", "delivery": "sent", "turn_status": "stopping",
        "turn_completed_at": null, "inflight_cleared": false, "dispatch_cancelled": null,
    });
    assert_eq!(claims, stopping);

    // Until its own terminal the turn keeps its row and token, and nothing else is prompted.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let row = inflight::load_inflight_state(&provider, channel.get()).expect("the turn's row");
    assert_eq!(row.turn_nonce.as_deref(), live.turn_nonce());
    let current = harness.mailbox().await.cancel_token;
    assert!(current.is_some_and(|current| std::sync::Arc::ptr_eq(&current, &live)));
    assert_eq!(prompts_of(SENTINEL), 0, "{:?}", sends());
    assert_eq!(escapes(), 1, "{:?}", sends());
    drop(busy);

    // The provider's own abort ends it; the bridge's finalizer frees the channel and the queue
    // prompts the sentinel once, well before the queue's 60s backstop would.
    let interrupt = json!({"type": "user", "message": {"role": "user",
        "content": [{"type": "text", "text": "[Request interrupted by user]"}]}});
    append(&[stamped(interrupt), stamped(done)]);
    let next = wait_until(Duration::from_secs(30), || {
        let done = prompts_of(SENTINEL) >= 1;
        Box::pin(async move { done })
    })
    .await;
    let after = inflight::load_inflight_state(&provider, channel.get());
    assert!(
        after
            .as_ref()
            .is_none_or(|row| row.turn_nonce.as_deref() != live.turn_nonce()),
        "the cancelled turn's row is cleared: {after:?}"
    );
    assert!(next, "the sentinel was not prompted: {:?}", sends());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(prompts_of(SENTINEL), 1, "{:?}", sends());
    assert_eq!(prompts_of(FIRST), 1, "{:?}", sends());
    assert_eq!(escapes(), 1, "{:?}", sends());
    assert!(
        harness.durable_queue().is_empty(),
        "consumed from the queue"
    );
    drop(harness);
    pool.close().await;
    database.drop().await;
}
