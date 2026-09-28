use super::*;
use crate::services::claude_tui::hook_server::HookEventKind;
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

        assert!(adopt_claude_continuation_session(&a, &b, &hook("stop", None)).is_some());

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
    assert!(adopt_claude_continuation_session(&a, &b, &clear).is_some());
    assert!(adopt_claude_continuation_session(&a, &c, &clear).is_some());
    let mut rx = subscribe_binding_events(channel).unwrap();
    assert_eq!(*rx.borrow_and_update(), 3);

    assert!(adopt_claude_continuation_session(&a, &b, &hook("stop", None)).is_none());
    assert!(adopt_claude_continuation_session(&a, &b, &hook("stop", None)).is_none());

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
        adopted.push(adopt_claude_continuation_session(command, session, &signal).is_some());
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
    assert!(adopt_claude_continuation_session(&a, &b, &hook("stop", None)).is_some());
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
    assert!(adopt_claude_continuation_session(&a, &c, &hook("stop", None)).is_none());
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
    assert!(adopt_claude_continuation_session(&a, &c, &hook("stop", None)).is_some());
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
        adopt_claude_continuation_session(a, b, &hook("session_start", Some("clear"))),
    );
    fs::write(path(b), b"{}\n").unwrap();
    filetime::set_file_mtime(path(b), filetime::FileTime::from_unix_time(20, 0)).unwrap();
    note(
        "b present",
        adopt_claude_continuation_session(a, b, &hook("stop", None)),
    );
    note(
        "b again",
        adopt_claude_continuation_session(a, b, &hook("stop", None)),
    );
    note(
        "c",
        adopt_claude_continuation_session(a, c, &hook("session_start", Some("compact"))),
    );
    note(
        "late b",
        adopt_claude_continuation_session(a, b, &hook("stop", None)),
    );
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
