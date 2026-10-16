//! The verified collector through the real `Sources` resume, follow and capture paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::*;
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::codex_provenance::{CODEX_PROVENANCE_FILE, CodexProvenance};
use crate::services::tui_o::store::rotation::{BOUNDARY_FILE, Boundary};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::{InitSource, Initialized, OStore, STORE_DIR_NAME, StoreConfig};
use crate::services::tui_o::writer::actor::{POLL_INTERVAL, run_channel};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::deliver::ChannelWriter;
use crate::services::tui_o::writer::pieces::UnitDeriver;
use crate::services::tui_o::writer::rotation::Sources;
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, DiscordPort, PostOutcome, SeenMessage, WriterAlarm,
};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeDeny, CodexEpisodeSpan, ExecutionProofRef, TurnEpisodeRef,
};

const CHANNEL: u64 = 584_521;

#[derive(Default)]
pub(crate) struct Port {
    posts: Mutex<Vec<String>>,
    /// Runs inside each POST's first poll, under the gate and any record lock.
    on_post: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Port {
    pub(crate) fn posts(&self) -> Vec<String> {
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
        async move { PostOutcome::Created(message) }
    }

    /// A readable channel where only another author spoke after `after`.
    async fn history_after(&self, _channel: u64, after: u64) -> Result<Vec<SeenMessage>, String> {
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

pub(crate) struct Lease;

impl DeliveryLease for Lease {
    type Held = ();

    fn try_acquire(&self, _channel: u64, _serial: u64) -> Option<()> {
        Some(())
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
pub(crate) struct Bindings {
    channel: u64,
    events: Mutex<Vec<BindingEvent>>,
    notice: watch::Sender<u64>,
}

impl Bindings {
    pub(crate) fn new() -> Arc<Self> {
        Self::on(CHANNEL)
    }

    pub(crate) fn on(channel: u64) -> Arc<Self> {
        let (events, notice) = (Mutex::new(Vec::new()), watch::channel(0).0);
        Arc::new(Self {
            channel,
            events,
            notice,
        })
    }

    fn bind(&self, old: Option<&SourceId>, new: &SourceId, cause: BindingCause) {
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

pub(crate) fn owed_writer(store: ChannelStore) -> (Writer, Arc<Port>) {
    let (gate, port) = (
        Arc::new(OwnershipGate::default()),
        Arc::new(Port::default()),
    );
    gate.acquired();
    let writer = ChannelWriter::new(store, gate, Arc::clone(&port), Lease, Alarms::default());
    (writer, port)
}

/// One actor's Codex sources and deriver, owing into the verified collector.
pub(crate) struct Collector {
    sources: Sources<Bindings>,
    deriver: UnitDeriver,
    pub log: OwedLog,
}

impl Collector {
    pub(crate) fn resumed(
        writer: &mut Writer,
        bindings: &Arc<Bindings>,
    ) -> Result<Self, WriterAlarm> {
        let (channel, codex) = (writer.channel(), ShadowProvider::Codex);
        let mut collector = Self {
            sources: Sources::new(channel, codex, Arc::clone(bindings)),
            deriver: UnitDeriver::new(channel, codex),
            log: OwedLog::new(channel, codex),
        };
        let (deriver, log) = (&mut collector.deriver, &mut collector.log);
        collector.sources.resume(writer, deriver, log)?;
        Ok(collector)
    }

    /// One actor read pass: binds past the checkpoint, then capture.
    fn read(&mut self, writer: &mut Writer) -> Result<(), WriterAlarm> {
        self.sources.follow(writer)?;
        (self.sources).capture(writer, &mut self.deriver, &mut self.log)
    }
}

/// A step the contract requires to complete without a writer halt.
pub(crate) fn kept<T>(result: Result<T, WriterAlarm>, step: &str) -> T {
    result.unwrap_or_else(|alarm| panic!("{step} halted the writer: {alarm:?}"))
}

fn header(session: &str) -> String {
    let meta = serde_json::json!({"type": "session_meta", "payload": {
        "id": session, "cwd": "/native", "source": "cli", "originator": "codex-tui"}});
    format!("{meta}\n")
}

fn event(kind: &str, id: &str) -> String {
    let event = serde_json::json!({"type": "event_msg", "payload": {"type": kind, "turn_id": id}});
    format!("{event}\n")
}

fn body(id: &str, text: &str) -> String {
    let body = serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant", "id": id,
        "content": [{"type": "output_text", "text": text}]}});
    format!("{body}\n")
}

fn append(path: &Path, lines: &[String]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(lines.concat().as_bytes()).unwrap();
}

fn transcript(beside: &Path, name: &str, session: &str, lines: &[String]) -> SourceId {
    let path = beside.with_file_name(name);
    std::fs::write(
        &path,
        [header(session)]
            .iter()
            .chain(lines)
            .cloned()
            .collect::<String>(),
    )
    .unwrap();
    source_id_for(session, &path).unwrap()
}

/// A Codex channel switched over `a.jsonl` after its session header, and no sidecar.
fn switched() -> (tempfile::TempDir, OStore, SourceId) {
    switched_on(CHANNEL)
}

fn switched_on(channel: u64) -> (tempfile::TempDir, OStore, SourceId) {
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
            channel,
            sources,
            initial_anchor: 100,
            build_digest,
            at,
        })
    };
    store
        .begin_era(&[channel], Utc::now(), initialized)
        .unwrap();
    (runtime, store, source)
}

fn opened(store: &OStore) -> ChannelStore {
    let era = store.read_era().unwrap().unwrap();
    let channel = era.initial_channels[0];
    store.open_channel(&era, channel).unwrap().unwrap()
}

fn channel_file(runtime: &tempfile::TempDir, file: &str) -> PathBuf {
    channel_file_on(runtime, CHANNEL, file)
}

fn channel_file_on(runtime: &tempfile::TempDir, channel: u64, file: &str) -> PathBuf {
    let dir = runtime.path().join(STORE_DIR_NAME);
    dir.join(channel.to_string()).join(file)
}

fn span(source: &SourceId, start: u64, end: Option<u64>) -> CodexEpisodeSpan {
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

fn present(store: &ChannelStore) -> CodexProvenance {
    match store.codex_provenance() {
        Sidecar::Present(read) => read.raw().clone(),
        other => panic!("sidecar not present: {other:?}"),
    }
}

fn payload(work: &OwedWork) -> &str {
    match &work.derived {
        Derived::Piece(piece) => &piece.payload,
        other => panic!("not a piece: {other:?}"),
    }
}

#[test]
fn rotation_writes_between_span_open_close_and_deny_keep_the_sidecar_and_never_halt() {
    let (runtime, store, a) = switched();
    let mut stale = span(&a, 0, Some(1));
    (stale.episode.user_message_id, stale.episode.native_turn_id) = (99, "n0".into());
    stale.episode.turn_nonce = "stale".into();
    let stale = serde_json::json!({"links": {}, "successors": {},
        "codex_spans": [stale], "codex_denies": []});
    let boundary = channel_file(&runtime, BOUNDARY_FILE);
    std::fs::write(&boundary, serde_json::to_vec(&stale).unwrap()).unwrap();
    let (mut writer, _) = owed_writer(opened(&store));
    let bindings = Bindings::new();
    bindings.bind(None, &a, BindingCause::Startup);
    let mut collector = kept(Collector::resumed(&mut writer, &bindings), "resume");
    writer.store().init_codex_provenance().unwrap();
    let start = std::fs::metadata(&a.path).unwrap().len();
    append(&a.path, &[event("task_started", "n1"), body("b1", "first")]);
    kept(collector.read(&mut writer), "capture");
    let open = span(&a, start, None);
    writer.store().persist_codex_span(open.clone()).unwrap();
    let b = transcript(&a.path, "b.jsonl", "s2", &[body("b2", "second")]);
    bindings.bind(Some(&a), &b, BindingCause::Clear);
    kept(collector.read(&mut writer), "bind between open and close");
    assert_eq!(present(writer.store()).spans, std::slice::from_ref(&open));
    append(&a.path, &[event("task_complete", "n1")]);
    kept(collector.read(&mut writer), "capture terminal");
    let mut closed = open;
    closed.end = Some(std::fs::metadata(&a.path).unwrap().len());
    writer.store().persist_codex_span(closed.clone()).unwrap();
    let c = transcript(&a.path, "c.jsonl", "s3", &[]);
    bindings.bind(Some(&b), &c, BindingCause::Clear);
    kept(collector.read(&mut writer), "bind between close and deny");
    let deny = CodexEpisodeDeny {
        execution: closed.execution.clone(),
        episode: closed.episode.clone(),
    };
    writer.store().persist_codex_deny(deny.clone()).unwrap();
    kept(collector.read(&mut writer), "capture after deny");
    drop(writer);
    let (mut writer, _) = owed_writer(opened(&store));
    let mut restarted = kept(Collector::resumed(&mut writer, &bindings), "restart");
    kept(restarted.read(&mut writer), "restarted capture");
    let sidecar = present(writer.store());
    assert_eq!(
        (sidecar.spans, sidecar.denies),
        (vec![closed.clone()], vec![deny])
    );
    let rotation = writer.store().rotation().unwrap();
    let links: Vec<_> = rotation
        .links
        .values()
        .map(|l| (&l.source, &l.boundary))
        .collect();
    let owed = Boundary::Owed { from: 0 };
    assert_eq!(links.len(), 2);
    assert!(links.contains(&(&b, &owed)) && links.contains(&(&c, &owed)));
    assert_eq!(rotation.successors[&source_key(&a)].source, b);
    assert_eq!(rotation.successors[&source_key(&b)].source, c);
    let rewritten = std::fs::read_to_string(&boundary).unwrap();
    assert!(
        !rewritten.contains("codex_spans"),
        "old additive fields are not carried"
    );
    let work = &mut collector.log.work;
    assert_eq!(
        work.iter().map(payload).collect::<Vec<_>>(),
        ["first", "second"]
    );
    reevaluate(writer.store(), work).unwrap();
    assert_eq!(work[0].origin.as_ref().map(|o| &o.span), Some(&closed));
    assert_eq!((&work[1].source, &work[1].origin), (&b, &None));
}

/// A unit read again from a second source derives nothing there, yet both sources stay
/// associated with its key.
#[test]
fn a_repeated_unit_keeps_every_source_it_was_owed_from() {
    let (_runtime, store, a) = switched();
    let lines = [body("shared", "same reply")];
    append(&a.path, &lines);
    let b = transcript(&a.path, "b.jsonl", "s2", &lines);
    let mut channel = opened(&store);
    channel.attach_source(&b).unwrap();
    for source in [&a, &b] {
        let through = channel.cursor(source).unwrap().captured_through;
        let mut capture = crate::services::tui_o::shadow::capture::SourceCapture::reopen(
            source.clone(),
            through,
            &channel.cursor(source).unwrap().prefix_hash,
        )
        .unwrap();
        let crate::services::tui_o::shadow::CaptureOutcome::Batch(batch) =
            crate::services::tui_o::shadow::CaptureSource::poll(&mut capture, 1 << 20)
        else {
            panic!("capture failed")
        };
        channel
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
    }
    let (mut writer, _) = owed_writer(channel);
    let collector = kept(Collector::resumed(&mut writer, &Bindings::new()), "resume");
    assert_eq!(
        collector.log.work.len(),
        1,
        "the repeat derives no second piece"
    );
    let [(key, sources)] = collector.log.associations.iter().collect::<Vec<_>>()[..] else {
        panic!("one unit key expected")
    };
    assert_eq!(key.native_key, "shared");
    let mut named: Vec<_> = sources
        .iter()
        .map(|(source, _)| source_key(source))
        .collect();
    named.sort();
    let mut expected = vec![source_key(&a), source_key(&b)];
    expected.sort();
    assert_eq!(named, expected, "a derive-0 repeat lost its source");
}

/// The Claude-only empty-session rule never maps a Codex bind onto a stored cursor.
#[test]
fn a_codex_bind_with_an_empty_session_does_not_take_the_stored_cursor() {
    let (_runtime, store, a) = switched();
    let (mut writer, _) = owed_writer(opened(&store));
    let bindings = Bindings::new();
    bindings.bind(None, &a, BindingCause::Startup);
    let mut collector = kept(Collector::resumed(&mut writer, &bindings), "resume");
    kept(collector.read(&mut writer), "seed");
    let mut empty = a.clone();
    empty.session_id.clear();
    bindings.bind(Some(&a), &empty, BindingCause::Clear);
    kept(collector.read(&mut writer), "empty-session bind");
    assert!(
        writer.store().cursor(&empty).is_some(),
        "a Codex empty session was taken for the stored source"
    );
    assert!(writer.store().cursor(&a).is_some());
}

/// Production delivery neither reads, creates nor changes the sidecar: the same records post
/// the same way whether it is absent, valid with a closed span over them, or damaged.
#[tokio::test(start_paused = true)]
async fn the_running_actor_posts_the_same_and_leaves_the_sidecar_alone() {
    let mut seen = Vec::new();
    for sidecar in [None, Some(true), Some(false)] {
        let (runtime, store, a) = switched();
        let file = channel_file(&runtime, CODEX_PROVENANCE_FILE);
        let start = std::fs::metadata(&a.path).unwrap().len();
        let lines = [event("task_started", "n1"), body("b1", "first")];
        append(&a.path, &[lines.concat(), event("task_complete", "n1")]);
        let end = std::fs::metadata(&a.path).unwrap().len();
        let mut channel = opened(&store);
        match sidecar {
            Some(true) => {
                channel.init_codex_provenance().unwrap();
                channel
                    .persist_codex_span(span(&a, start, Some(end)))
                    .unwrap();
            }
            Some(false) => std::fs::write(&file, b"{damaged").unwrap(),
            None => {}
        }
        let before = std::fs::read(&file).ok();
        let (writer, port) = owed_writer(channel);
        let bindings = Bindings::new();
        bindings.bind(None, &a, BindingCause::Startup);
        let (stop, stopped) = watch::channel(false);
        let resumed = watch::channel(false).0;
        let codex = ShadowProvider::Codex;
        use crate::services::tui_o::store::codex_provenance::SIDECAR_IO;
        SIDECAR_IO.with(|io| io.set((0, 0)));
        let task = tokio::spawn(run_channel(writer, codex, bindings, stopped, resumed));
        tokio::time::sleep(POLL_INTERVAL * 4).await;
        stop.send(true).unwrap();
        assert_eq!(task.await.unwrap(), None, "the actor stopped: {sidecar:?}");
        let (reads, writes) = SIDECAR_IO.with(std::cell::Cell::get);
        assert_eq!(
            (reads, writes),
            (0, 0),
            "production sidecar I/O: {sidecar:?}"
        );
        assert_eq!(
            std::fs::read(&file).ok(),
            before,
            "sidecar touched: {sidecar:?}"
        );
        seen.push(port.posts());
    }
    assert_eq!(seen[0], ["first"]);
    assert!(seen.iter().all(|posts| *posts == seen[0]), "{seen:?}");
}

#[path = "verified_delivery_tests.rs"]
mod verified;
