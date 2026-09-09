//! #5191: a terminal-committed row rejects late streaming republication from its
//! own pinned owner, and the tick then stops before any trailing preview write
//! while every pre-existing cleanup path keeps its effects.

use super::*;
use crate::services::discord::inflight::{
    InflightTurnIdentity, WatcherProgressOutcome, inflight_runtime_root, load_inflight_state,
    save_inflight_state,
};
use std::sync::Mutex;
use std::time::Duration;
use tracing_subscriber::fmt::MakeWriter;

const WARN_EVENT: &str = "watcher_stream_progress_terminal_rejected";
const PANEL_CHILD_ENV: &str = "AGENTDESK_5191_PANEL_FIXTURE_CHILD";
const COMMITTED_BODY: &str = "완료된 응답 A";
const TRAILING_BODY: &str = "후속 응답 B를 표시합니다.";
const PANEL_MSG: u64 = 5_191_700;
const PLACEHOLDER_MSG: u64 = 5_191_002;
const SERVER_MSG: u64 = 5_191_999;
const CONTINUE: StreamingStatusTickOutcome = StreamingStatusTickOutcome::ContinueStreamingLoop;
const FALLTHROUGH: StreamingStatusTickOutcome = StreamingStatusTickOutcome::Fallthrough;

#[rustfmt::skip]
fn msg(id: u64) -> Option<serenity::MessageId> { Some(serenity::MessageId::new(id)) }

#[rustfmt::skip]
fn expect(fx: &Fixture, identity: Option<&InflightTurnIdentity>, want: WatcherProgressOutcome) {
    assert_eq!(wrapper_patch(fx, identity, None, TRAILING_BODY), want);
}

#[derive(Clone)]
struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    #[rustfmt::skip]
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[rustfmt::skip]
impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;
    fn make_writer(&'a self) -> Self::Writer { self.clone() }
}

struct RootGuard {
    previous: Option<std::ffi::OsString>,
    root: tempfile::TempDir,
}

#[rustfmt::skip]
impl Drop for RootGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(v) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", v) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn isolate_root() -> (std::sync::MutexGuard<'static, ()>, RootGuard) {
    let env_lock = crate::config::shared_test_env_lock();
    let lock = env_lock.lock().unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
    (lock, RootGuard { previous, root })
}

/// Install a WARN capturing subscriber around the ENTIRE current-thread
/// `block_on` — not merely around future construction — and return the `#5191`
/// rejection lines it observed.
#[rustfmt::skip]
fn capture_warns(body: impl std::future::Future<Output = ()>) -> Vec<String> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::WARN)
        .with_ansi(false).without_time()
        .with_writer(CapturingWriter(buffer.clone())).finish();
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread().enable_all().build()
            .expect("current-thread runtime").block_on(body);
    });
    String::from_utf8(buffer.lock().unwrap().clone()).expect("utf8 log")
        .lines().filter(|line| line.contains(WARN_EVENT)).map(str::to_owned).collect()
}

struct Fixture {
    provider: ProviderKind,
    channel: ChannelId,
    tmux: String,
    output_path: String,
    identity: InflightTurnIdentity,
}

impl Fixture {
    fn path(&self) -> std::path::PathBuf {
        inflight_runtime_root()
            .expect("inflight root")
            .join(self.provider.as_str())
            .join(format!("{}.json", self.channel.get()))
    }
    #[rustfmt::skip]
    fn row_bytes(&self) -> Vec<u8> { std::fs::read(self.path()).expect("row on disk") }
    fn uri(&self, msg: u64) -> String {
        format!("/api/v10/channels/{}/messages/{msg}", self.channel.get())
    }
}

/// A real on-disk row. `terminal` seeds the committed final body X; an active
/// row stays empty so a later frame never moves `response_sent_offset` backward.
#[rustfmt::skip]
fn seed_row(root: &std::path::Path, case: u64, terminal: bool, silent: bool) -> Fixture {
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(5_191_000_000 + case);
    let tmux = format!("agentdesk-5191-fixture-{case}");
    let output_path = root.join(format!("fixture-{case}.jsonl")).to_string_lossy().into_owned();
    std::fs::write(&output_path, b"").expect("fixture transcript");
    let mut row = InflightTurnState::new(
        provider.clone(), channel.get(), None, 1, 5_191_001, PLACEHOLDER_MSG,
        "시험 입력".to_string(), None, Some(tmux.clone()), Some(output_path.clone()), None, 0,
    );
    if terminal {
        row.full_response = COMMITTED_BODY.to_string();
        row.response_sent_offset = COMMITTED_BODY.len();
        row.streaming_rollover_frozen_msg_ids = vec![5_191_003];
        row.terminal_delivery_committed = true;
    }
    row.silent_turn = silent;
    save_inflight_state(&row).expect("seed row");
    let identity = InflightTurnIdentity::from_state(&row);
    Fixture { provider, channel, tmux, output_path, identity }
}

/// The real `tmux.rs` wrapper with a post-send-shaped patch: anchor id, nonzero
/// sent offset and frozen rollover ids.
#[rustfmt::skip]
fn wrapper_patch(fx: &Fixture, identity: Option<&InflightTurnIdentity>,
    anchor: Option<serenity::MessageId>, body: &str) -> WatcherProgressOutcome {
    persist_watcher_stream_progress(
        &fx.provider, fx.channel, &fx.tmux, identity, anchor, body, body.len(),
        None, None, None, false, false, &[serenity::MessageId::new(5_191_004)],
    )
}

struct Recorder {
    calls: Arc<Mutex<Vec<(String, String)>>>,
    http: Arc<serenity::Http>,
    server: tokio::task::AbortHandle,
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Recorder {
    #[rustfmt::skip]
    fn seen(&self, method: &str) -> Vec<String> {
        let calls = self.calls.lock().unwrap();
        calls.iter().filter(|(m, _)| m == method).map(|(_, uri)| uri.clone()).collect()
    }
    #[rustfmt::skip]
    fn total(&self) -> usize { self.calls.lock().unwrap().len() }
}

/// Local axum recorder: every Discord method/URI is captured, and no external
/// network, tmux or database call happens.
#[rustfmt::skip]
async fn recorder(channel: ChannelId, delete_ok: bool) -> Recorder {
    use axum::body::Bytes;
    use axum::http::{Method, StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::{Json, Router, routing::any};
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let channel_text = channel.get().to_string();
    let app = Router::new().fallback(any(move |method: Method, uri: Uri, body: Bytes| {
        let (recorded, channel_text) = (recorded.clone(), channel_text.clone());
        async move {
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            recorded.lock().unwrap().push((method.to_string(), uri.to_string()));
            if method == Method::DELETE {
                let status = if delete_ok { StatusCode::NO_CONTENT }
                    else { StatusCode::INTERNAL_SERVER_ERROR };
                return (status, String::new()).into_response();
            }
            Json(serde_json::json!({
                "id": SERVER_MSG.to_string(), "channel_id": channel_text,
                "content": payload["content"],
                "author": {"id":"1","username":"t","discriminator":"0001","avatar":null},
                "timestamp":"2026-09-09T00:00:00+00:00", "edited_timestamp":null,
                "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                "attachments":[], "embeds":[], "pinned":false, "type":0
            })).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Recorder { calls, http, server: server.abort_handle() }
}

#[rustfmt::skip]
struct TickLocals {
    last: tokio::time::Instant, spin: usize,
    placeholder: Option<serenity::MessageId>, restored: bool,
    edit: String, sent: usize, frozen: Vec<serenity::MessageId>,
    panel_id: Option<serenity::MessageId>, panel_text: String,
    external: bool, pin: Option<InflightTurnIdentity>,
    recent_stop: bool, missing: bool, reacquire: bool, generation: u64,
}

#[rustfmt::skip]
fn tick_locals(fx: &Fixture, placeholder: Option<u64>) -> TickLocals {
    TickLocals {
        last: tokio::time::Instant::now(), spin: 0,
        placeholder: placeholder.map(serenity::MessageId::new), restored: false,
        edit: String::new(), sent: 0, frozen: Vec::new(),
        panel_id: None, panel_text: String::new(),
        external: false, pin: Some(fx.identity.clone()),
        recent_stop: false, missing: false, reacquire: false, generation: 0,
    }
}

/// One EXPIRED throttle tick through the real `update_streaming_status_tick`.
#[rustfmt::skip]
async fn run_tick(
    locals: &mut TickLocals,
    rec: &Recorder,
    shared: &Arc<SharedData>,
    fx: &Fixture,
    delivered: bool,
) -> StreamingStatusTickOutcome {
    locals.last = tokio::time::Instant::now()
        - crate::services::discord::status_update_interval()
        - Duration::from_millis(1);
    let tools = WatcherToolState::new();
    let delivered_flag = Arc::new(AtomicBool::new(delivered));
    let full = TRAILING_BODY.to_string();
    let ctx = StreamingStatusTickContext {
        http: &rec.http, shared, channel_id: fx.channel, watcher_provider: &fx.provider,
        tmux_session_name: &fx.tmux, output_path: &fx.output_path,
        turn_delivered: &delivered_flag,
    };
    let turn = StreamingStatusTickTurn {
        data_start_offset: 0, current_offset: full.len() as u64, full_response: &full,
        tool_state: &tools, task_notification_kind: None,
        status_panel_started_at: 1_700_000_000, single_message_panel_footer_mode: false,
        restored_injected_prompt_message_id: None,
    };
    let mut render = StreamingRenderState {
        last_status_update: &mut locals.last, spin_idx: &mut locals.spin,
        placeholder_msg_id: &mut locals.placeholder,
        placeholder_from_restored_inflight: &mut locals.restored,
        last_edit_text: &mut locals.edit, response_sent_offset: &mut locals.sent,
        watcher_streaming_rollover_frozen_msg_ids: &mut locals.frozen,
    };
    let mut panel = StatusPanelState {
        status_panel_msg_id: &mut locals.panel_id,
        last_status_panel_text: &mut locals.panel_text,
    };
    let mut suppress = StreamingSuppressState {
        turn_is_external_input_for_session: &mut locals.external,
        turn_identity_for_panel: &mut locals.pin,
        streaming_suppressed_by_recent_stop: &mut locals.recent_stop,
        streaming_suppressed_by_missing_inflight: &mut locals.missing,
        active_stream_inflight_reacquire_logged: &mut locals.reacquire,
    };
    let mut generation = PanelGenerationState {
        this_turn_status_panel_generation: &mut locals.generation,
    };
    update_streaming_status_tick(&ctx, turn, &mut render, &mut panel, &mut suppress,
        &mut generation).await
}

/// Seed a REAL current-generation durable frontier enclosing the observed range
/// so the pre-existing bridge-delivered cleanup is reachable.
#[rustfmt::skip]
fn seed_bridge_frontier(shared: &Arc<SharedData>, fx: &Fixture, end: u64) {
    use crate::services::discord::outbound::{delivery_frontier_probe, delivery_record};
    let marker = crate::services::tmux_common::session_temp_path(&fx.tmux, "generation");
    std::fs::write(marker, b"1").expect("wrapper generation marker");
    let generation = delivery_record::current_generation_mtime_ns(&fx.tmux);
    assert_ne!(generation, 0, "fixture needs a readable generation");
    shared.tmux_relay_coord(fx.channel)
        .confirmed_end_generation_mtime_ns
        .store(generation, Ordering::Release);
    delivery_record::record_delivered_frontier_with_body(
        shared, &fx.provider, fx.channel, Some(&fx.tmux), (0, end),
        PANEL_MSG, fx.channel.get(), COMMITTED_BODY, Some(fx.identity.user_msg_id),
    );
    assert_eq!(
        delivery_frontier_probe::delivered_frontier_current_generation(
            &fx.provider, fx.channel, &fx.tmux, Some(end),
        ).map(|commit| commit.range),
        Some((0, end)),
        "cleanup fixture must see its durable frontier before the tick"
    );
}

#[test]
fn committed_progress_preserves_exact_row() {
    let (_lock, guard) = isolate_root();
    let fx = seed_row(guard.root.path(), 1, true, false);
    let before = fx.row_bytes();
    for anchor in [None, Some(serenity::MessageId::new(5_191_555))] {
        assert_eq!(
            wrapper_patch(&fx, Some(&fx.identity), anchor, "옛 본문"),
            WatcherProgressOutcome::TerminalAlreadyCommitted
        );
        assert_eq!(fx.row_bytes(), before, "committed row stays byte-identical");
    }
}

#[test]
fn terminal_progress_no_false_authority() {
    let (_lock, guard) = isolate_root();
    let fx = seed_row(guard.root.path(), 2, true, false);
    let mut other = fx.identity.clone();
    other.user_msg_id += 1;
    expect(&fx, Some(&other), WatcherProgressOutcome::Skipped);
    // `None` identity keeps its historical behaviour: no late-birth denial.
    expect(&fx, None, WatcherProgressOutcome::Saved);
    let active = seed_row(guard.root.path(), 3, false, false);
    expect(
        &active,
        Some(&active.identity),
        WatcherProgressOutcome::Saved,
    );
    let mut row = load_inflight_state(&fx.provider, fx.channel.get()).expect("terminal row");
    row.rebind_origin = true;
    save_inflight_state(&row).expect("rebound row");
    expect(&fx, Some(&fx.identity), WatcherProgressOutcome::Skipped);
    row.rebind_origin = false;
    row.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
    save_inflight_state(&row).expect("restart row");
    expect(&fx, Some(&fx.identity), WatcherProgressOutcome::Skipped);
    std::fs::remove_file(fx.path()).expect("drop row");
    expect(&fx, Some(&fx.identity), WatcherProgressOutcome::Skipped);
}

#[test]
#[rustfmt::skip]
fn progress_short_body_and_ioerror_stay_nonterminal() {
    let (_lock, guard) = isolate_root();
    let fx = seed_row(guard.root.path(), 5, true, false);
    assert_eq!(
        persist_watcher_stream_progress(
            &fx.provider, fx.channel, &fx.tmux, Some(&fx.identity), None, "짧", 9_999,
            None, None, None, false, false, &[],
        ),
        WatcherProgressOutcome::Skipped
    );
    let blocked = guard.root.path().join("blocked-root");
    std::fs::write(&blocked, b"not-a-dir").expect("blocking file");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &blocked) };
    let outcome = wrapper_patch(&fx, Some(&fx.identity), None, TRAILING_BODY);
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", guard.root.path()) };
    assert_eq!(outcome, WatcherProgressOutcome::IoError);
}

#[test]
fn terminal_outcome_logged_at_wrapper() {
    let (_lock, guard) = isolate_root();
    let fx = seed_row(guard.root.path(), 6, true, false);
    let before = fx.row_bytes();
    let anchor = Some(serenity::MessageId::new(PLACEHOLDER_MSG));
    let warns = capture_warns(async {
        wrapper_patch(&fx, Some(&fx.identity), anchor, TRAILING_BODY);
        wrapper_patch(&fx, Some(&fx.identity), None, TRAILING_BODY);
    });
    assert_eq!(
        warns.len(),
        2,
        "one WARN per rejected wrapper call: {warns:?}"
    );
    assert!(warns[0].contains(&format!("channel_id={}", fx.channel.get())));
    assert!(warns[0].contains(&format!("tmux_session={}", fx.tmux)));
    assert!(warns[0].contains(&format!("user_msg_id=Some({})", fx.identity.user_msg_id)));
    assert!(warns[0].contains(&format!("discarded_current_msg_id=Some({PLACEHOLDER_MSG})")));
    assert!(warns[1].contains("discarded_current_msg_id=None"));
    assert_eq!(
        fx.row_bytes(),
        before,
        "post-send-shaped patch never writes"
    );
    let active = seed_row(guard.root.path(), 7, false, false);
    let saved = capture_warns(async {
        assert_eq!(
            wrapper_patch(&active, Some(&active.identity), anchor, TRAILING_BODY),
            WatcherProgressOutcome::Saved
        );
    });
    assert!(saved.is_empty(), "Saved emits no rejection WARN");
}

#[test]
fn progress_caller_uses_actual_terminal_outcome() {
    // The runtime tick tests are authoritative; this only pins the wiring.
    assert!(include_str!("../turn_stream_collector.rs").contains("update_streaming_status_tick("));
    let tick = include_str!("../streaming_status_tick.rs");
    let at = |needle: &str| {
        tick.find(needle)
            .unwrap_or_else(|| panic!("missing {needle}"))
    };
    let persist = at("let progress_outcome = persist_watcher_stream_progress(");
    assert!(
        persist < at("if !terminal_progress_rejected {"),
        "gate the panel helper"
    );
    let cleanup = at("watcher_streaming_recent_stop_cleanup");
    assert!(
        cleanup < at("if terminal_progress_rejected {"),
        "cleanup precedes the stop"
    );
}

/// Two expired ticks on a terminal-committed row: no republication at all.
#[rustfmt::skip]
async fn terminal_tick_is_inert(guard: &RootGuard, case: u64, placeholder: Option<u64>)
    -> (Vec<u8>, TickLocals) {
    let fx = seed_row(guard.root.path(), case, true, false);
    let before = fx.row_bytes();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let rec = recorder(fx.channel, true).await;
    let mut locals = tick_locals(&fx, placeholder);
    for _ in 0..2 {
        let spin = locals.spin;
        assert_eq!(
            run_tick(&mut locals, &rec, &shared, &fx, false).await,
            CONTINUE
        );
        assert_eq!(locals.spin, spin + 1, "the real throttle path ran");
    }
    assert_eq!(fx.row_bytes(), before, "terminal row stays byte-identical");
    assert_eq!(locals.pin.as_ref(), Some(&fx.identity), "pin stays A");
    assert_eq!(rec.total(), 0, "zero POST/PATCH/DELETE");
    (before, locals)
}

#[test]
fn committed_progress_tick_emits_no_republication() {
    let (_lock, guard) = isolate_root();
    for (case, placeholder) in [(8_u64, None), (9, Some(PLACEHOLDER_MSG))] {
        let warns = capture_warns(async {
            let (_before, locals) = terminal_tick_is_inert(&guard, case, placeholder).await;
            assert_eq!(locals.sent, 0);
            assert_eq!(locals.placeholder, placeholder.and_then(|id| msg(id)));
        });
        assert_eq!(warns.len(), 2, "one central WARN per suppressed tick");
    }
}

#[test]
fn committed_progress_pinned_identity_suppresses_trailing_body() {
    let (_lock, guard) = isolate_root();
    for (case, placeholder) in [(10_u64, None), (11, Some(PLACEHOLDER_MSG))] {
        let warns = capture_warns(async {
            let (before, locals) = terminal_tick_is_inert(&guard, case, placeholder).await;
            let disk = String::from_utf8_lossy(&before).into_owned();
            assert!(disk.contains(COMMITTED_BODY), "disk keeps committed X");
            assert!(!disk.contains(TRAILING_BODY), "trailing Y never lands");
            assert!(locals.frozen.is_empty() && locals.edit.is_empty());
        });
        assert_eq!(warns.len(), 2);
        for warn in &warns {
            assert!(warn.contains(&format!("discarded_current_msg_id={placeholder:?}")));
        }
    }
}

#[test]
fn active_progress_tick_emits_once() {
    let (_lock, guard) = isolate_root();
    capture_warns(async {
        let fx = seed_row(guard.root.path(), 12, false, false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let rec = recorder(fx.channel, true).await;
        let mut locals = tick_locals(&fx, None);
        assert_eq!(
            run_tick(&mut locals, &rec, &shared, &fx, false).await,
            FALLTHROUGH
        );
        assert_eq!(rec.seen("POST").len(), 1, "active turn publishes once");
        assert_eq!(locals.placeholder, msg(SERVER_MSG), "local anchor adopted");
        let disk = load_inflight_state(&fx.provider, fx.channel.get()).expect("row");
        assert_eq!(disk.current_msg_id, SERVER_MSG, "durable anchor agrees");
        assert!(
            disk.full_response.contains(TRAILING_BODY),
            "body is durable"
        );
        assert!(!locals.edit.is_empty(), "the rendered frame is retained");
        // A spinner-only PATCH is allowed here; a second POST would be a duplicate.
        run_tick(&mut locals, &rec, &shared, &fx, false).await;
        assert_eq!(rec.seen("POST").len(), 1, "no duplicate on the next tick");
        assert_eq!(locals.placeholder, msg(SERVER_MSG), "anchor retained");
    });
}

#[test]
fn active_silent_progress_is_durable() {
    let (_lock, guard) = isolate_root();
    capture_warns(async {
        let fx = seed_row(guard.root.path(), 13, false, true);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let rec = recorder(fx.channel, true).await;
        let mut locals = tick_locals(&fx, None);
        assert_eq!(
            run_tick(&mut locals, &rec, &shared, &fx, false).await,
            CONTINUE
        );
        assert_eq!(rec.total(), 0, "silent turns render nothing");
        assert!(
            String::from_utf8_lossy(&fx.row_bytes()).contains(TRAILING_BODY),
            "silent turns still persist parsed progress"
        );
    });
}

async fn bridge_cleanup_variant(guard: &RootGuard, case: u64, delete_ok: bool, restored: bool) {
    let fx = seed_row(guard.root.path(), case, true, false);
    let shared = crate::services::discord::make_shared_data_for_tests();
    seed_bridge_frontier(&shared, &fx, TRAILING_BODY.len() as u64);
    let before = fx.row_bytes();
    let rec = recorder(fx.channel, delete_ok).await;
    let mut locals = tick_locals(&fx, Some(PLACEHOLDER_MSG));
    locals.restored = restored;
    locals.edit = "이전 편집".to_string();
    assert_eq!(
        run_tick(&mut locals, &rec, &shared, &fx, true).await,
        CONTINUE
    );
    assert!(rec.seen("POST").is_empty() && rec.seen("PATCH").is_empty());
    assert_eq!(fx.row_bytes(), before, "terminal row bytes unchanged");
    assert!(locals.recent_stop, "bridge suppression flag committed");
    assert!(!locals.restored);
    if restored {
        assert!(
            rec.seen("DELETE").is_empty(),
            "restored anchor is never deleted"
        );
        assert_eq!(locals.placeholder, None, "local ownership still dropped");
        return;
    }
    assert_eq!(rec.seen("DELETE"), vec![fx.uri(PLACEHOLDER_MSG)]);
    if delete_ok {
        assert_eq!(locals.placeholder, None);
        assert!(locals.edit.is_empty());
        run_tick(&mut locals, &rec, &shared, &fx, true).await;
        assert_eq!(
            rec.seen("DELETE").len(),
            1,
            "committed cleanup never repeats"
        );
    } else {
        assert_eq!(
            locals.placeholder,
            msg(PLACEHOLDER_MSG),
            "DELETE failure keeps it"
        );
        assert_eq!(locals.edit, "이전 편집");
        run_tick(&mut locals, &rec, &shared, &fx, true).await;
        assert_eq!(rec.seen("DELETE").len(), 2, "failed cleanup retries");
    }
}

async fn panel_parity_variant(guard: &RootGuard, case: u64, terminal: bool) {
    let footer_off = !crate::services::discord::single_message_panel_enabled();
    assert!(
        footer_off,
        "separate-panel mode must be pinned by the parent"
    );
    let fx = seed_row(guard.root.path(), case, terminal, false);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    let ui = &mut Arc::get_mut(&mut shared).unwrap().ui;
    ui.status_panel_v2_enabled = true;
    let before = fx.row_bytes();
    let rec = recorder(fx.channel, true).await;
    let mut locals = tick_locals(&fx, None);
    locals.panel_id = msg(PANEL_MSG);
    locals.panel_text = "낡은 패널".to_string();
    let outcome = run_tick(&mut locals, &rec, &shared, &fx, false).await;
    if terminal {
        assert_eq!(outcome, CONTINUE);
        assert_eq!(rec.total(), 0, "terminal tick reaches the final gate");
        assert_eq!(locals.panel_text, "낡은 패널");
        assert_eq!(fx.row_bytes(), before);
    } else {
        assert_eq!(outcome, FALLTHROUGH);
        assert!(
            rec.seen("PATCH").contains(&fx.uri(PANEL_MSG)),
            "active ticks still run the extracted existing-panel refresh: {:?}",
            rec.seen("PATCH")
        );
        assert_ne!(locals.panel_text, "낡은 패널", "panel cache text updated");
    }
}

#[test]
fn committed_progress_tick_preserves_cleanup_and_panel_parity() {
    if std::env::var_os(PANEL_CHILD_ENV).is_some() {
        let (_lock, guard) = isolate_root();
        capture_warns(async {
            panel_parity_variant(&guard, 20, true).await;
            panel_parity_variant(&guard, 21, false).await;
        });
        return;
    }
    let name = {
        let (_lock, guard) = isolate_root();
        capture_warns(async {
            bridge_cleanup_variant(&guard, 14, true, false).await;
            bridge_cleanup_variant(&guard, 15, false, false).await;
            bridge_cleanup_variant(&guard, 16, true, true).await;
        });
        format!(
            "{}::committed_progress_tick_preserves_cleanup_and_panel_parity",
            module_path!().split_once("::").unwrap().1
        )
    };
    // The separate-status-panel branch sits behind a process-cached, default-ON
    // footer flag, so panel parity runs in a child that pins it OFF.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(PANEL_CHILD_ENV, "1")
        .env("AGENTDESK_SINGLE_MESSAGE_PANEL", "0")
        .output()
        .expect("run panel-parity child");
    assert!(
        output.status.success(),
        "panel parity child failed: {output:?}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
}
