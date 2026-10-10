//! Gateway tap and the observe loop it feeds: capture -> derive -> diff, persisted only via the sink.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::services::tui_o::repost::config::RepostConfig;

use super::binding_reader::{BindingReader, LiveBindingLookup, ShadowTarget};
use super::capture::SourceCapture;
use super::derive::TranscriptDerive;
use super::diff::{WindowDiff, sha256_hex};
use super::identity::{RecordFact, classify, row_key};
use super::metrics::MetricsSnapshot;
use super::root::{ShadowRoot, ShadowStore, StoredRecord};
use super::seal::{TurnEvent, TurnTracker};
use super::{
    BindingChange, CaptureBatch, CaptureOutcome, CaptureSource, CapturedRecord, DISK_CAP_BYTES,
    DeriveOutput, IDENTITY_VERSION, LegacyMsg, LegacyTapEvent, MAX_READ_BYTES, POLL_INTERVAL,
    SCHEMA_VERSION, ShadowConfig, ShadowDerive, ShadowDiff, ShadowProvider, ShadowRecord,
    ShadowSink, SourceBinding, SourceId, TAP_CAPACITY, WindowStartSource,
};

/// The `tui_o:` config section; absent means every output-track feature is off.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiOConfig {
    pub shadow: ShadowConfig,
    pub writer: WriterChannelsConfig,
    pub turn: crate::services::tui_o::turn_mode::TurnConfig,
    /// Operator channel for writer alarms; absent keeps them to health and logs.
    pub alert_channel_id: Option<u64>,
    #[serde(skip_serializing_if = "RepostConfig::is_default")]
    pub repost: RepostConfig,
    /// #6325: Codex history adoption at boot. Off (the default) installs no observation runtime.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub codex_history_adoption: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WriterChannelsConfig {
    pub channels: std::collections::BTreeSet<u64>,
    /// Selects every `agents[].channels` binding that resolves to a Claude or Codex TUI.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub all_tui: bool,
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

/// The tap installed by `start`; `None` while the shadow is off.
pub fn installed() -> Option<&'static GatewayTap> {
    TAP.get()
}

/// Derive-side hooks; one instance serves every source so keys seen on another source stay visible.
pub trait DeriveLink: Send {
    /// Offset to capture from: the running turn's start found by the bounded reverse scan, else `extent`.
    fn capture_start(&mut self, binding: &SourceBinding, extent: u64) -> u64;
    /// Records below `attach_extent` must come out as historical, never live.
    fn attach(&mut self, source: &SourceId, attach_extent: u64, attached_at: DateTime<Utc>);
    /// Sources attached before `t0` count as live only when `sources` lists them.
    fn window_start(&mut self, t0: DateTime<Utc>, sources: &[WindowStartSource]);
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
    /// Batches with their capture time, derived after the loop applies any WindowStart it read.
    captured: Vec<(DateTime<Utc>, SourceBinding, CaptureBatch)>,
    window_applied: bool,
    last_checkpoint: Option<DateTime<Utc>>,
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
            captured: Vec::new(),
            window_applied: false,
            last_checkpoint: None,
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

    /// Opens the window at `t0`; listed sources move their live boundary to their size at t0.
    pub fn window_start(&mut self, t0: DateTime<Utc>, sources: &[WindowStartSource]) {
        self.link.window_start(t0, sources);
        self.window_applied = true;
    }

    /// Binding changes, last tick's captures derived, one capture poll per feed, then tap events and diffs.
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
        // Before the first window, the newest HOLD_BYTES of captures wait for a late WindowStart.
        let (applied, mut bytes, mut held) =
            (self.window_applied, 0, std::mem::take(&mut self.captured));
        let keep = (held.iter().rev())
            .take_while(|held| {
                bytes += held_bytes(held);
                !applied && bytes <= HOLD_BYTES
            })
            .count();
        let due: Vec<_> = held.drain(..held.len() - keep).collect();
        self.captured = held;
        let outputs: Vec<DeriveOutput> = (due.iter())
            .flat_map(|(at, binding, batch)| {
                OBSERVED_AT.set(Some(*at));
                let outputs = self.link.derive(binding, batch);
                OBSERVED_AT.set(None);
                outputs
            })
            .collect();
        for output in outputs {
            self.diff.observe_derived(&output, now);
            self.persist(ShadowRecord::Derived { output });
        }
        let (mut anomalies, mut lags) = (Vec::new(), Vec::new());
        for feed in self.feeds.values_mut() {
            match feed.capture.poll(MAX_READ_BYTES) {
                CaptureOutcome::Batch(batch) if !batch.records.is_empty() => {
                    lags.extend(capture_lag_ms(&feed.binding.source.path, now));
                    self.captured.push((now, feed.binding.clone(), batch));
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
        for event in &legacy {
            self.diff.observe_legacy(event);
            // Also recorded when created, so the report can tell a window message is still open.
            if let LegacyTapEvent::Created {
                channel_id,
                msg_id,
                at,
                content,
            } = event
            {
                let msg = LegacyMsg {
                    msg_id: *msg_id,
                    channel_id: *channel_id,
                    created_at: *at,
                    edits: Vec::new(),
                    deleted: false,
                    content_sha256: sha256_hex(content),
                };
                self.persist(ShadowRecord::Legacy { msg });
            }
        }
        // A loss is dated by the span since the previous TapGap, so collections are checkpointed too.
        let checkpoint = Duration::seconds(CHECKPOINT_SECS);
        if dropped > 0
            || self
                .last_checkpoint
                .is_none_or(|last| now - last >= checkpoint)
        {
            self.diff.observe_tap_gap(dropped, now);
            self.persist(ShadowRecord::TapGap { dropped });
            self.last_checkpoint = Some(now);
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

/// Heap held for the first window, as `held_bytes` counts it; older captures derive without one.
const HOLD_BYTES: usize = 32 * 1024 * 1024;

/// Heap a held batch keeps: record slots at capacity, each line, and both copies of its source id.
fn held_bytes((_, binding, batch): &(DateTime<Utc>, SourceBinding, CaptureBatch)) -> usize {
    let id = |source: &SourceId| source.session_id.capacity() + source.path.capacity();
    let lines: usize = batch.records.iter().map(|r| r.line.capacity()).sum();
    let slots = batch.records.capacity() * std::mem::size_of::<CapturedRecord>();
    let entry = std::mem::size_of::<(DateTime<Utc>, SourceBinding, CaptureBatch)>();
    entry + id(&binding.source) + id(&batch.source) + slots + lines
}

/// Most seconds between two TapGap records while the observer runs, drops or not.
pub const CHECKPOINT_SECS: i64 = 10;

thread_local! {
    /// Capture time of the batch being derived.
    static OBSERVED_AT: Cell<Option<DateTime<Utc>>> = const { Cell::new(None) };
}

/// Derive clock: when the observer read the batch, so holding it does not move its evidence.
pub fn observed_at() -> DateTime<Utc> {
    OBSERVED_AT.get().unwrap_or_else(Utc::now)
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
    fn window_start(&mut self, t0: DateTime<Utc>, sources: &[WindowStartSource]) {
        // Opened first so an empty list still sets t0 for sources attached later.
        self.window_open(t0);
        for listed in sources {
            let extent = listed.window_start_extent;
            TranscriptDerive::window_start(self, t0, &listed.source, extent);
        }
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

    /// Reads to the end, so a WindowStart applies on the loop pass after its append.
    fn poll(&mut self) -> Vec<(DateTime<Utc>, Vec<WindowStartSource>)> {
        let mut starts = Vec::new();
        loop {
            let mut bytes = Vec::new();
            let read = File::open(&self.path).and_then(|mut file| {
                file.seek(SeekFrom::Start(self.offset))?;
                file.take(MAX_READ_BYTES).read_to_end(&mut bytes)
            });
            if read.is_err() {
                return starts;
            }
            let full = bytes.len() as u64 == MAX_READ_BYTES;
            let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            // A line longer than one read can never complete here, so it is skipped.
            self.offset += if whole == 0 && full {
                bytes.len()
            } else {
                whole
            } as u64;
            let lines = bytes[..whole].split(|b| *b == b'\n');
            let stored = lines.filter_map(|line| serde_json::from_slice::<StoredRecord>(line).ok());
            starts.extend(stored.filter_map(|stored| match stored.record {
                ShadowRecord::WindowStart { t0, sources } => Some((t0, sources)),
                _ => None,
            }));
            if !full {
                return starts;
            }
        }
    }
}

/// Lists allowlisted channels with a live TUI binding; supplied by the host, which may read relay state.
pub type TargetDiscovery = fn(&[u64]) -> Vec<ShadowTarget>;

/// Starts the shadow once per process; the caller checks `enabled`, and intake never waits on it.
pub fn start(config: &ShadowConfig, runtime_root: &Path, discover: TargetDiscovery) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let store =
        ShadowRoot::under(runtime_root).and_then(|root| ShadowStore::open(root, DISK_CAP_BYTES));
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
    let link = Box::new(TranscriptDerive::with_clock(observed_at));
    let observer = Observer::new(Box::new(store), open_capture, link);
    let allowlist = config.channel_allowlist.clone();
    let spawned = std::thread::Builder::new()
        .name("o-shadow".into())
        .spawn(move || run(observer, tail, gateway_rx, allowlist, discover));
    if let Err(error) = spawned {
        tracing::warn!(%error, "o-shadow: observe thread did not start");
    }
}

fn run(
    mut observer: Observer,
    mut tail: WindowStartTail,
    mut gateway_rx: mpsc::Receiver<LegacyTapEvent>,
    allowlist: Vec<u64>,
    discover: TargetDiscovery,
) {
    let mut readers: HashMap<(u64, String), BindingReader> = HashMap::new();
    observer.start(Utc::now());
    for tick in 0u64.. {
        std::thread::sleep(POLL_INTERVAL);
        for target in discover(&allowlist) {
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
        fn window_start(&mut self, _t0: DateTime<Utc>, _sources: &[WindowStartSource]) {}
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
        observer.window_start(now, &[]);
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
        // A drop is recorded at once; quiet collections only every CHECKPOINT_SECS. A Legacy
        // message is recorded when created and again when it retires.
        let expected = [
            "header", "attach", "binding", "legacy", "tap_gap", "diff", "derived", "anomaly",
            "tap_gap", "diff", "tap_gap", "legacy",
        ];
        assert_eq!(kinds, expected);
        assert_eq!(*starts.lock().unwrap(), vec![7]);
        let records = sink.0.lock().unwrap();
        assert!(
            matches!(&records[3], ShadowRecord::Legacy { msg } if msg.msg_id == 100 && msg.created_at == now)
        );
        assert!(
            matches!(&records[9], ShadowRecord::Diff { diff } if diff.class == crate::services::tui_o::shadow::DiffClass::Match && diff.legacy_msg_ids == vec![100])
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
        // No source was attached at t0, so the window lists none and this source joins it.
        observer.window_start(at - chrono::Duration::seconds(30), &[]);
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

    fn race_clock() -> DateTime<Utc> {
        "2026-09-27T12:06:06.750Z".parse().unwrap()
    }

    #[test]
    fn a_window_start_applied_minutes_after_a_capture_still_reaches_it_at_its_capture_time() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tui_o_shadow");
        let fixture = std::fs::read_to_string(format!("{path}/derive_claude_tui.jsonl")).unwrap();
        let lines: Vec<String> = fixture.lines().map(|line| format!("{line}\n")).collect();
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("race.jsonl");
        std::fs::write(&transcript, lines[..3].concat()).unwrap();
        let source = source_id_for("s-claude", &transcript).unwrap();
        let (channel_id, provider) = (7, ShadowProvider::Claude);
        let binding = SourceBinding {
            channel_id,
            provider,
            source: source.clone(),
        };
        let extent = lines[..3].concat().len() as u64;
        let sink = Records::default();
        let opener: CaptureOpener = Box::new(|binding, start| {
            let capture = SourceCapture::open(binding.source.clone(), start)?;
            Ok(Box::new(capture) as Box<dyn CaptureSource>)
        });
        let link = Box::new(TranscriptDerive::with_clock(observed_at));
        let mut observer = Observer::new(Box::new(sink.clone()), opener, link);
        let attached_at: DateTime<Utc> = "2026-09-27T12:05:00Z".parse().unwrap();
        let change = BindingChange {
            channel_id,
            old: None,
            new: Some(binding),
            at: attached_at,
        };
        observer.tick(attached_at, vec![change], vec![], 0);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, lines[3..10].concat().as_bytes()).unwrap();
        // The CLI appended WindowStart more than five minutes after the capture of the closer.
        let captured_at = race_clock() - Duration::seconds(1);
        observer.tick(captured_at, vec![], vec![], 0);
        observer.tick(race_clock(), vec![], vec![], 0);
        let expired = captured_at + Duration::seconds(301);
        observer.tick(expired, vec![], vec![], 0);
        let t0: DateTime<Utc> = "2026-09-27T12:06:05Z".parse().unwrap();
        let window_start_extent = extent;
        observer.window_start(
            t0,
            &[WindowStartSource {
                source,
                window_start_extent,
            }],
        );
        observer.tick(expired + Duration::seconds(1), vec![], vec![], 0);
        let records = sink.0.lock().unwrap();
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
            turns[0].live && turns[0].closed_at == captured_at,
            "{turns:?}"
        );
    }

    #[test]
    fn captures_waiting_for_the_first_window_stay_within_the_hold_budget() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"0123456789").unwrap();
        let (channel_id, provider) = (7, ShadowProvider::Claude);
        let source = source_id_for("s", file.path()).unwrap();
        // One complete line of MAX_READ_BYTES per tick, the most a feed reads.
        let line = CapturedRecord {
            start: 0,
            end: MAX_READ_BYTES,
            line: vec![b'x'; MAX_READ_BYTES as usize - 1],
        };
        let batch = |_| {
            let (source, records) = (source.clone(), vec![line.clone()]);
            CaptureOutcome::Batch(CaptureBatch {
                source,
                records,
                captured_through: 0,
            })
        };
        let script: VecDeque<CaptureOutcome> = (0..40).map(batch).collect();
        let opener: CaptureOpener = Box::new(move |binding, _| {
            let capture = ScriptedCapture(binding.source.clone(), script.clone());
            Ok(Box::new(capture) as Box<dyn CaptureSource>)
        });
        let sink = Box::new(Records::default());
        let mut observer = Observer::new(sink, opener, Box::new(Link(None)));
        let now = Utc::now();
        let binding = SourceBinding {
            channel_id,
            provider,
            source,
        };
        let (old, new, at) = (None, Some(binding), now);
        let change = BindingChange {
            channel_id,
            old,
            new,
            at,
        };
        observer.tick(now, vec![change], vec![], 0);
        (1..40).for_each(|i| observer.tick(now + Duration::seconds(i), vec![], vec![], 0));
        let held: usize = (observer.captured.iter())
            .flat_map(|(_, _, batch)| &batch.records)
            .map(|record| record.line.len())
            .sum();
        // The budget plus the one read taken after this tick's release.
        assert!(held <= HOLD_BYTES + MAX_READ_BYTES as usize, "{held}");
        assert!(observer.captured.len() < 40);
    }

    #[test]
    fn blank_lines_spend_the_first_window_hold_budget() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let source = source_id_for("blank", file.path()).unwrap();
        let (channel_id, provider) = (7, ShadowProvider::Claude);
        let binding = SourceBinding {
            channel_id,
            provider,
            source,
        };
        let opener: CaptureOpener = Box::new(|binding, start| {
            let capture = SourceCapture::open(binding.source.clone(), start)?;
            Ok(Box::new(capture) as Box<dyn CaptureSource>)
        });
        let link = Box::new(TranscriptDerive::with_clock(observed_at));
        let mut observer = Observer::new(Box::new(Records::default()), opener, link);
        let now = Utc::now();
        let (old, new, at) = (None, Some(binding), now);
        let change = BindingChange {
            channel_id,
            old,
            new,
            at,
        };
        observer.tick(now, vec![change], vec![], 0);
        let mut writer = std::fs::OpenOptions::new().append(true).open(file.path());
        let writer = writer.as_mut().unwrap();
        for i in 1..=6 {
            io::Write::write_all(writer, &vec![b'\n'; MAX_READ_BYTES as usize]).unwrap();
            observer.tick(now + Duration::seconds(i), vec![], vec![], 0);
        }
        // Newline-only records carry no line bytes, yet each keeps a record slot.
        let slot = std::mem::size_of::<CapturedRecord>();
        let kept: usize = (observer.captured.iter())
            .map(|(_, _, batch)| {
                let lines: usize = batch.records.iter().map(|r| r.line.capacity()).sum();
                batch.records.capacity() * slot + lines
            })
            .sum();
        assert!(kept <= 200 * 1024 * 1024, "{kept}");
        assert_eq!(observer.captured.len(), 1);
    }

    #[test]
    fn the_window_start_tail_reads_to_the_end_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        std::fs::write(&path, b"").unwrap();
        let mut tail = WindowStartTail::at_end(path.clone());
        let line = |record| {
            let stored = StoredRecord {
                at: Utc::now(),
                record,
            };
            serde_json::to_string(&stored).unwrap() + "\n"
        };
        // More than one read of other rows lands ahead of the line the CLI appends.
        let filler = line(ShadowRecord::TapGap { dropped: 0 });
        let mut bytes = filler.repeat(2 * MAX_READ_BYTES as usize / filler.len());
        let (t0, sources) = (Utc::now(), Vec::new());
        bytes += &line(ShadowRecord::WindowStart { t0, sources });
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(tail.poll(), vec![(t0, Vec::new())]);
    }
}
