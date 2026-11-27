//! A Pending its pane moved past, driven from the hook receiver through the real binding judgment
//! to O: a later Pending, or a later prompt of the bound session, supersedes it and O passes it.

use super::*;
use crate::services::claude_tui::hook_server::adoption_retry::{
    deferred_adoption_count, retry_deferred_claude_adoptions,
};
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, ProceedReason, observe_binding_hook,
};
use crate::services::claude_tui::hook_server::relay_receipts::RELAY_PUBLISHED_AT_HEADER;
use crate::services::tui_o::store::rotation::Boundary;
use crate::services::tui_o::writer::binding::BindingLog;
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope, PreparedIncarnation,
};

impl ProducerPane {
    /// Sends hook `event` through the receiver's binding judgment, published `secs` after launch.
    pub(crate) fn http(
        &self,
        event: &str,
        source: Option<&str>,
        command: &str,
        session: &str,
        path: &Path,
        secs: i64,
    ) -> IngressOutcome {
        let payload = serde_json::json!({ "source": source, "transcript_path": path });
        let at = self.base + TimeDelta::seconds(secs);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(RELAY_PUBLISHED_AT_HEADER, at.to_rfc3339().parse().unwrap());
        crate::services::tui_prompt_dedupe::clear_claude_session_rotation(self.tmux);
        observe_binding_hook(
            "claude",
            event,
            Some(command),
            Some(session),
            &payload,
            &headers,
        )
    }

    fn history(&self) -> crate::services::claude_tui::source_verify::SourceHistory {
        let marker = crate::services::tmux_common::session_temp_path(self.tmux, "spawn_nonce");
        let nonce = std::fs::read_to_string(marker).unwrap();
        p5::claude_history(CHANNEL, self.tmux, Some(nonce.trim()))
            .unwrap()
            .1
    }
}

/// A channel whose O store starts on launch session `a`'s transcript, and that transcript's path.
fn launched(a: &str) -> (Harness, PathBuf) {
    let mut bound_a = None;
    let harness = Harness::build(|runtime| {
        let path = runtime.join(format!("{a}.jsonl"));
        let body = session_row(a);
        std::fs::write(&path, &body).unwrap();
        let source_id = source_id_for(a, &path).unwrap();
        bound_a = Some(path);
        let delivery_start = body.len() as u64;
        let prefix_hash = hex::encode(Sha256::digest(&body));
        vec![InitSource {
            source_id,
            delivery_start,
            prefix_hash,
        }]
    });
    harness.gate.acquired();
    (harness, bound_a.unwrap())
}

fn execution_headers(pane: &ProducerPane, session: &str, path: &Path) -> axum::http::HeaderMap {
    let marker = crate::services::tmux_common::session_temp_path(pane.tmux, "spawn_nonce");
    let execution_nonce = std::fs::read_to_string(marker).unwrap();
    let prepared = PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: pane.base,
        execution_nonce,
        tmux_session: pane.tmux.into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: Some(session.into()),
        launch_mode: "fresh".into(),
        provider_root: path.parent().and_then(Path::parent).map(Path::to_path_buf),
        first_prompt_digest: None,
        source_policy: None,
        resume_start: None,
    })
    .unwrap();
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(prepared.context.into()),
        observed: Default::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    headers
}

#[tokio::test(start_paused = true)]
async fn hook_refresh_recovers_one_cursor_and_reposts_nothing_after_interrupt() {
    use crate::services::claude_tui::source_verify::boot_time_for_tests;
    use crate::services::tui_o::shadow::capture::renumber;
    use crate::services::tui_o::store::{STORE_DIR_NAME, StoreError, fault};

    for (step, committed) in [
        (fault::Step::RefreshBeforeIntent, false),
        (fault::Step::RefreshInstall, true),
    ] {
        let session = uuid::Uuid::new_v4().to_string();
        let (harness, path) = launched(&session);
        let pane = ProducerPane::launch(&session, &path);
        let headers = execution_headers(&pane, &session, &path);
        pane.http(
            "SessionStart",
            Some("startup"),
            &session,
            &session,
            &path,
            1,
        );
        let old = pane.history().current.unwrap().pin.unwrap();
        let bindings = Arc::new(BindingLog);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&path, &row("old", "already posted"));
        polls(4).await;
        halt(stop, task).await;
        let before = harness.channel().cursor(&old).unwrap().clone();
        assert_eq!(harness.port.posts(), ["already posted"]);

        let _boot = boot_time_for_tests(Some(Utc::now()));
        let _device = renumber::shift(&path, 1 << 40);
        let live = source_id_for(&session, &path).unwrap();
        let dir = harness
            ._runtime
            .path()
            .join(STORE_DIR_NAME)
            .join(CHANNEL.to_string());
        let failing = fault::plant(&dir, step, std::io::ErrorKind::StorageFull, Some(1));
        let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        let payload = serde_json::json!({"session_id": session, "transcript_path": path});
        assert!(
            !observe_binding_hook(
                "claude",
                "Stop",
                Some(&session),
                Some(&session),
                &payload,
                &headers
            )
            .refused()
        );
        polls(4).await;
        assert!(task.is_finished(), "{step:?} must interrupt publication");
        assert!(task.await.unwrap().is_some());
        drop(failing);

        if committed {
            assert!(
                matches!(harness.store.peek_cursors(CHANNEL), Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
            assert!(
                matches!(harness.store.peek_rotation(CHANNEL), Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
        } else {
            assert_eq!(
                harness.store.peek_cursors(CHANNEL).unwrap(),
                [before.clone()]
            );
        }
        let store = harness.channel();
        let mut cursor = before.clone();
        cursor.source = if committed { live.clone() } else { old };
        assert_eq!(store.cursors().collect::<Vec<_>>(), [&cursor]);
        drop(store);

        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(4).await;
        cursor.source = live;
        assert_eq!(harness.channel().cursors().collect::<Vec<_>>(), [&cursor]);
        assert_eq!(harness.port.posts(), ["already posted"]);
        append(&path, &row("new", "after recovery"));
        polls(4).await;
        assert_eq!(harness.port.posts(), ["already posted", "after recovery"]);
        halt(stop, task).await;
    }
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn hook_refresh_moves_the_cursor_to_the_live_device_without_repost() {
    use crate::services::claude_tui::source_verify::boot_time_for_tests;
    use crate::services::tui_o::shadow::capture::renumber;
    for event in ["UserPromptSubmit", "Stop"] {
        let session = uuid::Uuid::new_v4().to_string();
        let (harness, path) = launched(&session);
        let pane = ProducerPane::launch(&session, &path);
        let headers = execution_headers(&pane, &session, &path);
        pane.http(
            "SessionStart",
            Some("startup"),
            &session,
            &session,
            &path,
            1,
        );
        let stored = pane.history().current.unwrap().pin.unwrap();
        let bindings = Arc::new(BindingLog);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&path, &row("old", "before reboot"));
        polls(4).await;
        halt(stop, task).await;
        assert_eq!(harness.port.posts(), ["before reboot"]);
        let before = harness.channel().cursor(&stored).unwrap().clone();

        let _boot = boot_time_for_tests(Some(Utc::now()));
        let _renumber = renumber::shift(&path, 1 << 40);
        let live = source_id_for(&session, &path).unwrap();
        assert_ne!(stored.dev, live.dev);
        assert_eq!(stored.ino, live.ino);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        let payload = serde_json::json!({"session_id": session, "transcript_path": path});
        let ingress = observe_binding_hook(
            "claude",
            event,
            Some(&session),
            Some(&session),
            &payload,
            &headers,
        );
        assert!(!ingress.refused(), "{ingress:?}");
        polls(4).await;
        let pinned = pane.history().current.unwrap().pin.unwrap();
        assert_eq!(pinned, live, "{event} must refresh the verified pin");
        let store = harness.channel();
        let after = store.cursor(&live).expect("the live file owns the cursor");
        assert_eq!(after.captured_through, before.captured_through);
        assert_eq!(after.prefix_hash, before.prefix_hash);
        assert_eq!(store.cursors().count(), 1);
        drop(store);
        assert_eq!(harness.port.posts(), ["before reboot"]);
        append(&path, &row("new", "after reboot"));
        polls(4).await;
        assert_eq!(harness.port.posts(), ["before reboot", "after reboot"]);
        halt(stop, task).await;

        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(4).await;
        assert_eq!(harness.port.posts(), ["before reboot", "after reboot"]);
        halt(stop, task).await;
    }
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn hook_refresh_compaction_starts_at_its_own_cursor_and_keeps_posts_unique() {
    let [a, b] = [(); 2].map(|_| uuid::Uuid::new_v4().to_string());
    let (harness, a_path) = launched(&a);
    let pane = ProducerPane::launch(&a, &a_path);
    pane.http("SessionStart", Some("startup"), &a, &a, &a_path, 1);
    let a_source = source_id_for(&a, &a_path).unwrap();
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    let inherited = row("first", "inherited output");
    append(&a_path, &inherited);
    polls(4).await;
    assert_eq!(harness.port.posts(), ["inherited output"]);
    let old_cursor = harness.channel().cursor(&a_source).unwrap().clone();

    let b_path = a_path.with_file_name(format!("{b}.jsonl"));
    let next = row("second", "compacted output");
    let body = [session_row(&b), inherited, next].concat();
    std::fs::write(&b_path, &body).unwrap();
    pane.http("SessionStart", Some("compact"), &a, &b, &b_path, 2);
    pane.http("Stop", None, &a, &b, &b_path, 3);
    polls(4).await;
    let new = source_id_for(&b, &b_path).unwrap();
    assert_eq!(pane.history().current.unwrap().pin.unwrap(), new);
    let store = harness.channel();
    let cursor = store
        .cursor(&new)
        .expect("compaction attached its own file");
    assert_eq!(cursor.captured_through, body.len() as u64);
    assert_eq!(cursor.prefix_hash, hex::encode(Sha256::digest(&body)));
    assert_eq!(store.cursor(&a_source).unwrap(), &old_cursor);
    drop(store);
    assert_eq!(
        harness.port.posts(),
        ["inherited output", "compacted output"]
    );
    halt(stop, task).await;
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(4).await;
    assert_eq!(
        harness.port.posts(),
        ["inherited output", "compacted output"]
    );
    halt(stop, task).await;
}

fn kind(event: &BindingEvent) -> String {
    match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(s),
            evidence,
            ..
        } => format!("source:{}:{}", s.session_id, evidence.hook_event),
        BindingRecord::Bound {
            new: BindingTarget::Pending {
                payload_session_id, ..
            },
            ..
        } => format!("pending:{payload_session_id}"),
        BindingRecord::Resolved { source, .. } => format!("resolved:{}", source.session_id),
        BindingRecord::Rejected { .. } => "rejected".into(),
    }
}

/// O applied every logged event without a Pending boundary or a wait, and a restart may adopt it.
fn passed(harness: &Harness, events: &[BindingEvent], tag: &str) {
    let last = events.last().unwrap().seq;
    let checkpoint = harness.channel().binding_checkpoint().unwrap();
    assert_eq!(checkpoint, Some(last), "{tag} checkpoint {events:#?}");
    let rotation = harness.channel().rotation().unwrap();
    let held = |link: &&crate::services::tui_o::store::rotation::SourceLink| {
        matches!(link.boundary, Boundary::Pending { .. })
    };
    assert!(
        !rotation.links.values().any(|l| held(&l)),
        "{tag} no Pending boundary"
    );
    let alarms = harness.alarms.taken();
    let waits = |alarm: &WriterAlarm| {
        matches!(
            alarm,
            WriterAlarm::BindingPending { .. } | WriterAlarm::BoundaryPending { .. }
        )
    };
    assert!(!alarms.iter().any(waits), "{tag} no wait {alarms:?}");
    let logged = crate::services::tui_o::writer::adoption::logged(events);
    assert!(logged.is_ok(), "{tag} adoptable {:?}", logged.err());
}

/// The last record re-pins `s` on its prompt and the pane left `x` for good.
fn reclaimed(pane: &ProducerPane, events: &[BindingEvent], s: &str, x: &str, tag: &str) {
    let last = events.last().map(kind);
    let reclaim = format!("source:{s}:user_prompt_submit");
    assert_eq!(last, Some(reclaim), "{tag} {events:#?}");
    let history = pane.history();
    assert!(history.awaiting.is_none(), "{tag} awaiting {history:?}");
    assert!(history.left.contains_key(x), "{tag} left {history:?}");
}

/// O and the log writer restart on the same log: X's transcript appearing resolves nothing and
/// the bound transcript `out` keeps posting.
async fn restarted(
    harness: &Harness,
    pane: &ProducerPane,
    events: &[BindingEvent],
    x: &str,
    out: &Path,
) {
    let posted = harness.port.posts();
    p5::forget_channel_for_tests(CHANNEL);
    let s = pane.bound().unwrap();
    reclaimed(pane, events, &s, x, "[R5:reclaim_reload]");
    std::fs::write(out.with_file_name(format!("{x}.jsonl")), session_row(x)).unwrap();
    retry_deferred_claude_adoptions();
    append(out, &row("after", "after restart"));
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(6).await;
    let after = bindings.binding_events_since(CHANNEL, 0).unwrap();
    assert_eq!(after, events, "[R5:reclaim_reload] nothing more is logged");
    passed(harness, &after, "[R5:reclaim_reload]");
    let posts = harness.port.posts();
    assert_eq!(
        posts[..],
        [&posted[..], &["after restart".into()]].concat(),
        "[R5:reclaim_reload]"
    );
    halt(stop, task).await;
}

/// A → /clear X → /clear Y, neither written yet: Y's Pending replaces X's, so O passes X and binds
/// the resolved Y; X's transcript appearing later resolves nothing.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn o_passes_a_pending_its_successor_overwrote_and_binds_the_resolved_one() {
    let [a, x, y] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
    let (harness, a_path) = launched(&a);
    let path = |session: &str| a_path.with_file_name(format!("{session}.jsonl"));
    let pane = ProducerPane::launch(&a, &a_path);
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    pane.http("SessionStart", Some("clear"), &a, &x, &path(&x), 10);
    pane.http("SessionStart", Some("clear"), &a, &y, &path(&y), 20);
    std::fs::write(path(&y), session_row(&y)).unwrap();
    pane.http("UserPromptSubmit", None, &a, &y, &path(&y), 21);
    append(&path(&y), &row("n1", "y out"));
    std::fs::write(path(&x), session_row(&x)).unwrap();
    retry_deferred_claude_adoptions();
    assert_eq!(
        pane.bound().as_deref(),
        Some(y.as_str()),
        "[R5:overwrite] bound"
    );
    polls(6).await;
    let events = bindings.binding_events_since(CHANNEL, 0).unwrap();
    passed(&harness, &events, "[R5:overwrite]");
    let y_source = source_id_for(&y, &path(&y)).unwrap();
    assert!(
        harness.channel().cursor(&y_source).is_some(),
        "[R5:overwrite] reader"
    );
    assert_eq!(harness.port.posts(), ["y out"], "[R5:overwrite] posts");
    let kinds: Vec<String> = events.iter().map(kind).collect();
    let x_resolved = format!("resolved:{x}");
    assert!(!kinds.contains(&x_resolved), "[R5:no_x] {kinds:?}");
    halt(stop, task).await;
}

/// A later Pending of another pane on the channel does not replace the first pane's.
#[test]
fn o_keeps_waiting_on_a_pending_another_pane_did_not_overwrite() {
    let pending = |seq: u64, tmux: &str, session: &str| BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        tmux_session: tmux.into(),
        execution_nonce: "unit".into(),
        record: BindingRecord::Bound {
            old: None,
            new: BindingTarget::Pending {
                payload_session_id: session.into(),
                payload_transcript_path: PathBuf::from(format!("/t/{session}.jsonl")),
            },
            cause: crate::services::tui_o::writer::binding::BindingCause::Clear,
            parent_hint: None,
            evidence: crate::services::tui_o::writer::binding::BindingEvidence {
                hook_event: "session_start".into(),
                received_at: Utc::now(),
                reclaims: false,
            },
        },
        committed_at: Utc::now(),
    };
    let events = [pending(1, "pane-p", "x"), pending(2, "pane-q", "y")];
    let refused = crate::services::tui_o::writer::adoption::logged(&events).err();
    let refused = refused.map(|refused| refused.to_string());
    assert_eq!(
        refused.as_deref(),
        Some("bind 1 is still pending"),
        "[R5:other_pane]"
    );
}

/// Launch A, the pane clears to B, then a /clear X start delivered late logs a Pending; B's next
/// prompt, published after it, supersedes that Pending and O never waits on X.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_current_prompt_after_a_late_clear_reclaims_the_pane() {
    let [a, b, x] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
    let (harness, a_path) = launched(&a);
    let path = |session: &str| a_path.with_file_name(format!("{session}.jsonl"));
    let pane = ProducerPane::launch(&a, &a_path);
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    std::fs::write(path(&b), session_row(&b)).unwrap();
    pane.http("SessionStart", Some("clear"), &a, &b, &path(&b), 10);
    pane.http("UserPromptSubmit", None, &a, &b, &path(&b), 11);
    assert_eq!(pane.bound().as_deref(), Some(b.as_str()));
    pane.http("SessionStart", Some("clear"), &a, &x, &path(&x), 40);
    assert_eq!(pane.history().awaiting.map(|w| w.session), Some(x.clone()));
    pane.http("UserPromptSubmit", None, &a, &b, &path(&b), 45);
    append(&path(&b), &row("n1", "b out"));
    let events = bindings.binding_events_since(CHANNEL, 0).unwrap();
    reclaimed(&pane, &events, &b, &x, "[R5:reclaim]");
    polls(6).await;
    passed(&harness, &events, "[R5:reclaim]");
    assert_eq!(harness.port.posts(), ["b out"], "[R5:reclaim] posts");
    halt(stop, task).await;
    restarted(&harness, &pane, &events, &x, &path(&b)).await;
}

/// The same late /clear on a pane back on its launch session A: A's prompt names A as the launch
/// command does, so the receiver answers as before and still supersedes the Pending under the pane.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_launch_session_prompt_after_a_late_clear_reclaims_the_pane() {
    let [a, x] = [(); 2].map(|_| uuid::Uuid::new_v4().to_string());
    let (harness, a_path) = launched(&a);
    let path = |session: &str| a_path.with_file_name(format!("{session}.jsonl"));
    let pane = ProducerPane::launch(&a, &a_path);
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    let no_switch = |outcome| {
        matches!(
            outcome,
            IngressOutcome::Proceed(ProceedReason::NoSessionSwitch)
        )
    };
    let resumed = pane.http("SessionStart", Some("resume"), &a, &a, &a_path, 5);
    assert!(no_switch(resumed));
    pane.http("SessionStart", Some("clear"), &a, &x, &path(&x), 40);
    assert_eq!(pane.history().awaiting.map(|w| w.session), Some(x.clone()));
    assert_eq!(deferred_adoption_count(), 1);
    let answered = pane.http("UserPromptSubmit", None, &a, &a, &a_path, 45);
    assert!(no_switch(answered), "[R5:http]");
    append(&a_path, &row("m1", "a out"));
    let events = bindings.binding_events_since(CHANNEL, 0).unwrap();
    reclaimed(&pane, &events, &a, &x, "[R5:reclaim_launch]");
    assert_eq!(deferred_adoption_count(), 0, "[R5:reclaim_launch] queue");
    assert_eq!(pane.bound(), Some(a.clone()), "[R5:reclaim_launch]");
    polls(6).await;
    passed(&harness, &events, "[R5:reclaim_launch]");
    assert_eq!(harness.port.posts(), ["a out"], "[R5:reclaim_launch] posts");
    halt(stop, task).await;
    restarted(&harness, &pane, &events, &x, &a_path).await;
}

/// The pane is on S, registered without a hook and so unpinned, on an incomplete history: S's
/// prompt after a late /clear X pins S but supersedes nothing, in the writer, its fold or O.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_first_pin_on_an_incomplete_history_does_not_reclaim_the_pane() {
    use crate::services::tui_prompt_dedupe as dedupe;
    let [a, s, x] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
    let (harness, a_path) = launched(&a);
    let path = |session: &str| a_path.with_file_name(format!("{session}.jsonl"));
    let pane = ProducerPane::launch(&a, &a_path);
    std::fs::write(path(&s), session_row(&s)).unwrap();
    let binding = dedupe::TuiRuntimeBinding {
        runtime_kind: ClaudeTui,
        output_path: path(&s).display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(s.clone()),
        last_offset: 0,
        relay_last_offset: None,
    };
    let tmux = pane.tmux;
    assert!(dedupe::register_rehydrated_tmux_runtime_binding(
        "claude", tmux, CHANNEL, binding
    ));
    // A record logged while the spawn nonce is unreadable leaves this execution incomplete.
    let marker = crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
    let nonce = std::fs::read_to_string(&marker).unwrap();
    std::fs::remove_file(&marker).unwrap();
    pane.http("SessionStart", Some("clear"), &a, &x, &path(&x), 40);
    std::fs::write(&marker, nonce).unwrap();
    let waits = |pane: &ProducerPane| pane.history().awaiting.map(|w| w.session);
    assert!(!pane.history().complete && waits(&pane) == Some(x.clone()));
    let x_seq = BindingLog
        .binding_events_since(CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    pane.http("UserPromptSubmit", None, &a, &s, &path(&s), 45);
    assert_eq!(pane.bound(), Some(s.clone()));
    let events = BindingLog.binding_events_since(CHANNEL, 0).unwrap();
    let held = |events: &[BindingEvent], tag: &str| {
        let refused = crate::services::tui_o::writer::adoption::logged(events).err();
        let refused = refused.map(|refused| refused.to_string());
        let expected = format!("bind {x_seq} is still pending");
        assert_eq!(refused, Some(expected), "{tag} {events:#?}");
    };
    assert_eq!(waits(&pane), Some(x.clone()), "[R5:first_pin] fold");
    held(&events, "[R5:first_pin]");
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(6).await;
    let checkpoint = harness.channel().binding_checkpoint().unwrap();
    assert!(
        checkpoint < Some(x_seq),
        "[R5:first_pin] O waits on X: {checkpoint:?}"
    );
    halt(stop, task).await;

    p5::forget_channel_for_tests(CHANNEL);
    assert_eq!(waits(&pane), Some(x.clone()), "[R5:first_pin_reload] fold");
    let after = BindingLog.binding_events_since(CHANNEL, 0).unwrap();
    assert_eq!(after, events, "[R5:first_pin_reload]");
    held(&after, "[R5:first_pin_reload]");
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(6).await;
    let checkpoint = harness.channel().binding_checkpoint().unwrap();
    assert!(
        checkpoint < Some(x_seq),
        "[R5:first_pin_reload] O waits on X: {checkpoint:?}"
    );
    halt(stop, task).await;
}

/// Launch A, the pane clears to C, then an in-session /resume to B, never held: the pane moves to
/// B at its start, and O holds B's rows, history and new alike, until an operator resolves it.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn an_adopted_resume_holds_its_rows_until_an_operator_resolves() {
    let [a, b, c] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
    let (harness, a_path) = launched(&a);
    let path = |session: &str| a_path.with_file_name(format!("{session}.jsonl"));
    let pane = ProducerPane::launch(&a, &a_path);
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    std::fs::write(path(&c), session_row(&c)).unwrap();
    pane.http("SessionStart", Some("clear"), &a, &c, &path(&c), 10);
    pane.http("UserPromptSubmit", None, &a, &c, &path(&c), 11);
    let past = [session_row(&b), row("p1", "past 1"), row("p2", "past 2")].concat();
    std::fs::write(path(&b), &past).unwrap();
    pane.http("SessionStart", Some("resume"), &a, &b, &path(&b), 30);
    append(&path(&b), &row("n1", "b out"));
    polls(6).await;
    let b_source = source_id_for(&b, &path(&b)).unwrap();
    let rotation = harness.channel().rotation().unwrap();
    let boundary = rotation.link(&b_source).map(|link| link.boundary.clone());
    let held = Some(Boundary::Pending {
        candidates: vec![0],
    });
    assert_eq!(boundary, held, "[R5:resume_held] bound {:?}", pane.bound());
    let pending = WriterAlarm::BoundaryPending {
        source: b_source.clone(),
    };
    let alarms = harness.alarms.taken();
    let raised = alarms.iter().filter(|alarm| **alarm == pending).count();
    assert_eq!(raised, 1, "[R5:resume_held] {alarms:?}");
    assert!(harness.port.posts().is_empty(), "[R5:resume_held] posts");
    let spooled = harness.channel().retained_segments(&b_source);
    assert!(spooled > 0, "[R5:resume_held] spool");
    halt(stop, task).await;

    let resolved = harness
        .store
        .seed_boundary_resolved(CHANNEL, &b_source, past.len() as u64);
    assert!(resolved.is_ok(), "[R5:resume_resolved] {resolved:?}");
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(6).await;
    assert_eq!(harness.port.posts(), ["b out"], "[R5:resume_resolved]");
    halt(stop, task).await;
}

#[path = "transition_execution_tests.rs"]
mod transition_execution;
#[path = "transition_host_tests.rs"]
mod transition_host;
