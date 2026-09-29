use super::*;
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::claude_tui::hook_server::adoption_retry::reset_deferred_adoptions_for_tests;
use crate::services::tui_prompt_dedupe::{
    TEST_LOCK, adopt_claude_continuation_session, lock_claude_session_rotations_for_tests,
    register_launched_tmux_runtime_binding, register_provider_session,
    register_rehydrated_tmux_runtime_binding, register_tmux_channel, register_tmux_runtime_binding,
    reset_state_for_tests, runtime_binding_for_tmux_session,
};

/// Serialises the dedupe state and points the log at a scratch root for one test.
struct Lane {
    root: tempfile::TempDir,
    dir: tempfile::TempDir,
    _rotations: MutexGuard<'static, ()>,
    _state: MutexGuard<'static, ()>,
}

impl Lane {
    fn new() -> Self {
        let state = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let rotations = lock_claude_session_rotations_for_tests();
        reset_state_for_tests();
        reset_deferred_adoptions_for_tests();
        let root = tempfile::tempdir().unwrap();
        set_test_root(Some(root.path()));
        Self {
            root,
            dir: tempfile::tempdir().unwrap(),
            _rotations: rotations,
            _state: state,
        }
    }

    fn transcript(&self, session: &str) -> PathBuf {
        let path = self.dir.path().join(format!("{session}.jsonl"));
        fs::write(&path, b"{}\n").unwrap();
        path
    }

    fn log(&self, channel: u64) -> PathBuf {
        let name = format!("{channel}.log");
        self.root.path().join(BINDING_EVENTS_DIR).join(name)
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        set_test_root(None);
        APPEND_FAULT.with(|fault| fault.set(None));
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

fn src(path: &Path, session: &str) -> SourceId {
    source_id(
        Some(session),
        &path.display().to_string(),
        &fs::metadata(path).unwrap(),
    )
}

fn hook(event: &str, source: Option<&str>) -> HookSignal {
    let payload = serde_json::json!({ "source": source });
    HookSignal::from_payload(event, &payload)
}

fn adopt(command: &str, payload: &str, hook: &HookSignal) -> Option<(String, String)> {
    adopt_claude_continuation_session(command, payload, hook).expect("binding event persisted")
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn events(channel: u64) -> Vec<BindingEvent> {
    binding_events_since(channel, 0).unwrap()
}

fn bound(tmux: &str) -> (String, Option<String>) {
    let binding = runtime_binding_for_tmux_session(tmux).unwrap();
    (binding.output_path, binding.session_id)
}

fn last_committed(channel: u64) -> Option<SourceId> {
    events(channel)
        .into_iter()
        .rev()
        .find_map(|event| match event.new {
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Some(source),
            _ => None,
        })
}

#[test]
fn b_hook_handled_before_the_a_tail_is_read_still_records_old_a() {
    let lane = Lane::new();
    for (channel, logged_first) in [(7_001, true), (7_002, false)] {
        let tmux = format!("p5-old-a-{channel}");
        let (a, b) = (uuid(), uuid());
        let a_path = lane.transcript(&a);
        register_provider_session("claude", &a, &tmux);
        // The second pane had no record for A before B arrived, so `old` must come from memory.
        if logged_first {
            register_tmux_channel(&tmux, channel);
        }
        register_tmux_runtime_binding(&tmux, claude(&a_path, &a));
        register_tmux_channel(&tmux, channel);
        let mut tail = OpenOptions::new().append(true).open(&a_path).unwrap();
        tail.write_all(b"{\"unread\":\"A[n:]\"}\n").unwrap();
        let b_path = lane.transcript(&b);

        assert!(adopt(&a, &b, &hook("stop", None)).is_some());

        let log = events(channel);
        let switch = log.last().unwrap();
        assert_eq!(switch.old, Some(src(&a_path, &a)));
        assert_eq!(switch.new, BindingTarget::Source(src(&b_path, &b)));
        assert_eq!(log.len(), if logged_first { 2 } else { 1 });
        assert_eq!(bound(&tmux).0, b_path.display().to_string());
    }
}

#[test]
fn a_b_c_chain_links_seq_and_old_and_a_late_b_is_only_audited() {
    let lane = Lane::new();
    let (channel, tmux) = (7_010, "p5-chain");
    let (a, b, c) = (uuid(), uuid(), uuid());
    let (a_path, b_path, c_path) = (
        lane.transcript(&a),
        lane.transcript(&b),
        lane.transcript(&c),
    );
    filetime::set_file_mtime(&b_path, filetime::FileTime::from_unix_time(20, 0)).unwrap();
    filetime::set_file_mtime(&c_path, filetime::FileTime::from_unix_time(30, 0)).unwrap();
    register_provider_session("claude", &a, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    let clear = hook("session_start", Some("clear"));
    assert!(adopt(&a, &b, &clear).is_some());
    assert!(adopt(&a, &c, &clear).is_some());
    let mut rx = subscribe_binding_events(channel).unwrap();
    assert_eq!(*rx.borrow_and_update(), 3);

    assert!(adopt(&a, &b, &hook("stop", None)).is_none());
    assert!(adopt(&a, &b, &hook("stop", None)).is_none());

    let log = events(channel);
    let seqs: Vec<u64> = log.iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        [1, 2, 3, 4],
        "one audit record for repeated late hooks"
    );
    let olds: Vec<_> = log.iter().map(|e| e.old.clone()).collect();
    let (sa, sb, sc) = (src(&a_path, &a), src(&b_path, &b), src(&c_path, &c));
    assert_eq!(olds, [None, Some(sa), Some(sb), Some(sc.clone())]);
    assert_eq!(log[2].new, BindingTarget::Source(sc));
    assert_eq!(
        (log[1].cause, log[1].parent_hint.clone()),
        (BindingCause::Clear, None)
    );
    let BindingTarget::Rejected {
        payload_session_id, ..
    } = &log[3].new
    else {
        panic!("late B must be a Rejected record: {:?}", log[3].new);
    };
    assert_eq!(payload_session_id, &b);
    assert!(rx.has_changed().unwrap());
    assert_eq!(
        bound(tmux).1.as_deref(),
        Some(c.as_str()),
        "the binding stays on C"
    );
    assert_eq!(last_committed(channel).map(|s| s.path), Some(c_path));
}

#[test]
fn fork_fixture_is_pending_until_its_transcript_exists_then_resolved_with_parent_a() {
    let lane = Lane::new();
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hook_payload/claude-2.1.283.json"
    );
    let fixture: serde_json::Value = serde_json::from_slice(&fs::read(fixture).unwrap()).unwrap();
    let runs = fixture["runs"].as_array().unwrap();
    let run = runs.iter().find(|run| run["name"] == "print_fork").unwrap();
    let steps = run["events"].as_array().unwrap();
    let parent = steps[0]["command_session_id"].as_str().unwrap();
    let fork = steps[0]["payload"]["session_id"].as_str().unwrap();
    let (channel, tmux) = (7_020, "p5-fork");
    let parent_path = lane.transcript(parent);
    let fork_path = lane.dir.path().join(format!("{fork}.jsonl"));
    register_provider_session("claude", parent, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&parent_path, parent));

    let mut adopted = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        if index == 2 {
            // A restart between the Pending record and the transcript must not break the chain.
            forget_channel_for_tests(channel);
            reset_state_for_tests();
            register_provider_session("claude", parent, tmux);
            let binding = claude(&parent_path, parent);
            register_rehydrated_tmux_runtime_binding("claude", tmux, channel, binding);
        }
        if step["transcript_exists_at_hook"] == true && !fork_path.exists() {
            fs::write(&fork_path, b"{}\n").unwrap();
        }
        let payload = &step["payload"];
        let event = HookEventKind::from_path(step["event"].as_str().unwrap());
        let signal = HookSignal::from_payload(event.as_str(), payload);
        let command = step["command_session_id"].as_str().unwrap();
        let session = payload["session_id"].as_str().unwrap();
        adopted.push(adopt(command, session, &signal).is_some());
    }

    assert_eq!(
        adopted,
        [false, false, true, true],
        "existing adoption judgment"
    );
    let log = events(channel);
    assert_eq!(
        log.len(),
        3,
        "a repeated hook before the file exists adds nothing"
    );
    let a = src(&parent_path, parent);
    let payload_path = steps[0]["payload"]["transcript_path"]
        .as_str()
        .map(str::to_owned);
    let pending = BindingTarget::Pending {
        payload_session_id: fork.to_owned(),
        payload_transcript_path: payload_path,
    };
    assert_eq!(log[1].new, pending);
    let resolved = BindingTarget::Resolved {
        pending_seq: log[1].seq,
        source: src(&fork_path, fork),
    };
    assert_eq!(log[2].new, resolved);
    for record in &log[1..] {
        assert_eq!(record.cause, BindingCause::Fork);
        assert_eq!(record.parent_hint.as_ref(), Some(&a));
        assert_eq!(record.old.as_ref(), Some(&a));
    }
    assert_eq!(log[1].evidence.hook_event.as_deref(), Some("session_start"));
    assert_eq!(bound(tmux).0, fork_path.display().to_string());
}

#[test]
fn crash_leaves_log_and_memory_on_the_same_source() {
    let lane = Lane::new();
    let (channel, tmux) = (7_030, "p5-crash");
    let (a, b, c) = (uuid(), uuid(), uuid());
    let (a_path, b_path, c_path) = (
        lane.transcript(&a),
        lane.transcript(&b),
        lane.transcript(&c),
    );
    filetime::set_file_mtime(&b_path, filetime::FileTime::from_unix_time(20, 0)).unwrap();
    filetime::set_file_mtime(&c_path, filetime::FileTime::from_unix_time(30, 0)).unwrap();
    register_provider_session("claude", &a, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    assert!(adopt(&a, &b, &hook("stop", None)).is_some());
    let restart = |binding: TuiRuntimeBinding| {
        forget_channel_for_tests(channel);
        reset_state_for_tests();
        register_provider_session("claude", &a, tmux);
        register_rehydrated_tmux_runtime_binding("claude", tmux, channel, binding);
    };

    // Crash in the middle of the next append: the torn line was never published.
    let mut torn = OpenOptions::new()
        .append(true)
        .open(lane.log(channel))
        .unwrap();
    torn.write_all(b"{\"seq\":3,\"chan").unwrap();
    restart(claude(&b_path, &b));
    assert_eq!(events(channel).len(), 2);
    assert!(fs::read(lane.log(channel)).unwrap().ends_with(b"\n"));
    assert_eq!(
        last_committed(channel).map(|s| s.path),
        Some(PathBuf::from(bound(tmux).0))
    );

    // A failed fsync publishes nothing and leaves no line for a reader or a reload.
    let size = fs::metadata(lane.log(channel)).unwrap().len();
    let rx = subscribe_binding_events(channel).unwrap();
    APPEND_FAULT.with(|fault| fault.set(Some("sync")));
    assert!(adopt_claude_continuation_session(&a, &c, &hook("stop", None)).is_err());
    APPEND_FAULT.with(|fault| fault.set(None));
    assert_eq!(bound(tmux).0, b_path.display().to_string(), "fail-closed");
    assert_eq!(fs::metadata(lane.log(channel)).unwrap().len(), size);
    APPEND_FAULT.with(|fault| fault.set(Some("write")));
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    APPEND_FAULT.with(|fault| fault.set(None));
    assert_eq!(
        bound(tmux).0,
        b_path.display().to_string(),
        "registration too"
    );
    assert!(!rx.has_changed().unwrap());
    assert!(adopt(&a, &c, &hook("stop", None)).is_some());
    assert_eq!(
        events(channel).last().map(|e| e.seq),
        Some(3),
        "seq stays contiguous"
    );

    // Crash after an append but before its publish: the restart binding is logged on top of it.
    let d = uuid();
    let d_path = lane.transcript(&d);
    let unpublished = claude(&d_path, &d);
    let orphan = Proposal::for_binding(
        Some(channel),
        tmux,
        &unpublished,
        None,
        CauseSource::Observed,
    );
    record_source(&orphan.unwrap()).unwrap();
    restart(claude(&c_path, &c));
    let log = events(channel);
    assert_eq!(
        log.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    assert_eq!(log[4].old, Some(src(&d_path, &d)));
    assert_eq!(
        last_committed(channel).map(|s| s.path),
        Some(PathBuf::from(bound(tmux).0))
    );
}

/// The same hook and registration sequence, once without a log and once with one.
fn judgment_trace(dir: &Path, tmux: &str, channel: u64) -> Vec<String> {
    reset_state_for_tests();
    let name = |path: &str| {
        Path::new(path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    let (a, b, c) = (
        "a0000000-0000-4000-8000-000000000001",
        "b0000000-0000-4000-8000-000000000002",
        "c0000000-0000-4000-8000-000000000003",
    );
    let path = |session: &str| dir.join(format!("{session}.jsonl"));
    let mut trace = Vec::new();
    let mut note = |label: &str, adopted: Option<(String, String)>| {
        let binding = runtime_binding_for_tmux_session(tmux).unwrap();
        let adopted = adopted.map(|(_, path)| name(&path));
        let now = (
            name(&binding.output_path),
            binding.session_id,
            binding.last_offset,
        );
        trace.push(format!("{label}: {adopted:?} -> {now:?}"));
    };
    register_provider_session("claude", a, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&path(a), a));
    note("register a", None);
    let _ = fs::remove_file(path(b));
    note(
        "b missing",
        adopt(a, b, &hook("session_start", Some("clear"))),
    );
    fs::write(path(b), b"{}\n").unwrap();
    filetime::set_file_mtime(path(b), filetime::FileTime::from_unix_time(20, 0)).unwrap();
    note("b present", adopt(a, b, &hook("stop", None)));
    note("b again", adopt(a, b, &hook("stop", None)));
    note("c", adopt(a, c, &hook("session_start", Some("compact"))));
    note("late b", adopt(a, b, &hook("stop", None)));
    let mut progressed = runtime_binding_for_tmux_session(tmux).unwrap();
    progressed.last_offset = 9;
    register_tmux_runtime_binding(tmux, progressed);
    note("progress", None);
    register_rehydrated_tmux_runtime_binding("claude", tmux, channel, claude(&path(a), a));
    note("rehydrate a", None);
    register_launched_tmux_runtime_binding(tmux, claude(&path(c), c));
    note("launch c", None);
    trace
}

#[test]
fn binding_judgment_is_the_same_with_and_without_the_log() {
    let lane = Lane::new();
    for session in [
        "a0000000-0000-4000-8000-000000000001",
        "c0000000-0000-4000-8000-000000000003",
    ] {
        lane.transcript(session);
    }
    filetime::set_file_mtime(
        lane.dir
            .path()
            .join("c0000000-0000-4000-8000-000000000003.jsonl"),
        filetime::FileTime::from_unix_time(30, 0),
    )
    .unwrap();
    set_test_root(None);
    let without = judgment_trace(lane.dir.path(), "p5-judgment-off", 7_040);
    set_test_root(Some(lane.root.path()));
    let with = judgment_trace(lane.dir.path(), "p5-judgment-on", 7_041);
    assert_eq!(without, with);
    assert!(!lane.log(7_040).exists());
    let kinds: Vec<_> = events(7_041)
        .iter()
        .map(|e| std::mem::discriminant(&e.new))
        .collect();
    assert_eq!(kinds.len(), 7, "{:#?}", events(7_041));
}

#[cfg(unix)]
#[test]
fn launch_cause_comes_from_the_execution_context_only_once() {
    use crate::services::tmux_common as tc;
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation, tests::fixture,
    };
    let (context_root, _env) = fixture();
    let lane = Lane::new();
    let (channel, tmux) = (7_050, "p5-launch");
    let launch = |mode: &str| {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: Utc::now(),
            execution_nonce: nonce.clone(),
            tmux_session: tmux.into(),
            channel_id: Some(channel),
            owner_runtime_root: context_root.path().display().to_string(),
            host: None,
            expected_native_session_id: None,
            launch_mode: mode.into(),
            provider_root: None,
        };
        PreparedIncarnation::create(context).unwrap();
        fs::write(tc::session_temp_path(tmux, "spawn_nonce"), &nonce).unwrap();
        nonce
    };
    register_tmux_channel(tmux, channel);
    let fresh = launch("fresh");
    let (a, b, c, d) = (uuid(), uuid(), uuid(), uuid());
    register_launched_tmux_runtime_binding(tmux, claude(&lane.transcript(&a), &a));
    register_launched_tmux_runtime_binding(tmux, claude(&lane.transcript(&b), &b));
    launch("resume");
    register_launched_tmux_runtime_binding(tmux, claude(&lane.transcript(&c), &c));
    register_tmux_runtime_binding(tmux, claude(&lane.transcript(&d), &d));

    let log = events(channel);
    let causes: Vec<_> = log.iter().map(|e| e.cause).collect();
    use BindingCause::{Resume, Startup, Unknown};
    assert_eq!(causes, [Startup, Unknown, Resume, Unknown]);
    assert_eq!(log[0].execution_nonce.as_deref(), Some(fresh.as_str()));
    assert!(log.iter().all(|e| e.parent_hint.is_none()));
    let _ = fs::remove_file(tc::session_temp_path(tmux, "spawn_nonce"));
}

#[test]
fn log_outage_defers_a_hook_adoption_and_the_idle_poll_adopts_b_without_another_hook() {
    use crate::services::claude_tui::hook_server::relay_receipts::{
        RELAY_DEADLINE_HEADER, RELAY_PUBLISHED_AT_HEADER, RELAY_REQUEST_ID_HEADER,
    };
    use crate::services::claude_tui::hook_server::{
        HookServerState, adoption_retry::deferred_adoption_count, hook_receiver_router_with_state,
        retry_deferred_claude_adoptions,
    };
    use tower::ServiceExt;
    let lane = Lane::new();
    let (channel, tmux) = (7_060, "p5-deferred");
    let (a, b) = (uuid(), uuid());
    let (a_path, b_path) = (lane.transcript(&a), lane.transcript(&b));
    register_provider_session("claude", &a, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let app = hook_receiver_router_with_state(HookServerState::new());
    let (request_id, now) = (uuid(), Utc::now());
    let payload = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": b,
        "source": "clear",
        "transcript_path": b_path,
    });
    let send = || {
        let request =
            axum::http::Request::post(format!("/hooks/claude/SessionStart?session_id={a}"))
                .header("content-type", "application/json")
                .header(RELAY_REQUEST_ID_HEADER, &request_id)
                .header(RELAY_PUBLISHED_AT_HEADER, now.to_rfc3339())
                .header(
                    RELAY_DEADLINE_HEADER,
                    (now + chrono::Duration::minutes(5)).to_rfc3339(),
                )
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap();
        runtime.block_on(async {
            let response = app.clone().oneshot(request).await.unwrap();
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        })
    };
    let b_len = fs::metadata(&b_path).unwrap().len();

    APPEND_FAULT.with(|fault| fault.set(Some("write")));
    let (status, body) = send();
    assert_eq!(
        (status, &body["binding_adoption"]),
        (202, &serde_json::json!("deferred"))
    );
    assert_eq!(
        send().1,
        body,
        "the receipt replays the deferral, not a plain success"
    );
    retry_deferred_claude_adoptions();
    assert_eq!(
        bound(tmux).0,
        a_path.display().to_string(),
        "still A during the outage"
    );
    assert_eq!(deferred_adoption_count(), 1);

    APPEND_FAULT.with(|fault| fault.set(None));
    retry_deferred_claude_adoptions();
    assert_eq!(bound(tmux), (b_path.display().to_string(), Some(b.clone())));
    assert_eq!(
        fs::metadata(&b_path).unwrap().len(),
        b_len,
        "B did not grow"
    );
    assert_eq!(deferred_adoption_count(), 1, "held until A→B settles");
    crate::services::tui_prompt_dedupe::clear_claude_session_rotation(tmux);
    retry_deferred_claude_adoptions();
    assert_eq!(deferred_adoption_count(), 0);
    let switch = events(channel).pop().unwrap();
    assert_eq!(switch.new, BindingTarget::Source(src(&b_path, &b)));
    assert_eq!(
        switch.cause,
        BindingCause::Clear,
        "the original hook is the evidence"
    );
    assert_eq!(switch.evidence.hook_event.as_deref(), Some("session_start"));
}

#[test]
fn a_reloaded_log_is_made_durable_before_the_same_source_is_published() {
    let lane = Lane::new();
    let (channel, tmux) = (7_070, "p5-reload");
    let a = uuid();
    let a_path = lane.transcript(&a);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    forget_channel_for_tests(channel);
    reset_state_for_tests();

    APPEND_FAULT.with(|fault| fault.set(Some("reload")));
    register_rehydrated_tmux_runtime_binding("claude", tmux, channel, claude(&a_path, &a));
    assert!(runtime_binding_for_tmux_session(tmux).is_none());
    APPEND_FAULT.with(|fault| fault.set(None));
    register_rehydrated_tmux_runtime_binding("claude", tmux, channel, claude(&a_path, &a));
    assert_eq!(bound(tmux).0, a_path.display().to_string());
    assert_eq!(
        events(channel).len(),
        1,
        "the reloaded record needs no second append"
    );
}

#[cfg(unix)]
#[test]
fn a_transcript_replaced_on_the_same_path_is_a_new_source() {
    let lane = Lane::new();
    let (channel, tmux) = (7_080, "p5-replaced");
    let a = uuid();
    let a_path = lane.transcript(&a);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    let first = src(&a_path, &a);
    let replacement = lane.dir.path().join("replacement.jsonl");
    fs::write(&replacement, b"{}\n").unwrap();
    fs::rename(&replacement, &a_path).unwrap();
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));

    let log = events(channel);
    assert_eq!(log.len(), 2);
    assert_ne!(src(&a_path, &a), first);
    assert_eq!(log[1].old, Some(first));
    assert_eq!(log[1].new, BindingTarget::Source(src(&a_path, &a)));
}

#[test]
fn a_deferred_b_is_adopted_and_handed_to_the_rotation_before_a_deferred_c() {
    use crate::services::claude_tui::hook_server::relay_receipts::{
        RELAY_DEADLINE_HEADER, RELAY_PUBLISHED_AT_HEADER, RELAY_REQUEST_ID_HEADER,
    };
    use crate::services::claude_tui::hook_server::{
        HookServerState, adoption_retry::deferred_adoption_count, hook_receiver_router_with_state,
        retry_deferred_claude_adoptions,
    };
    use crate::services::tui_prompt_dedupe::{
        advance_tmux_runtime_binding_offset, claude_session_rotation_for_tmux,
        clear_claude_session_rotation,
    };
    use tower::ServiceExt;
    let lane = Lane::new();
    let (channel, tmux) = (7_090, "p5-queue");
    let (a, b, c) = (uuid(), uuid(), uuid());
    let (a_path, b_path, c_path) = (
        lane.transcript(&a),
        lane.transcript(&b),
        lane.transcript(&c),
    );
    let prompt = b"{\"type\":\"user\",\"text\":\"B-only prompt\"}\n";
    let answer = b"{\"type\":\"assistant\",\"text\":\"B-only answer\"}\n";
    fs::write(&b_path, [&prompt[..], &answer[..]].concat()).unwrap();
    filetime::set_file_mtime(&b_path, filetime::FileTime::from_unix_time(20, 0)).unwrap();
    filetime::set_file_mtime(&c_path, filetime::FileTime::from_unix_time(30, 0)).unwrap();
    register_provider_session("claude", &a, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let app = hook_receiver_router_with_state(HookServerState::new());
    let send = |event: &str, session: &str, source: Option<&str>| {
        let now = Utc::now();
        let payload = serde_json::json!({
            "session_id": session,
            "source": source,
            "transcript_path": lane.dir.path().join(format!("{session}.jsonl")),
        });
        let request = axum::http::Request::post(format!("/hooks/claude/{event}?session_id={a}"))
            .header("content-type", "application/json")
            .header(RELAY_REQUEST_ID_HEADER, uuid())
            .header(RELAY_PUBLISHED_AT_HEADER, now.to_rfc3339())
            .header(
                RELAY_DEADLINE_HEADER,
                (now + chrono::Duration::minutes(5)).to_rfc3339(),
            )
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
        runtime.block_on(async {
            let response = app.clone().oneshot(request).await.unwrap();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["binding_adoption"].clone()
        })
    };

    APPEND_FAULT.with(|fault| fault.set(Some("write")));
    for (event, session, source) in [
        ("SessionStart", &b, Some("clear")),
        ("UserPromptSubmit", &b, None),
        ("Stop", &b, None),
        ("SessionStart", &c, Some("clear")),
    ] {
        assert_eq!(
            send(event, session, source),
            "deferred",
            "{event} {session}"
        );
    }
    assert_eq!(deferred_adoption_count(), 2, "B and C stay separate");
    APPEND_FAULT.with(|fault| fault.set(None));
    assert_eq!(
        send("Stop", &c, None),
        "deferred",
        "a C hook after recovery still waits behind B"
    );
    assert_eq!(deferred_adoption_count(), 2);

    retry_deferred_claude_adoptions();
    let binding = runtime_binding_for_tmux_session(tmux).unwrap();
    assert_eq!(bound(tmux), (b_path.display().to_string(), Some(b.clone())));
    assert_eq!(
        binding.last_offset, 0,
        "B's own lines are owed from its head"
    );
    let rotation = claude_session_rotation_for_tmux(tmux).unwrap();
    assert_eq!(rotation.old_output_path, a_path.display().to_string());
    retry_deferred_claude_adoptions();
    assert_eq!(
        bound(tmux).1.as_deref(),
        Some(b.as_str()),
        "C waits for A→B"
    );

    // The relay reads B's prompt, then the settle pass retires A→B.
    let b_str = b_path.display().to_string();
    assert!(advance_tmux_runtime_binding_offset(
        tmux,
        &b_str,
        prompt.len() as u64
    ));
    assert!(clear_claude_session_rotation(tmux));
    retry_deferred_claude_adoptions();
    assert_eq!(bound(tmux), (c_path.display().to_string(), Some(c.clone())));
    assert_eq!(deferred_adoption_count(), 1, "C is held until B→C settles");
    let rotation = claude_session_rotation_for_tmux(tmux).unwrap();
    assert_eq!(
        (rotation.old_output_path, rotation.old_last_offset),
        (b_str, prompt.len() as u64),
        "B's unread answer is handed to the B→C rotation"
    );
    let log = events(channel);
    let tail: Vec<_> = log[log.len() - 2..]
        .iter()
        .map(|e| (e.new.clone(), e.cause, e.evidence.hook_event.clone()))
        .collect();
    let start = Some("session_start".to_owned());
    assert_eq!(
        tail,
        [
            (
                BindingTarget::Source(src(&b_path, &b)),
                BindingCause::Clear,
                start.clone()
            ),
            (
                BindingTarget::Source(src(&c_path, &c)),
                BindingCause::Clear,
                start
            ),
        ]
    );
}

#[test]
fn a_follow_up_hook_keeps_the_first_session_start_as_the_deferred_evidence() {
    use crate::services::claude_tui::hook_server::adoption_retry::{
        AdoptionReport, adopt_from_hook, deferred_adoption_count,
    };
    use crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions;
    let lane = Lane::new();
    let (channel, tmux) = (7_100, "p5-evidence");
    let (a, b) = (uuid(), uuid());
    let (a_path, b_path) = (lane.transcript(&a), lane.transcript(&b));
    register_provider_session("claude", &a, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));

    APPEND_FAULT.with(|fault| fault.set(Some("write")));
    let start = hook("session_start", Some("clear"));
    assert_eq!(adopt_from_hook(&a, &b, &start), AdoptionReport::Deferred);
    let stop = hook("stop", None);
    assert_eq!(adopt_from_hook(&a, &b, &stop), AdoptionReport::Deferred);
    assert_eq!(deferred_adoption_count(), 1);
    APPEND_FAULT.with(|fault| fault.set(None));
    retry_deferred_claude_adoptions();

    assert_eq!(bound(tmux).0, b_path.display().to_string());
    let switch = events(channel).pop().unwrap();
    assert_eq!(
        (switch.cause, switch.evidence.hook_event.as_deref()),
        (BindingCause::Clear, Some("session_start"))
    );
    assert_eq!(switch.evidence.received_at, start.received_at);
}

#[test]
fn a_retry_paused_before_its_artifacts_cannot_overwrite_a_later_hooks_cutover() {
    use crate::services::claude_tui::hook_server::adoption_retry::{
        AdoptionReport, adopt_from_hook, set_artifact_probe,
    };
    use crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions;
    use crate::services::tui_prompt_dedupe::clear_claude_session_rotation;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    let lane = Lane::new();
    let (channel, tmux) = (7_110, "p5-barrier");
    let (a, b, c) = (uuid(), uuid(), uuid());
    let (a_path, b_path, c_path) = (
        lane.transcript(&a),
        lane.transcript(&b),
        lane.transcript(&c),
    );
    filetime::set_file_mtime(&b_path, filetime::FileTime::from_unix_time(20, 0)).unwrap();
    filetime::set_file_mtime(&c_path, filetime::FileTime::from_unix_time(30, 0)).unwrap();
    register_provider_session("claude", &a, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    let clear = hook("session_start", Some("clear"));
    APPEND_FAULT.with(|fault| fault.set(Some("write")));
    assert_eq!(adopt_from_hook(&a, &b, &clear), AdoptionReport::Deferred);
    APPEND_FAULT.with(|fault| fault.set(None));
    retry_deferred_claude_adoptions();
    assert!(clear_claude_session_rotation(tmux));

    // B leaves the queue on the next retry. The probe stands in for the artifact write; B holds it until C finishes or 300ms pass.
    let (paused_tx, paused_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let done_rx = Mutex::new(done_rx);
    let written = Arc::new(Mutex::new(Vec::<String>::new()));
    let (probe_b, probe_c, log) = (b.clone(), c.clone(), written.clone());
    set_artifact_probe(Some(Arc::new(move |session: &str| {
        if session == probe_b {
            paused_tx.send(()).unwrap();
            let _ = done_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_millis(300));
        }
        if session == probe_b || session == probe_c {
            log.lock().unwrap().push(session.to_owned());
        }
    })));
    let root = lane.root.path().to_path_buf();
    let retry_root = root.clone();
    let retry = std::thread::spawn(move || {
        set_test_root(Some(&retry_root));
        retry_deferred_claude_adoptions();
    });
    paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (live_a, live_c) = (a.clone(), c.clone());
    let live = std::thread::spawn(move || {
        set_test_root(Some(&root));
        let report = adopt_from_hook(&live_a, &live_c, &clear);
        done_tx.send(()).ok();
        report
    });
    retry.join().unwrap();
    let report = live.join().unwrap();
    set_artifact_probe(None);

    assert_eq!(report, AdoptionReport::Adopted);
    assert_eq!(bound(tmux).1.as_deref(), Some(c.as_str()));
    assert_eq!(
        *written.lock().unwrap(),
        [b, c],
        "the last cutover is the bound C"
    );
}

#[test]
fn a_new_source_after_recovery_waits_until_the_retried_b_rotation_settles() {
    use crate::services::claude_tui::hook_server::adoption_retry::{
        AdoptionReport, adopt_from_hook, deferred_adoption_count,
    };
    use crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions;
    use crate::services::tui_prompt_dedupe::{
        claude_session_rotation_for_tmux, clear_claude_session_rotation,
    };
    let lane = Lane::new();
    let (channel, tmux) = (7_120, "p5-window");
    let (a, b, c) = (uuid(), uuid(), uuid());
    let (a_path, b_path, c_path) = (
        lane.transcript(&a),
        lane.transcript(&b),
        lane.transcript(&c),
    );
    filetime::set_file_mtime(&b_path, filetime::FileTime::from_unix_time(20, 0)).unwrap();
    filetime::set_file_mtime(&c_path, filetime::FileTime::from_unix_time(30, 0)).unwrap();
    register_provider_session("claude", &a, tmux);
    register_tmux_channel(tmux, channel);
    register_tmux_runtime_binding(tmux, claude(&a_path, &a));
    let clear = hook("session_start", Some("clear"));
    APPEND_FAULT.with(|fault| fault.set(Some("write")));
    assert_eq!(adopt_from_hook(&a, &b, &clear), AdoptionReport::Deferred);
    APPEND_FAULT.with(|fault| fault.set(None));
    retry_deferred_claude_adoptions();
    assert_eq!(bound(tmux).1.as_deref(), Some(b.as_str()));

    assert_eq!(
        adopt_from_hook(&a, &c, &clear),
        AdoptionReport::Deferred,
        "C's first hook lands before A→B settles"
    );
    assert_eq!(bound(tmux).1.as_deref(), Some(b.as_str()));
    assert_eq!(deferred_adoption_count(), 2);
    assert!(clear_claude_session_rotation(tmux));
    retry_deferred_claude_adoptions();
    assert_eq!(bound(tmux).1.as_deref(), Some(c.as_str()));
    let rotation = claude_session_rotation_for_tmux(tmux).unwrap();
    assert_eq!(rotation.old_output_path, b_path.display().to_string());
}
