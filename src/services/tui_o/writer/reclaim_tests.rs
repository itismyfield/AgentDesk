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

impl ProducerPane {
    /// Sends hook `event` through the receiver's binding judgment, published `secs` after launch.
    fn http(
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
