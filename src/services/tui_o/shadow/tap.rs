//! Gateway tap and the observe loop it feeds: capture -> derive -> diff, persisted only via the sink.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::binding_reader::{BindingReader, LiveBindingLookup, ShadowTarget};
use super::capture::SourceCapture;
use super::derive::TranscriptDerive;
use super::diff::WindowDiff;
use super::identity::{RecordFact, classify, row_key};
use super::metrics::MetricsSnapshot;
use super::root::{ShadowRoot, ShadowStore, StoredRecord};
use super::seal::{TurnEvent, TurnTracker};
use super::{
    BindingChange, CaptureBatch, CaptureOutcome, CaptureSource, DISK_CAP_BYTES, DeriveOutput,
    IDENTITY_VERSION, LegacyTapEvent, MAX_READ_BYTES, POLL_INTERVAL, SCHEMA_VERSION, ShadowConfig,
    ShadowDerive, ShadowDiff, ShadowProvider, ShadowRecord, ShadowSink, SourceBinding, SourceId,
    TAP_CAPACITY, WindowStartSource,
};
use crate::services::agent_protocol::RuntimeHandoffKind;

/// The `tui_o:` config section; absent means every output-track feature is off.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiOConfig {
    pub shadow: ShadowConfig,
}

/// Bounded copy of bot-authored message events for allowlisted channels.
pub struct GatewayTap {
    tx: mpsc::Sender<LegacyTapEvent>,
    channels: HashSet<u64>,
    dropped: AtomicU64,
}

static TAP: OnceLock<GatewayTap> = OnceLock::new();

impl GatewayTap {
    pub fn new(channels: &[u64], capacity: usize) -> (Self, mpsc::Receiver<LegacyTapEvent>) {
        let (tx, rx) = mpsc::channel(capacity);
        let channels = channels.iter().copied().collect();
        (
            Self {
                tx,
                channels,
                dropped: AtomicU64::new(0),
            },
            rx,
        )
    }

    pub fn watches(&self, channel_id: u64) -> bool {
        self.channels.contains(&channel_id)
    }

    /// Never waits on the gateway task: a full or closed queue drops the event and counts it.
    pub fn offer(&self, event: LegacyTapEvent) {
        if self.tx.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

/// Called by the gateway event handler before dispatch; a no-op while the shadow is off.
pub fn observe(ctx: &serenity::Context, event: &serenity::FullEvent) {
    let Some(tap) = TAP.get() else { return };
    let channel_id = match event {
        serenity::FullEvent::Message { new_message } => new_message.channel_id,
        serenity::FullEvent::MessageUpdate { event, .. } => event.channel_id,
        serenity::FullEvent::MessageDelete { channel_id, .. } => *channel_id,
        _ => return,
    };
    if tap.watches(channel_id.get()) {
        if let Some(copy) = tap_event(ctx.cache.current_user().id.get(), event) {
            tap.offer(copy);
        }
    }
}

/// Own-bot messages only; authorless updates and deletes pass because the diff ignores unseen ids.
fn tap_event(bot_id: u64, event: &serenity::FullEvent) -> Option<LegacyTapEvent> {
    match event {
        serenity::FullEvent::Message { new_message: m } if m.author.id.get() == bot_id => {
            Some(LegacyTapEvent::Created {
                channel_id: m.channel_id.get(),
                msg_id: m.id.get(),
                at: *m.timestamp,
                content: m.content.clone(),
            })
        }
        serenity::FullEvent::MessageUpdate { event: e, .. }
            if e.author.as_ref().is_none_or(|a| a.id.get() == bot_id) =>
        {
            Some(LegacyTapEvent::Updated {
                channel_id: e.channel_id.get(),
                msg_id: e.id.get(),
                at: e.edited_timestamp.map_or_else(Utc::now, |t| *t),
                content: e.content.clone(),
            })
        }
        serenity::FullEvent::MessageDelete {
            channel_id,
            deleted_message_id,
            ..
        } => Some(LegacyTapEvent::Deleted {
            channel_id: channel_id.get(),
            msg_id: deleted_message_id.get(),
            at: Utc::now(),
        }),
        _ => None,
    }
}

/// Derive-side hooks; one instance serves every source so keys seen on another source stay visible.
pub trait DeriveLink: Send {
    /// Offset to capture from: the running turn's start found by the bounded reverse scan, else `extent`.
    fn capture_start(&mut self, binding: &SourceBinding, extent: u64) -> u64;
    /// Records below `attach_extent` must come out as historical, never live.
    fn attach(&mut self, source: &SourceId, attach_extent: u64, attached_at: DateTime<Utc>);
    fn window_start(&mut self, t0: DateTime<Utc>, source: &SourceId, window_start_extent: u64);
    fn derive(&mut self, binding: &SourceBinding, batch: &CaptureBatch) -> Vec<DeriveOutput>;
}

pub type CaptureOpener =
    Box<dyn FnMut(&SourceBinding, u64) -> io::Result<Box<dyn CaptureSource>> + Send>;

struct Feed {
    binding: SourceBinding,
    capture: Box<dyn CaptureSource>,
}

/// Owns one observe step; every write goes through `sink`.
pub struct Observer {
    sink: Box<dyn ShadowSink>,
    diff: WindowDiff,
    metrics: MetricsSnapshot,
    feeds: HashMap<u64, Feed>,
    open_capture: CaptureOpener,
    link: Box<dyn DeriveLink>,
}

impl Observer {
    pub fn new(
        sink: Box<dyn ShadowSink>,
        open_capture: CaptureOpener,
        link: Box<dyn DeriveLink>,
    ) -> Self {
        let (diff, metrics, feeds) = (
            WindowDiff::default(),
            MetricsSnapshot::default(),
            HashMap::new(),
        );
        Self {
            sink,
            diff,
            metrics,
            feeds,
            open_capture,
            link,
        }
    }

    pub fn metrics(&self) -> &MetricsSnapshot {
        &self.metrics
    }

    fn persist(&mut self, record: ShadowRecord) {
        self.metrics.record(&record);
        if let Err(error) = self.sink.append(&record) {
            tracing::warn!(%error, "o-shadow: append failed");
        }
    }

    pub fn start(&mut self, now: DateTime<Utc>) {
        self.persist(ShadowRecord::Header {
            schema_version: SCHEMA_VERSION,
            identity_version: IDENTITY_VERSION,
            build: env!("CARGO_PKG_VERSION").to_string(),
            started_at: now,
        });
    }

    fn attach(&mut self, binding: SourceBinding, now: DateTime<Utc>) {
        let opened = std::fs::metadata(&binding.source.path).and_then(|meta| {
            let start = self
                .link
                .capture_start(&binding, meta.len())
                .min(meta.len());
            Ok((meta.len(), start, (self.open_capture)(&binding, start)?))
        });
        match opened {
            Ok((attach_extent, capture_start, capture)) => {
                let source = binding.source.clone();
                self.link.attach(&source, attach_extent, now);
                self.persist(ShadowRecord::Attach {
                    source,
                    attach_extent,
                    capture_start,
                    attached_at: now,
                });
                self.feeds
                    .insert(binding.channel_id, Feed { binding, capture });
            }
            Err(error) => {
                tracing::warn!(%error, channel_id = binding.channel_id, "o-shadow: attach failed")
            }
        }
    }

    /// Moves each listed source's live boundary to its size at `t0`.
    pub fn window_start(&mut self, t0: DateTime<Utc>, sources: &[WindowStartSource]) {
        for listed in sources {
            let extent = listed.window_start_extent;
            self.link.window_start(t0, &listed.source, extent);
        }
    }

    /// Binding changes, one capture poll per feed, then tap events and whatever the diff decided.
    pub fn tick(
        &mut self,
        now: DateTime<Utc>,
        changes: Vec<BindingChange>,
        legacy: Vec<LegacyTapEvent>,
        dropped: u64,
    ) {
        for change in changes {
            self.feeds.remove(&change.channel_id);
            if let Some(binding) = change.new.clone() {
                self.attach(binding, now);
            }
            self.persist(ShadowRecord::Binding { change });
        }
        let (mut outputs, mut anomalies, mut lags) = (Vec::new(), Vec::new(), Vec::new());
        for feed in self.feeds.values_mut() {
            match feed.capture.poll(MAX_READ_BYTES) {
                CaptureOutcome::Batch(batch) if !batch.records.is_empty() => {
                    lags.extend(capture_lag_ms(&feed.binding.source.path, now));
                    outputs.extend(self.link.derive(&feed.binding, &batch));
                }
                CaptureOutcome::Batch(_) => {}
                CaptureOutcome::Anomaly(anomaly) => {
                    anomalies.push((feed.binding.channel_id, anomaly))
                }
            }
        }
        lags.into_iter()
            .for_each(|lag| self.metrics.record_capture_lag(lag));
        for (channel_id, anomaly) in anomalies {
            self.feeds.remove(&channel_id);
            self.persist(ShadowRecord::Anomaly { anomaly });
        }
        for output in outputs {
            self.diff.observe_derived(&output, now);
            self.persist(ShadowRecord::Derived { output });
        }
        legacy
            .iter()
            .for_each(|event| self.diff.observe_legacy(event));
        if dropped > 0 {
            self.diff.observe_tap_gap(dropped, now);
            self.persist(ShadowRecord::TapGap { dropped });
        }
        for diff in self.diff.drain_ready(now) {
            self.persist(ShadowRecord::Diff { diff });
        }
        for msg in self.diff.drain_retired() {
            self.persist(ShadowRecord::Legacy { msg });
        }
    }
}

fn capture_lag_ms(path: &Path, now: DateTime<Utc>) -> Option<u64> {
    let modified: DateTime<Utc> = std::fs::metadata(path).ok()?.modified().ok()?.into();
    u64::try_from((now - modified).num_milliseconds()).ok()
}

/// How far back attach looks for the opener of a turn that is still running.
const OPENER_SCAN_BYTES: u64 = 8 * 1024 * 1024;

impl DeriveLink for TranscriptDerive {
    fn capture_start(&mut self, binding: &SourceBinding, extent: u64) -> u64 {
        running_turn_start(binding.provider, &binding.source.path, extent).unwrap_or(extent)
    }
    fn attach(&mut self, source: &SourceId, attach_extent: u64, attached_at: DateTime<Utc>) {
        TranscriptDerive::attach(self, source, attach_extent, attached_at);
    }
    fn window_start(&mut self, t0: DateTime<Utc>, source: &SourceId, window_start_extent: u64) {
        TranscriptDerive::window_start(self, t0, source, window_start_extent);
    }
    fn derive(&mut self, binding: &SourceBinding, batch: &CaptureBatch) -> Vec<DeriveOutput> {
        ShadowDerive::derive(self, binding, batch)
    }
}

/// Start of the turn open at `extent`: its opener row, or the idle row an autonomous turn follows.
fn running_turn_start(provider: ShadowProvider, path: &Path, extent: u64) -> io::Result<u64> {
    let from = extent.saturating_sub(OPENER_SCAN_BYTES);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.take(extent - from).read_to_end(&mut bytes)?;
    let mut offset = from;
    let mut lines: Vec<(u64, &[u8])> = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        // A line cut by the scan window or still being written is not a whole record.
        if line.ends_with(b"\n") && (offset > from || from == 0) {
            lines.push((offset, line));
        }
        offset += line.len() as u64;
    }
    let mut assistant_after = false;
    for (start, line) in lines.into_iter().rev() {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let (facts, key) = (classify(provider, &value), row_key(&value));
        if facts.iter().any(|f| matches!(f, RecordFact::Idle(_))) {
            return Ok(if assistant_after { start } else { extent });
        }
        // A tracker that has not seen the file start opens only on rows that open a turn alone.
        let mut probe = TurnTracker::starting_at(1);
        if facts.iter().any(|f| {
            matches!(
                probe.observe(f, key.as_ref(), (start, start), Utc::now()),
                TurnEvent::Opened(_)
            )
        }) {
            return Ok(start);
        }
        assistant_after |= facts.contains(&RecordFact::Assistant);
    }
    Ok(extent)
}

/// Follows the store for `WindowStart` lines the CLI appends, from the end seen at startup.
struct WindowStartTail {
    path: PathBuf,
    offset: u64,
}

impl WindowStartTail {
    fn at_end(path: PathBuf) -> Self {
        let offset = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        Self { path, offset }
    }

    fn poll(&mut self) -> Vec<(DateTime<Utc>, Vec<WindowStartSource>)> {
        let mut bytes = Vec::new();
        let read = File::open(&self.path).and_then(|mut file| {
            file.seek(SeekFrom::Start(self.offset))?;
            file.take(MAX_READ_BYTES).read_to_end(&mut bytes)
        });
        if read.is_err() {
            return Vec::new();
        }
        let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        // A line longer than one read can never complete here, so it is skipped.
        let consumed = if whole == 0 && bytes.len() as u64 == MAX_READ_BYTES {
            bytes.len()
        } else {
            whole
        };
        self.offset += consumed as u64;
        let lines = bytes[..whole].split(|b| *b == b'\n');
        lines
            .filter_map(|line| serde_json::from_slice::<StoredRecord>(line).ok())
            .filter_map(|stored| match stored.record {
                ShadowRecord::WindowStart { t0, sources } => Some((t0, sources)),
                _ => None,
            })
            .collect()
    }
}

/// Starts the shadow once per process when `tui_o.shadow.enabled`; it never blocks intake.
pub fn spawn_if_enabled(config: Option<&TuiOConfig>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    let Some(config) = config.map(|c| &c.shadow).filter(|c| c.enabled) else {
        return;
    };
    if STARTED.set(()).is_err() {
        return;
    }
    let store = crate::config::runtime_root()
        .ok_or_else(|| io::Error::other("runtime root unresolved"))
        .and_then(|runtime_root| {
            ShadowStore::open(ShadowRoot::under(&runtime_root)?, DISK_CAP_BYTES)
        });
    let store = match store {
        Ok(store) => store,
        Err(error) => return tracing::warn!(%error, "o-shadow: store unavailable; not started"),
    };
    let (tap, gateway_rx) = GatewayTap::new(&config.channel_allowlist, TAP_CAPACITY);
    if TAP.set(tap).is_err() {
        return;
    }
    let open_capture: CaptureOpener = Box::new(|binding, start| {
        Ok(Box::new(SourceCapture::open(binding.source.clone(), start)?) as Box<dyn CaptureSource>)
    });
    let tail = WindowStartTail::at_end(store.root().records_path());
    let link = Box::new(TranscriptDerive::default());
    let observer = Observer::new(Box::new(store), open_capture, link);
    let allowlist = config.channel_allowlist.clone();
    let spawned = std::thread::Builder::new()
        .name("o-shadow".into())
        .spawn(move || run(observer, tail, gateway_rx, allowlist));
    if let Err(error) = spawned {
        tracing::warn!(%error, "o-shadow: observe thread did not start");
    }
}

fn run(
    mut observer: Observer,
    mut tail: WindowStartTail,
    mut gateway_rx: mpsc::Receiver<LegacyTapEvent>,
    allowlist: Vec<u64>,
) {
    let mut readers: HashMap<(u64, String), BindingReader> = HashMap::new();
    observer.start(Utc::now());
    for tick in 0u64.. {
        std::thread::sleep(POLL_INTERVAL);
        for target in discover_targets(&allowlist) {
            let key = (target.channel_id, target.tmux_session.clone());
            readers
                .entry(key)
                .or_insert_with(|| BindingReader::new(Box::new(LiveBindingLookup), vec![target]));
        }
        let changes = readers
            .values_mut()
            .flat_map(|reader| reader.poll())
            .collect();
        for (t0, sources) in tail.poll() {
            observer.window_start(t0, &sources);
        }
        let legacy = std::iter::from_fn(|| gateway_rx.try_recv().ok()).collect();
        let dropped = TAP.get().map_or(0, GatewayTap::take_dropped);
        observer.tick(Utc::now(), changes, legacy, dropped);
        if tick % 60 == 0 {
            tracing::info!(metrics = ?observer.metrics(), "o-shadow: metrics");
        }
    }
}

/// Allowlisted channels that currently have a TUI binding in the in-memory registry.
fn discover_targets(allowlist: &[u64]) -> Vec<ShadowTarget> {
    use crate::services::tui_prompt_dedupe::{
        owner_channel_for_tmux_session, runtime_bindings_for_kind,
    };
    [RuntimeHandoffKind::ClaudeTui, RuntimeHandoffKind::CodexTui]
        .into_iter()
        .flat_map(runtime_bindings_for_kind)
        .filter_map(|(tmux_session, _)| {
            let channel_id = owner_channel_for_tmux_session(&tmux_session)?;
            allowlist.contains(&channel_id).then_some(ShadowTarget {
                channel_id,
                tmux_session,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_o::shadow::binding_reader::source_id_for;
    use crate::services::tui_o::shadow::diff::sha256_hex;
    use crate::services::tui_o::shadow::{
        CapturedRecord, PieceDigest, ShadowTurn, ShadowUnit, SourceAnomaly, SourceAnomalyKind,
        SourceRange, UnitKey, UnitKind,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn created(msg_id: u64, at: DateTime<Utc>, content: &str) -> LegacyTapEvent {
        let content = content.to_string();
        LegacyTapEvent::Created {
            channel_id: 7,
            msg_id,
            at,
            content,
        }
    }

    #[test]
    fn tap_counts_a_full_queue_as_dropped_instead_of_waiting() {
        let (tap, mut rx) = GatewayTap::new(&[7], 1);
        tap.offer(created(1, Utc::now(), "a"));
        tap.offer(created(2, Utc::now(), "b"));
        assert_eq!((tap.take_dropped(), tap.take_dropped()), (1, 0));
        assert!(matches!(
            rx.try_recv(),
            Ok(LegacyTapEvent::Created { msg_id: 1, .. })
        ));
        assert!(tap.watches(7) && !tap.watches(8));
    }

    #[test]
    fn tap_copies_own_bot_messages_and_authorless_edits_only() {
        let mut own = serenity::Message::default();
        own.author.id = serenity::UserId::new(1);
        own.channel_id = serenity::ChannelId::new(7);
        own.id = serenity::MessageId::new(9);
        let mut other = own.clone();
        other.author.id = serenity::UserId::new(2);
        let message = |new_message| serenity::FullEvent::Message { new_message };
        assert!(matches!(
            tap_event(1, &message(own)),
            Some(LegacyTapEvent::Created { msg_id: 9, .. })
        ));
        assert!(tap_event(1, &message(other)).is_none());
        let update = |author: serde_json::Value| serenity::FullEvent::MessageUpdate {
            old_if_available: None,
            new: None,
            event: serde_json::from_value(serde_json::json!({
                "id": "9", "channel_id": "7", "content": "x", "author": author,
            }))
            .unwrap(),
        };
        let user = |id: &str| serde_json::json!({"id": id, "username": "u", "discriminator": "0"});
        assert!(tap_event(1, &update(serde_json::Value::Null)).is_some());
        assert!(tap_event(1, &update(user("2"))).is_none());
    }

    #[derive(Clone, Default)]
    struct Records(Arc<Mutex<Vec<ShadowRecord>>>);

    impl ShadowSink for Records {
        fn append(&mut self, record: &ShadowRecord) -> io::Result<()> {
            self.0.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    struct ScriptedCapture(SourceId, VecDeque<CaptureOutcome>);

    impl CaptureSource for ScriptedCapture {
        fn source(&self) -> &SourceId {
            &self.0
        }
        fn poll(&mut self, _max_bytes: u64) -> CaptureOutcome {
            self.1.pop_front().unwrap_or_else(|| {
                CaptureOutcome::Batch(CaptureBatch {
                    source: self.0.clone(),
                    records: vec![],
                    captured_through: 0,
                })
            })
        }
    }

    struct Link(Option<ShadowUnit>);

    impl DeriveLink for Link {
        fn capture_start(&mut self, _binding: &SourceBinding, extent: u64) -> u64 {
            extent - 3
        }
        fn attach(&mut self, _source: &SourceId, _extent: u64, _at: DateTime<Utc>) {}
        fn window_start(&mut self, _t0: DateTime<Utc>, _source: &SourceId, _extent: u64) {}
        fn derive(&mut self, _binding: &SourceBinding, _batch: &CaptureBatch) -> Vec<DeriveOutput> {
            self.0
                .take()
                .map(DeriveOutput::Sealed)
                .into_iter()
                .collect()
        }
    }

    #[test]
    fn observer_attaches_derives_diffs_and_persists_only_through_the_sink() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"0123456789").unwrap();
        let source = SourceId {
            session_id: "s".into(),
            path: file.path().into(),
            dev: 1,
            ino: 1,
        };
        let now = Utc::now();
        let unit = ShadowUnit {
            unit_key: UnitKey {
                channel_id: 7,
                provider: ShadowProvider::Claude,
                native_key: "m:0".into(),
                kind: UnitKind::Body,
            },
            kind: UnitKind::Body,
            source_range: SourceRange {
                source: source.clone(),
                start: 7,
                end: 10,
            },
            sealed_at: now,
            pieces: vec![PieceDigest {
                index: 0,
                units: 5,
                sha256: sha256_hex("hello"),
            }],
        };
        let line = CapturedRecord {
            start: 7,
            end: 10,
            line: b"{}".to_vec(),
        };
        let batch = CaptureBatch {
            source: source.clone(),
            records: vec![line],
            captured_through: 10,
        };
        let anomaly = SourceAnomaly {
            source: source.clone(),
            kind: SourceAnomalyKind::Shrunk,
            captured_through: 10,
            detail: String::new(),
        };
        let script = VecDeque::from([
            CaptureOutcome::Batch(batch),
            CaptureOutcome::Anomaly(anomaly),
        ]);
        let starts = Arc::new(Mutex::new(Vec::new()));
        let seen = starts.clone();
        let opener: CaptureOpener = Box::new(move |binding, start| {
            seen.lock().unwrap().push(start);
            Ok(
                Box::new(ScriptedCapture(binding.source.clone(), script.clone()))
                    as Box<dyn CaptureSource>,
            )
        });
        let sink = Records::default();
        let mut observer =
            Observer::new(Box::new(sink.clone()), opener, Box::new(Link(Some(unit))));
        let binding = SourceBinding {
            channel_id: 7,
            provider: ShadowProvider::Claude,
            source,
        };
        let change = BindingChange {
            channel_id: 7,
            old: None,
            new: Some(binding),
            at: now,
        };
        observer.start(now);
        observer.tick(now, vec![change], vec![created(100, now, "hello")], 2);
        observer.tick(now + chrono::Duration::seconds(1), vec![], vec![], 0);
        observer.tick(now + chrono::Duration::seconds(300), vec![], vec![], 0);
        observer.tick(now + chrono::Duration::seconds(601), vec![], vec![], 0);
        let kinds: Vec<String> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                serde_json::to_value(r).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let expected = [
            "header", "attach", "binding", "derived", "tap_gap", "diff", "anomaly", "diff",
            "legacy",
        ];
        assert_eq!(kinds, expected);
        assert_eq!(*starts.lock().unwrap(), vec![7]);
        let records = sink.0.lock().unwrap();
        assert!(
            matches!(&records[7], ShadowRecord::Diff { diff } if diff.class == crate::services::tui_o::shadow::DiffClass::Match && diff.legacy_msg_ids == vec![100])
        );
        assert_eq!(observer.metrics().tap_dropped_total, 2);
    }

    fn fixed_clock() -> DateTime<Utc> {
        "2026-09-27T12:06:30Z".parse().unwrap()
    }

    #[test]
    fn a_turn_running_at_attach_is_captured_from_its_opener_and_closes_live() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tui_o_shadow");
        let fixture = std::fs::read_to_string(format!("{path}/derive_claude_tui.jsonl")).unwrap();
        let lines: Vec<String> = fixture.lines().map(|line| format!("{line}\n")).collect();
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, lines[..3].concat()).unwrap();
        let binding = SourceBinding {
            channel_id: 7,
            provider: ShadowProvider::Claude,
            source: source_id_for("s-claude", &transcript).unwrap(),
        };
        let sink = Records::default();
        let opener: CaptureOpener = Box::new(|binding, start| {
            let capture = SourceCapture::open(binding.source.clone(), start)?;
            Ok(Box::new(capture) as Box<dyn CaptureSource>)
        });
        let link = Box::new(TranscriptDerive::with_clock(fixed_clock));
        let mut observer = Observer::new(Box::new(sink.clone()), opener, link);
        let at: DateTime<Utc> = "2026-09-27T12:06:00.500Z".parse().unwrap();
        let change = BindingChange {
            channel_id: 7,
            old: None,
            new: Some(binding),
            at,
        };
        observer.tick(at, vec![change], vec![], 0);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, lines[3..10].concat().as_bytes()).unwrap();
        observer.tick(at, vec![], vec![], 0);
        let records = sink.0.lock().unwrap();
        let (opener_at, extent) = (lines[0].len() as u64, lines[..3].concat().len() as u64);
        assert!(records.iter().any(|r| matches!(r,
            ShadowRecord::Attach { capture_start, attach_extent, .. }
                if (*capture_start, *attach_extent) == (opener_at, extent))));
        let turns: Vec<&ShadowTurn> = records
            .iter()
            .filter_map(|r| match r {
                ShadowRecord::Derived {
                    output: DeriveOutput::TurnClosed(turn),
                } => Some(turn),
                _ => None,
            })
            .collect();
        assert_eq!(turns.len(), 1);
        assert!(
            turns[0].live && turns[0].native_turn_id == "u-1",
            "{turns:?}"
        );
    }

    #[test]
    fn only_window_starts_appended_after_startup_are_polled() {
        let dir = tempfile::tempdir().unwrap();
        let root = ShadowRoot::under(dir.path()).unwrap();
        let mut store = ShadowStore::open(root, DISK_CAP_BYTES).unwrap();
        let source = SourceId {
            session_id: "s".into(),
            path: dir.path().join("t.jsonl"),
            dev: 1,
            ino: 1,
        };
        let window_start = |minute: u32| ShadowRecord::WindowStart {
            t0: format!("2026-09-27T12:{minute:02}:00Z").parse().unwrap(),
            sources: vec![WindowStartSource {
                source: source.clone(),
                window_start_extent: 10,
            }],
        };
        store.append(&window_start(1)).unwrap();
        let mut tail = WindowStartTail::at_end(store.root().records_path());
        store.append(&window_start(2)).unwrap();
        let polled = tail.poll();
        assert_eq!(polled.len(), 1);
        assert_eq!(polled[0].0.to_rfc3339(), "2026-09-27T12:02:00+00:00");
        assert!(tail.poll().is_empty());
    }
}
