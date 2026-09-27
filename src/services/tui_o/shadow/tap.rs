//! Gateway tap and the observe loop it feeds: capture -> derive -> diff, persisted only via the sink.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::binding_reader::{BindingReader, LiveBindingLookup, ShadowTarget};
use super::capture::SourceCapture;
use super::diff::WindowDiff;
use super::metrics::MetricsSnapshot;
use super::root::{ShadowRoot, ShadowStore};
use super::{
    BindingChange, CaptureOutcome, CaptureSource, DISK_CAP_BYTES, IDENTITY_VERSION, LegacyTapEvent,
    MAX_READ_BYTES, POLL_INTERVAL, SCHEMA_VERSION, ShadowConfig, ShadowDerive, ShadowDiff,
    ShadowRecord, ShadowSink, SourceBinding, TAP_CAPACITY,
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

/// Derive-side hooks needed when a source is attached.
pub trait DeriveLink: Send {
    /// Offset to capture from: the last turn opener found by the bounded reverse scan, else `extent`.
    fn capture_start(&mut self, binding: &SourceBinding, extent: u64) -> u64;
    /// Records below `attach_extent` must come out as historical, never live.
    fn derive_for(&mut self, binding: &SourceBinding, attach_extent: u64) -> Box<dyn ShadowDerive>;
}

pub type CaptureOpener =
    Box<dyn FnMut(&SourceBinding, u64) -> io::Result<Box<dyn CaptureSource>> + Send>;

struct Feed {
    binding: SourceBinding,
    capture: Box<dyn CaptureSource>,
    derive: Box<dyn ShadowDerive>,
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
                let derive = self.link.derive_for(&binding, attach_extent);
                let source = binding.source.clone();
                self.persist(ShadowRecord::Attach {
                    source,
                    attach_extent,
                    capture_start,
                    attached_at: now,
                });
                self.feeds.insert(
                    binding.channel_id,
                    Feed {
                        binding,
                        capture,
                        derive,
                    },
                );
            }
            Err(error) => {
                tracing::warn!(%error, channel_id = binding.channel_id, "o-shadow: attach failed")
            }
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
                    outputs.extend(feed.derive.derive(&feed.binding, &batch));
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

/// The derive implementation for attached sources; `None` keeps an enabled shadow from starting half-built.
fn derive_link() -> Option<Box<dyn DeriveLink>> {
    None
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
    let Some(link) = derive_link() else {
        return tracing::warn!("o-shadow: enabled but no derive is linked; not started");
    };
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
    let observer = Observer::new(Box::new(store), open_capture, link);
    let allowlist = config.channel_allowlist.clone();
    let spawned = std::thread::Builder::new()
        .name("o-shadow".into())
        .spawn(move || run(observer, gateway_rx, allowlist));
    if let Err(error) = spawned {
        tracing::warn!(%error, "o-shadow: observe thread did not start");
    }
}

fn run(
    mut observer: Observer,
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
    use crate::services::tui_o::shadow::diff::sha256_hex;
    use crate::services::tui_o::shadow::{
        CaptureBatch, CapturedRecord, DeriveOutput, PieceDigest, ShadowProvider, ShadowUnit,
        SourceAnomaly, SourceAnomalyKind, SourceId, SourceRange, UnitKey, UnitKind,
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

    struct OneUnitDerive(Option<ShadowUnit>);

    impl ShadowDerive for OneUnitDerive {
        fn derive(&mut self, _binding: &SourceBinding, _batch: &CaptureBatch) -> Vec<DeriveOutput> {
            self.0
                .take()
                .map(DeriveOutput::Sealed)
                .into_iter()
                .collect()
        }
        fn unsealed(&self) -> Vec<UnitKey> {
            Vec::new()
        }
    }

    struct Link(Option<ShadowUnit>);

    impl DeriveLink for Link {
        fn capture_start(&mut self, _binding: &SourceBinding, extent: u64) -> u64 {
            extent - 3
        }
        fn derive_for(&mut self, _binding: &SourceBinding, _extent: u64) -> Box<dyn ShadowDerive> {
            Box::new(OneUnitDerive(self.0.take()))
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
}
