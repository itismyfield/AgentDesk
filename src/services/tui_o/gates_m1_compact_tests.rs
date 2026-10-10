use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::gates_m1_support::writer;
use crate::services::tui_input::actor::pane::{Pane, SendOutcome};
use crate::services::tui_input::actor::{InputActor, Step, token};
use crate::services::tui_input::attempt::{AttemptMeta, Effect, Tracking, WitnessKind};
use crate::services::tui_input::ledger::Ledger;
use crate::services::tui_input::rows::{AttemptEvidence, DoneReason, Entry, RowState};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::derive::TranscriptDerive;
use crate::services::tui_o::shadow::{
    CaptureBatch, CaptureOutcome, CaptureSource, DeriveOutput, ShadowDerive, ShadowProvider,
    SourceBinding, SourceId, UnitKey, UnitKind,
};
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::actor::{POLL_INTERVAL, run_channel};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::input_facts::{InputFacts, TurnState};

const CHANNEL: u64 = 632_506;
const SESSION: &str = "native-auto-session";
const NONCE: &str = "compact-test-incarnation";
const TOKEN: &str = "00000000000000000000000000000006";
const READY: &str = "⏺ Done.\n────────────────────\n❯\u{a0}\n────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";

#[derive(Default)]
struct FakePane {
    entered: Vec<String>,
}

impl Pane for FakePane {
    fn capture(&mut self) -> Result<String, String> {
        Ok(READY.into())
    }

    fn submit(&mut self, text: &str) -> SendOutcome {
        self.entered.push(text.into());
        SendOutcome::Sent
    }

    fn execution_nonce(&self) -> Option<String> {
        Some(NONCE.into())
    }
}

struct Bindings {
    events: Mutex<Vec<BindingEvent>>,
    notice: watch::Sender<u64>,
}

impl Bindings {
    fn new() -> Self {
        Self {
            events: Mutex::default(),
            notice: watch::channel(0).0,
        }
    }

    fn bind(&self, seq: u64, source: &SourceId, cause: BindingCause) {
        let at = Utc::now();
        let old = (seq > 1).then(|| source.clone());
        let event = BindingEvent {
            seq,
            channel_id: CHANNEL,
            provider: ShadowProvider::Claude,
            tmux_session: "fake-compact-pane".into(),
            execution_nonce: NONCE.into(),
            record: BindingRecord::Bound {
                old: old.clone(),
                new: BindingTarget::Source(source.clone()),
                cause,
                parent_hint: old,
                evidence: BindingEvidence {
                    hook_event: if seq == 1 {
                        "session_start"
                    } else {
                        "post_compact"
                    }
                    .into(),
                    received_at: at,
                    reclaims: false,
                },
            },
            committed_at: at,
        };
        self.events.lock().unwrap().push(event);
        self.notice.send_replace(seq);
    }
}

impl BindingEvents for Bindings {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        assert_eq!(channel, CHANNEL);
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.seq > after)
            .cloned()
            .collect())
    }

    fn subscribe(&self, channel: u64) -> watch::Receiver<u64> {
        assert_eq!(channel, CHANNEL);
        self.notice.subscribe()
    }
}

fn append(path: &Path, records: &[Value]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
}

fn body(id: &str, text: &str) -> Value {
    json!({"type":"assistant", "sessionId":SESSION, "uuid":format!("row-{id}"),
        "parentUuid":"own-native-turn", "apiBlockIndex":0,
        "message":{"id":id,"content":[{"type":"text","text":text}]}})
}

fn batch(capture: &mut SourceCapture) -> CaptureBatch {
    match capture.poll(u64::MAX) {
        CaptureOutcome::Batch(batch) => batch,
        CaptureOutcome::Anomaly(anomaly) => panic!("{anomaly:?}"),
    }
}

fn sealed(output: &[DeriveOutput]) -> Vec<UnitKey> {
    output
        .iter()
        .filter_map(|item| match item {
            DeriveOutput::Sealed(unit) => Some(unit.unit_key.clone()),
            _ => None,
        })
        .collect()
}

fn key(id: &str) -> UnitKey {
    UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: format!("{id}:0"),
        kind: UnitKind::Body,
    }
}

async fn offered_input_keeps_runtime_attempt_through_compact() {
    println!("PROBE runtime_attempt begin actual_offer=true");
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("offered-parent.jsonl");
    fs::write(
        &path,
        format!("{}\n", json!({"type":"system","subtype":"turn_duration"})),
    )
    .unwrap();
    let binding = SourceBinding {
        channel_id: CHANNEL + 1,
        provider: ShadowProvider::Claude,
        source: source_id_for(SESSION, &path).unwrap(),
    };
    let mut facts = InputFacts::open(binding.clone()).unwrap();
    let idle = facts.poll(u64::MAX).unwrap();
    assert_eq!(idle.state, TurnState::Idle);
    let input_runtime = fs::canonicalize(root.path()).unwrap().join("input");
    let mut inputs = Ledger::open(&input_runtime, binding.channel_id).unwrap();
    for (id, text) in [
        (1, "keep the actual offered input running"),
        (2, "next input"),
    ] {
        inputs
            .append_entry(
                &Entry::Received {
                    key: id,
                    input: json!({"text":text}),
                },
                &[],
            )
            .unwrap();
    }
    let mut actor = InputActor::new(binding.clone(), FakePane::default());
    assert_eq!(
        actor
            .step(&mut inputs, Some(&idle), Instant::now())
            .await
            .unwrap(),
        Step::Moved(1, RowState::AwaitTurn)
    );
    assert_eq!(actor.pane().entered.len(), 1);
    let entered_at = actor.entered_at();
    let submitted = actor.pane().entered[0].clone();
    append(
        &path,
        &[
            json!({"type":"user","sessionId":SESSION,"uuid":"own-native-turn","message":{"role":"user","content":submitted}}),
            body("offered-old-body", "existing offered body"),
        ],
    );
    let running = facts.poll(u64::MAX).unwrap();
    let open = TurnState::Open {
        native_turn_id: Some("own-native-turn".into()),
    };
    assert_eq!(running.state, open);
    assert_eq!(
        actor
            .step(&mut inputs, Some(&running), Instant::now())
            .await
            .unwrap(),
        Step::Moved(1, RowState::Running)
    );
    let before = inputs.rows().unwrap().row(1).unwrap().clone();
    assert_eq!(before.state, RowState::Running);
    let accepted = before.attempt.as_ref().unwrap();
    assert!(
        accepted.record_end.is_some(),
        "actual offered frame has its own native witness"
    );
    assert_eq!(accepted.native_turn_id.as_deref(), Some("own-native-turn"));
    assert_eq!(actor.entered_at(), entered_at);

    append(
        &path,
        &[
            json!({"type":"system","subtype":"compact_boundary","sessionId":SESSION,"uuid":"compact-boundary","parentUuid":null,"compactMetadata":{"trigger":"auto"}}),
            json!({"type":"user","sessionId":SESSION,"parentUuid":"compact-boundary","uuid":"compact-summary","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"message":{"role":"user","content":"synthetic compact summary"}}),
            body("offered-old-body", "existing offered body"),
            body("offered-new-body", "new offered body"),
        ],
    );
    let compact_fact = facts.poll(u64::MAX).unwrap();
    let compact_step = actor
        .step(&mut inputs, Some(&compact_fact), Instant::now())
        .await
        .unwrap();
    let after = inputs.rows().unwrap().row(1).unwrap().clone();
    assert_eq!(after.state, RowState::Running);
    assert_eq!(
        after.attempt, before.attempt,
        "actual offer evidence survives compact"
    );
    assert_eq!(
        actor.entered_at(),
        entered_at,
        "runtime paste anchor survives compact"
    );
    assert_eq!(
        inputs.rows().unwrap().row(2).unwrap().state,
        RowState::Received
    );
    assert_eq!(
        actor.pane().entered.len(),
        1,
        "compact cannot Enter the second input"
    );
    assert_eq!(compact_step, Step::Wait("turn_open"));
    assert_eq!(compact_fact.binding, binding);
    assert_eq!(compact_fact.state, open);
    println!(
        "PROBE runtime_attempt end actual_offer_enters=1 native_user_witness=1 runtime_anchor_unchanged=true compact_enters=0"
    );
}

#[tokio::test(start_paused = true)]
async fn g2_gate_same_source_auto_compact_preserves_input_attempt_and_output_identity() {
    // Auto record shape: native Claude trace 2026-09-05, 62cf3723:276–277, copied from
    // tui_prompt_relay/tests/synthetic_bridge_handoff_pg_tests.rs; version unknown, IDs/bodies synthetic.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    let initial = format!("{}\n", json!({"type":"system","subtype":"turn_duration"}));
    fs::write(&path, &initial).unwrap();
    let source = source_id_for(SESSION, &path).unwrap();
    let binding = SourceBinding {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        source: source.clone(),
    };
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
        .unwrap()
        .unwrap();
    let era = store
        .begin_era(&[CHANNEL], Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![InitSource {
                    source_id: source.clone(),
                    delivery_start: initial.len() as u64,
                    prefix_hash: hex::encode(Sha256::digest(initial.as_bytes())),
                }],
                initial_anchor: 100,
                build_digest: "compact-fixture".into(),
                at: Utc::now(),
            })
        })
        .unwrap();
    let open_o = || store.open_channel(&era, CHANNEL).unwrap().unwrap();
    let (output_writer, port) = writer(open_o());
    let bindings = Arc::new(Bindings::new());
    bindings.bind(1, &source, BindingCause::Startup);
    let (stop, stopped) = watch::channel(false);
    let resumed = watch::channel(false).0;
    let output = tokio::spawn(run_channel(
        output_writer,
        ShadowProvider::Claude,
        bindings.clone(),
        stopped,
        resumed,
    ));

    let mut facts = InputFacts::open(binding.clone()).unwrap();
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
    let mut shadow_capture = SourceCapture::open(source.clone(), initial.len() as u64).unwrap();
    let mut shadow = TranscriptDerive::default();
    shadow.attach(&source, initial.len() as u64, Utc::now());
    shadow.window_open(Utc::now());
    let frame = token::render(TOKEN, "keep working through automatic compaction");
    let input_runtime = fs::canonicalize(root.path()).unwrap().join("input");
    let mut inputs = Ledger::open(&input_runtime, CHANNEL).unwrap();
    for (id, text) in [(1, frame.as_str()), (2, "next input")] {
        inputs
            .append_entry(
                &Entry::Received {
                    key: id,
                    input: json!({"text":text}),
                },
                &[],
            )
            .unwrap();
    }
    let anchor = initial.len() as u64;
    let profile = token::profile(ShadowProvider::Claude);
    let meta = AttemptMeta {
        generation: 1,
        token: TOKEN.into(),
        frame_digest: token::digest(profile, &frame).unwrap(),
        frame_profile: Some(profile.into()),
        execution_nonce: NONCE.into(),
        source: source.clone(),
        anchor,
        effect: Effect::Intent,
        incarnation: None,
        queue_end: None,
    };
    let evidence = AttemptEvidence {
        binding: binding.clone(),
        execution_nonce: NONCE.into(),
        eof: anchor,
        rendered_prompt: frame.clone(),
        source_ids: vec![1],
        record_end: None,
        native_turn_id: None,
    };
    // Durable submission fixture uses the production ledger API; pane/Discord/binding ports are fakes.
    inputs
        .append_tracked(
            1,
            RowState::Injecting,
            Some(evidence),
            &Tracking {
                attempt: Some(meta),
                ..Tracking::default()
            },
        )
        .unwrap();
    inputs
        .append_entry(
            &Entry::Transition {
                key: 1,
                state: RowState::AwaitTurn,
                attempt: None,
            },
            &[],
        )
        .unwrap();
    let mut actor = InputActor::new(binding.clone(), FakePane::default());
    append(
        &path,
        &[
            json!({"type":"user","sessionId":SESSION,"uuid":"own-native-turn","message":{"role":"user","content":frame}}),
            body("old-body", "existing body"),
        ],
    );
    let before_fact = facts.poll(u64::MAX).unwrap();
    let open = TurnState::Open {
        native_turn_id: Some("own-native-turn".into()),
    };
    assert_eq!(before_fact.state, open);
    assert_eq!(
        actor
            .step(&mut inputs, Some(&before_fact), Instant::now())
            .await
            .unwrap(),
        Step::Wait("turn_open")
    );
    let before = inputs.rows().unwrap().row(1).unwrap().clone();
    assert_eq!(before.state, RowState::Running);
    assert_eq!(before.attempts.len(), 1);
    assert_eq!(before.witnesses.len(), 1);
    assert_eq!(before.witnesses[0].witness.kind, WitnessKind::User);
    assert_eq!(
        before.witnesses[0].witness.turn_ref.as_deref(),
        Some("own-native-turn")
    );
    assert_eq!(
        sealed(&shadow.derive(&binding, &batch(&mut shadow_capture))),
        [key("old-body")]
    );
    tokio::time::sleep(POLL_INTERVAL * 3).await;
    assert_eq!(port.posts(), ["existing body"]);
    let old_piece = open_o()
        .ledger()
        .latest_piece(&key("old-body"), 0)
        .unwrap()
        .1
        .clone();

    append(
        &path,
        &[
            json!({"type":"system","subtype":"compact_boundary","sessionId":SESSION,"uuid":"compact-boundary","parentUuid":null,"compactMetadata":{"trigger":"auto"}}),
            json!({"type":"user","sessionId":SESSION,"parentUuid":"compact-boundary","uuid":"compact-summary","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"message":{"role":"user","content":"synthetic compact summary"}}),
            body("old-body", "existing body"),
            body("new-body", "new body"),
        ],
    );
    bindings.bind(2, &source, BindingCause::Compact);
    let after_fact = facts.poll(u64::MAX).unwrap();
    let compact_step = actor
        .step(&mut inputs, Some(&after_fact), Instant::now())
        .await
        .unwrap();
    let after = inputs.rows().unwrap().row(1).unwrap().clone();
    assert_eq!(
        after.state,
        RowState::Running,
        "compact alone cannot finish the own input"
    );
    assert_eq!(
        after.attempt, before.attempt,
        "submission evidence survives compact"
    );
    assert_eq!(
        after.attempts, before.attempts,
        "the same attempt generation survives compact"
    );
    assert_eq!(
        after.witnesses, before.witnesses,
        "the own native witness survives compact"
    );
    assert_eq!(
        inputs.rows().unwrap().row(2).unwrap().state,
        RowState::Received
    );
    assert!(
        actor.pane().entered.is_empty(),
        "compact cannot Enter the next input"
    );
    assert_eq!(compact_step, Step::Wait("turn_open"));
    assert_eq!(
        after_fact.binding, before_fact.binding,
        "auto compact retains the exact source"
    );
    assert_eq!(after_fact.state, open, "compact is no Idle/turn completion");
    assert!(facts.events().iter().all(|event| !matches!(
        event,
        crate::services::tui_o::writer::input_facts::Ordered::Closed { .. }
    )));
    let compact_batch = batch(&mut shadow_capture);
    println!(
        "FIXTURE auto_compact provider=Claude version=unknown source_unchanged=true parent=own-native-turn native_units=old-body:0,new-body:0 sha256={}",
        hex::encode(Sha256::digest(fs::read(&path).unwrap()))
    );
    let compact_output = shadow.derive(&binding, &compact_batch);
    assert_eq!(
        sealed(&compact_output),
        [key("new-body")],
        "only the new body seals after compact"
    );
    assert!(
        compact_output
            .iter()
            .all(|item| !matches!(item, DeriveOutput::TurnClosed(_)))
    );
    tokio::time::sleep(POLL_INTERVAL * 3).await;
    assert_eq!(
        port.posts(),
        ["existing body", "new body"],
        "old reposts=0, new posts=1"
    );
    let persisted = open_o();
    assert_eq!(
        persisted
            .ledger()
            .latest_piece(&key("old-body"), 0)
            .unwrap()
            .1,
        &old_piece,
        "old UnitKey/body/delivery stays immutable"
    );
    assert_eq!(persisted.binding_checkpoint().unwrap(), Some(2));
    assert_eq!(persisted.cursors().count(), 1);
    assert!(!persisted.cursor(&source).unwrap().retired);

    // A source alias is only an identity control; the input binding and compact source stay unchanged.
    let mut alias_binding = binding.clone();
    alias_binding.source.session_id = "alias-session".into();
    let mut alias_batch = compact_batch;
    alias_batch.source = alias_binding.source.clone();
    let alias_output = shadow.derive(&alias_binding, &alias_batch);
    assert!(
        sealed(&alias_output).is_empty(),
        "source session ID cannot fork existing native UnitKeys"
    );
    append(&path, &[json!({"type":"system","subtype":"turn_duration"})]);
    let done_fact = facts.poll(u64::MAX).unwrap();
    assert_eq!(done_fact.state, TurnState::Idle);
    assert_eq!(
        actor
            .step(&mut inputs, Some(&done_fact), Instant::now())
            .await
            .unwrap(),
        Step::Moved(2, RowState::AwaitTurn)
    );
    assert_eq!(
        inputs.rows().unwrap().row(1).unwrap().state,
        RowState::Done(DoneReason::Completed)
    );
    assert_eq!(
        actor.pane().entered.len(),
        1,
        "only the matched real closer permits the next Enter"
    );
    println!(
        "PROBE InputActor::step/InputFacts::poll attempt_generation=1 own_witness=1 compact_enters=0 matched_closer_enters=1"
    );
    println!(
        "PROBE run_channel/ChannelWriter::deliver/TranscriptDerive::derive old_posts=1 new_posts=1 old_reposts=0 alias_new_units=0"
    );
    stop.send(true).unwrap();
    assert!(
        output.await.unwrap().is_none(),
        "the production O actor must finish without halting"
    );
    offered_input_keeps_runtime_attempt_through_compact().await;
}
