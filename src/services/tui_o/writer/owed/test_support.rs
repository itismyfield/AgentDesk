//! In-memory writer ports and temporary stores for Codex owed delivery tests.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::provider::CancelToken;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::deliver::ChannelWriter;
use crate::services::tui_o::writer::pieces::{Derived, OwedWork};
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, DiscordPort, PostOutcome, SeenMessage, WriterAlarm,
};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::{
    forget_channel_for_tests, issue_for_tests, set_ready_for_tests,
};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeSpan, ExecutionProofRef, TurnEpisodeRef,
};

pub(super) const CHANNEL: u64 = 584_521;

#[derive(Default)]
pub struct Port {
    pub(super) history_override: Mutex<Option<Vec<SeenMessage>>>,
    pub(super) history_reads: std::sync::atomic::AtomicUsize,
    pub(super) posts: Mutex<Vec<String>>,
    /// Runs inside each POST's first poll, under the gate and any record lock.
    pub(super) on_post: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Each POST answers only after this long.
    pub delay: Mutex<Option<std::time::Duration>>,
}

impl Port {
    pub fn posts(&self) -> Vec<String> {
        self.posts.lock().unwrap().clone()
    }
}

impl DiscordPort for Port {
    fn bot_id(&self) -> u64 {
        42
    }

    fn post(
        &self,
        _channel: u64,
        content: String,
    ) -> impl std::future::Future<Output = PostOutcome> + Send + 'static {
        let mut posts = self.posts.lock().unwrap();
        posts.push(content.clone());
        let (id, author_id) = (1000 + posts.len() as u64, 42);
        drop(posts);
        if let Some(hook) = self.on_post.lock().unwrap().as_ref() {
            hook();
        }
        let message = SeenMessage {
            id,
            author_id,
            content,
        };
        let delay = *self.delay.lock().unwrap();
        async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            PostOutcome::Created(message)
        }
    }

    /// A readable channel where only another author spoke after `after`.
    async fn history_after(&self, _channel: u64, after: u64) -> Result<Vec<SeenMessage>, String> {
        self.history_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(history) = &*self.history_override.lock().unwrap() {
            return Ok(history
                .iter()
                .filter(|message| message.id > after)
                .cloned()
                .collect());
        }
        let (id, author_id, content) = (after + 1, 7, "chatter".to_owned());
        Ok(vec![SeenMessage {
            id,
            author_id,
            content,
        }])
    }

    async fn message(&self, _channel: u64, _id: u64) -> Result<Option<SeenMessage>, String> {
        Ok(None)
    }
}

pub struct Lease;

thread_local! {
    /// While set, every lease on this thread is held elsewhere.
    pub static LEASE_BUSY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl DeliveryLease for Lease {
    type Held = ();

    fn try_acquire(&self, _channel: u64, _serial: u64) -> Option<()> {
        (!LEASE_BUSY.get()).then_some(())
    }
}

#[derive(Clone, Default)]
pub(crate) struct Alarms(Arc<Mutex<Vec<WriterAlarm>>>);

impl AlarmSink for Alarms {
    fn raise(&self, _channel: u64, alarm: WriterAlarm) {
        self.0.lock().unwrap().push(alarm);
    }
}

/// An in-memory Codex binding log whose seq notice moves with each bind.
pub struct Bindings {
    channel: u64,
    events: Mutex<Vec<BindingEvent>>,
    notice: watch::Sender<u64>,
}

impl Bindings {
    pub(crate) fn new() -> Arc<Self> {
        Self::on(CHANNEL)
    }

    pub fn on(channel: u64) -> Arc<Self> {
        let (events, notice) = (Mutex::new(Vec::new()), watch::channel(0).0);
        Arc::new(Self {
            channel,
            events,
            notice,
        })
    }

    pub fn bind(&self, old: Option<&SourceId>, new: &SourceId, cause: BindingCause) {
        let mut events = self.events.lock().unwrap();
        let seq = events.len() as u64 + 1;
        let evidence = BindingEvidence {
            hook_event: HookEventKind::SessionStart.as_str().into(),
            received_at: Utc::now(),
            reclaims: false,
        };
        let record = BindingRecord::Bound {
            old: old.cloned(),
            new: BindingTarget::Source(new.clone()),
            cause,
            parent_hint: old.cloned(),
            evidence,
        };
        events.push(BindingEvent {
            seq,
            channel_id: self.channel,
            provider: ShadowProvider::Codex,
            tmux_session: "pane".into(),
            execution_nonce: "nonce".into(),
            record,
            committed_at: Utc::now(),
        });
        self.notice.send_replace(seq);
    }
}

impl BindingEvents for Bindings {
    fn binding_events_since(&self, _channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        let events = self.events.lock().unwrap();
        Ok(events.iter().filter(|e| e.seq > after).cloned().collect())
    }

    fn subscribe(&self, _channel: u64) -> watch::Receiver<u64> {
        self.notice.subscribe()
    }
}

pub(crate) type Writer = ChannelWriter<Port, Lease, Alarms>;

pub fn owed_writer(store: ChannelStore) -> (Writer, Arc<Port>) {
    let (gate, port) = (
        Arc::new(OwnershipGate::default()),
        Arc::new(Port::default()),
    );
    gate.acquired();
    let writer = ChannelWriter::new(store, gate, Arc::clone(&port), Lease, Alarms::default());
    (writer, port)
}

pub(super) fn header(session: &str) -> String {
    let meta = serde_json::json!({"type": "session_meta", "payload": {
        "id": session, "cwd": "/native", "source": "cli", "originator": "codex-tui"}});
    format!("{meta}\n")
}

pub(super) fn event(kind: &str, id: &str) -> String {
    let event = serde_json::json!({"type": "event_msg", "payload": {"type": kind, "turn_id": id}});
    format!("{event}\n")
}

pub(super) fn body(id: &str, text: &str) -> String {
    let body = serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant", "id": id,
        "content": [{"type": "output_text", "text": text}]}});
    format!("{body}\n")
}

pub(super) fn append(path: &Path, lines: &[String]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(lines.concat().as_bytes()).unwrap();
}

pub fn switched_on(channel: u64) -> (tempfile::TempDir, OStore, SourceId) {
    let runtime = tempfile::tempdir().unwrap();
    let head = header("s1");
    let path = runtime.path().join("a.jsonl");
    std::fs::write(&path, &head).unwrap();
    let source = source_id_for("s1", &path).unwrap();
    let config = StoreConfig { enabled: true };
    let store = OStore::open_if_enabled(&config, runtime.path())
        .unwrap()
        .unwrap();
    let init = InitSource {
        source_id: source.clone(),
        delivery_start: head.len() as u64,
        prefix_hash: hex::encode(Sha256::digest(head.as_bytes())),
    };
    let initialized = |channel| {
        let (sources, build_digest, at) = (vec![init.clone()], "b".to_string(), Utc::now());
        Ok(Initialized {
            policy_parent: None,
            channel,
            sources,
            initial_anchor: 100,
            build_digest,
            at,
        })
    };
    store
        .begin_fixture_era(&[channel], Utc::now(), initialized)
        .unwrap();
    (runtime, store, source)
}

pub fn opened(store: &OStore) -> ChannelStore {
    let era = store.read_era().unwrap().unwrap();
    let channel = era.initial_channels[0];
    store.open_channel(&era, channel).unwrap().unwrap()
}

pub(super) fn span(source: &SourceId, start: u64, end: Option<u64>) -> CodexEpisodeSpan {
    let execution = ExecutionProofRef {
        owner_runtime_root: "/runtime/owner".into(),
        tmux_session: "pane".into(),
        execution_nonce: "execution".into(),
        proof_seq: 4,
        source: source.clone(),
    };
    let episode = TurnEpisodeRef {
        channel_id: CHANNEL,
        user_message_id: 10,
        request_owner_id: 8,
        turn_nonce: "turn".into(),
        native_turn_id: "n1".into(),
    };
    CodexEpisodeSpan {
        execution,
        episode,
        delivery_channel_id: CHANNEL,
        offset_authority_channel_id: CHANNEL,
        generation_mtime_ns: 1,
        start,
        end,
    }
}

pub(super) fn payload(work: &OwedWork) -> &str {
    match &work.derived {
        Derived::Piece(piece) => &piece.payload,
        other => panic!("not a piece: {other:?}"),
    }
}

/// Opens the readiness gate for one channel on this thread and forgets its records on drop.
pub struct Gate(u64);
impl Gate {
    pub fn open(channel: u64, ready: bool) -> Self {
        set_ready_for_tests(channel, ready);
        Self(channel)
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        set_ready_for_tests(self.0, false);
        forget_channel_for_tests(self.0);
    }
}

pub(super) fn len(source: &SourceId) -> u64 {
    std::fs::metadata(&source.path).unwrap().len()
}

pub(super) fn episode_span(
    channel: u64,
    source: &SourceId,
    message: u64,
    range: (u64, u64),
) -> CodexEpisodeSpan {
    let mut span = span(source, range.0, Some(range.1));
    span.episode.channel_id = channel;
    (span.delivery_channel_id, span.offset_authority_channel_id) = (channel, channel);
    span.episode.user_message_id = message;
    span.episode.turn_nonce = format!("turn-{message}");
    span.episode.native_turn_id = format!("n{message}");
    span
}

/// Appends one closed native turn, stores its span, and issues its permit when `permit`.
pub fn turn(
    store: &mut ChannelStore,
    source: &SourceId,
    message: u64,
    bodies: &[&str],
    permit: bool,
) -> (CodexEpisodeSpan, Option<Arc<CancelToken>>) {
    let native = format!("n{message}");
    let start = len(source);
    let mut lines = vec![event("task_started", &native)];
    lines.extend(bodies.iter().map(|text| body(text, text)));
    lines.push(event("task_complete", &native));
    append(&source.path, &lines);
    let channel = store.init().channel;
    let span = episode_span(channel, source, message, (start, len(source)));
    store.persist_codex_span(span.clone()).unwrap();
    let token = permit.then(|| {
        let nonce = Some(span.episode.turn_nonce.clone());
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(nonce));
        let (execution, episode) = (span.execution.clone(), span.episode.clone());
        issue_for_tests(execution, episode, &token).unwrap();
        token
    });
    (span, token)
}
