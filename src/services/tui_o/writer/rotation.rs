//! The sources one channel reads. A bind attaches its new source at offset 0 and the old one is
//! read until it provably stops; a resumed or forked source skips what its parent already holds.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::watch;
use tokio::time::Instant;

use super::binding::{BindingCause, BindingEvent, BindingEvents, BindingRecord, BindingTarget};
use super::deliver::ChannelWriter;
use super::pieces::{Derived, UnitDeriver};
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm};
use crate::services::tui_o::shadow::capture::{SourceCapture, file_identity};
use crate::services::tui_o::shadow::identity::{RecordFact, classify, native_time};
use crate::services::tui_o::shadow::{
    CaptureBatch, CaptureOutcome, CaptureSource, CapturedRecord, MAX_READ_BYTES, ShadowProvider,
    SourceId, UnitKind,
};
use crate::services::tui_o::store::StoreError;
use crate::services::tui_o::store::rotation::{Boundary, Rotation, SourceLink};
use crate::services::tui_o::store::spool::{SpoolFrame, source_key};

/// An old source whose length holds this long after its successor's first record is retired.
pub const RETIRE_QUIET: Duration = Duration::from_secs(10);
/// An old source still growing this long after its rotation alarms; both stay read.
pub const OLD_GROWTH_ALARM: Duration = Duration::from_secs(600);
/// A retired source is watched this long; growth un-retires it.
pub const RETIRED_WATCH: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_READERS: usize = 3;
/// A parent longer than this is not scanned, so its lineage stays unproven.
pub const LINEAGE_SCAN_CAP_BYTES: u64 = 256 << 20;
const PENDING_BIND_ALARM_SECS: i64 = 60;
/// A first new record older than its bind by more than this may be an unrelated old session.
const BIND_SLACK_SECS: i64 = 60;

type Keys = HashSet<(String, UnitKind)>;

struct Reader {
    source: SourceId,
    /// `None` once a retired source's watch has ended or its file is gone.
    capture: Option<SourceCapture>,
    /// A batch the full spool refused; it is retried before the source is read again.
    pending: Option<CaptureBatch>,
    captured_any: bool,
    /// Last length seen and since when it held after the successor's first record.
    quiet: Option<(u64, Instant)>,
    rotated_at: Option<Instant>,
    growth_alarmed: bool,
    /// Set while retired: the source is only stat-watched until then.
    watch_until: Option<Instant>,
}

impl Reader {
    fn new(source: SourceId, capture: Option<SourceCapture>) -> Self {
        Self {
            source,
            capture,
            pending: None,
            captured_any: false,
            quiet: None,
            rotated_at: None,
            growth_alarmed: false,
            watch_until: None,
        }
    }

    fn reading(&self) -> bool {
        self.capture.is_some() && self.watch_until.is_none()
    }
}

pub struct Sources<B> {
    channel: u64,
    provider: ShadowProvider,
    bindings: Arc<B>,
    notice: watch::Receiver<u64>,
    checkpoint: Option<u64>,
    rotation: Rotation,
    /// Every attached source, in bind order.
    readers: Vec<Reader>,
    /// Parent native keys by spool key; `None` when the scan could not finish.
    lineage: HashMap<String, Option<Keys>>,
    /// Boundary decisions and alarms made while deriving, flushed before delivery.
    dirty: bool,
    raised: Vec<WriterAlarm>,
    readers_alarmed: bool,
    pending_alarmed: Option<u64>,
}

fn halted(context: &'static str) -> impl Fn(StoreError) -> WriterAlarm {
    move |error| WriterAlarm::Halted {
        detail: format!("{context}: {error:?}"),
    }
}

fn halt(detail: impl Into<String>) -> WriterAlarm {
    WriterAlarm::Halted {
        detail: detail.into(),
    }
}

/// Native keys a record would seal or announce; `None` when it cannot be read as units.
fn record_keys(provider: ShadowProvider, line: &[u8]) -> Option<Vec<(String, UnitKind)>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Some(Vec::new());
    }
    let value = serde_json::from_slice(line).ok()?;
    let mut keys = Vec::new();
    for fact in classify(provider, &value) {
        match fact {
            RecordFact::Unit(key, kind, _) | RecordFact::Announced(key, kind) => {
                keys.push((key, kind));
            }
            RecordFact::Blocked(_) => return None,
            _ => {}
        }
    }
    Some(keys)
}

fn record_time(line: &[u8]) -> Option<DateTime<Utc>> {
    native_time(&serde_json::from_slice(line).ok()?)
}

/// Read-only scan of the parent file for the keys a fork or resume copies.
fn scan_lineage(provider: ShadowProvider, parent: &SourceId) -> Option<Keys> {
    let file = File::open(&parent.path).ok()?;
    let meta = file.metadata().ok()?;
    if file_identity(&meta) != (parent.dev, parent.ino) || meta.len() > LINEAGE_SCAN_CAP_BYTES {
        return None;
    }
    let mut keys = Keys::new();
    for line in BufReader::new(file.take(LINEAGE_SCAN_CAP_BYTES)).split(b'\n') {
        keys.extend(record_keys(provider, &line.ok()?).unwrap_or_default());
    }
    Some(keys)
}

fn bound_source(event: &BindingEvent) -> Option<&SourceId> {
    match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        }
        | BindingRecord::Resolved { source, .. } => Some(source),
        _ => None,
    }
}

impl<B: BindingEvents> Sources<B> {
    pub fn new(channel: u64, provider: ShadowProvider, bindings: Arc<B>) -> Self {
        let notice = bindings.subscribe(channel);
        Self {
            channel,
            provider,
            bindings,
            notice,
            checkpoint: None,
            rotation: Rotation::default(),
            readers: Vec::new(),
            lineage: HashMap::new(),
            dirty: false,
            raised: Vec::new(),
            readers_alarmed: false,
            pending_alarmed: None,
        }
    }

    /// Re-derives every retained spool in bind order, then reopens each source at its cursor.
    pub fn resume<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut VecDeque<Derived>,
    ) -> Result<(), WriterAlarm> {
        self.rotation = writer.store().rotation().map_err(halted("rotation"))?;
        let checkpoint = writer.store().binding_checkpoint();
        self.checkpoint = checkpoint.map_err(halted("binding checkpoint"))?;
        let mut cursors: Vec<_> = writer.store().cursors().cloned().collect();
        cursors.sort_by_key(|cursor| self.rotation.link(&cursor.source).map_or(0, |l| l.seq));
        let now = Instant::now();
        for cursor in cursors {
            let (key, mut captured_any) = (source_key(&cursor.source), false);
            if let Some(SourceLink {
                boundary: Boundary::Pending { .. },
                ..
            }) = self.rotation.link(&cursor.source)
            {
                let source = cursor.source.clone();
                self.raised.push(WriterAlarm::BoundaryPending { source });
            }
            let replay = writer.store().for_each_frame(&cursor.source, |frame| {
                if let SpoolFrame::Record(record) = frame {
                    captured_any = true;
                    self.owe(deriver, owed, &key, &record);
                }
            });
            replay.map_err(halted("spool replay"))?;
            let opened = SourceCapture::open(cursor.source.clone(), cursor.captured_through);
            let capture = match opened {
                Ok(capture) => Some(capture),
                Err(_) if cursor.retired => None,
                Err(error) => return Err(halt(format!("source reopen: {error}"))),
            };
            if capture
                .as_ref()
                .is_some_and(|capture| capture.prefix_hash() != cursor.prefix_hash)
            {
                return Err(halt("source bytes before the cursor changed"));
            }
            let mut reader = Reader::new(cursor.source.clone(), capture);
            reader.captured_any = captured_any
                || !writer
                    .store()
                    .ledger()
                    .gc_segments(&cursor.source)
                    .is_empty();
            reader.rotated_at = self.rotation.successors.contains_key(&key).then_some(now);
            reader.watch_until = cursor.retired.then_some(now + RETIRED_WATCH);
            self.readers.push(reader);
        }
        self.flush(writer)
    }

    /// Applies binding events past the checkpoint in seq order, stopping at an unresolved bind.
    pub fn follow<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<(), WriterAlarm> {
        let checkpoint = match self.checkpoint {
            Some(checkpoint) => checkpoint,
            None => match self.seed(writer)? {
                Some(checkpoint) => checkpoint,
                None => return Ok(()),
            },
        };
        if *self.notice.borrow() <= checkpoint {
            return Ok(());
        }
        let Ok(events) = self.bindings.binding_events_since(self.channel, checkpoint) else {
            return Ok(());
        };
        let resolved: HashMap<u64, SourceId> = events
            .iter()
            .filter_map(|event| match &event.record {
                BindingRecord::Resolved {
                    resolves_seq,
                    source,
                } => Some((*resolves_seq, source.clone())),
                _ => None,
            })
            .collect();
        let mut expected = checkpoint + 1;
        for event in &events {
            if event.seq != expected {
                let found = event.seq;
                return Err(WriterAlarm::BindingGap { expected, found });
            }
            if event.channel_id != self.channel {
                return Err(halt("a binding event names another channel"));
            }
            if let BindingRecord::Bound {
                old,
                new,
                cause,
                parent_hint,
                ..
            } = &event.record
            {
                let new = match (new, resolved.get(&event.seq)) {
                    (BindingTarget::Source(source), _) | (_, Some(source)) => source.clone(),
                    (BindingTarget::Pending { .. }, None) => {
                        self.wait_resolution(writer, event);
                        return Ok(());
                    }
                };
                self.bind(
                    writer,
                    event,
                    old.as_ref(),
                    new,
                    *cause,
                    parent_hint.as_ref(),
                )?;
            }
            let moved = writer.store().set_binding_checkpoint(event.seq);
            moved.map_err(halted("binding checkpoint"))?;
            self.checkpoint = Some(event.seq);
            expected += 1;
        }
        Ok(())
    }

    /// Without a checkpoint, the last bind of a source attached at the switch is where O starts.
    fn seed<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<Option<u64>, WriterAlarm> {
        let Ok(events) = self.bindings.binding_events_since(self.channel, 0) else {
            return Ok(None);
        };
        let store = writer.store();
        let attached = |source: &SourceId| store.cursor(source).is_some();
        let seq = match events.last() {
            None => 0,
            Some(_) => events
                .iter()
                .rev()
                .find(|event| bound_source(event).is_some_and(attached))
                .map(|event| event.seq)
                .ok_or_else(|| halt("the binding log names no source bound at the switch"))?,
        };
        let seeded = writer.store().set_binding_checkpoint(seq);
        seeded.map_err(halted("binding checkpoint"))?;
        self.checkpoint = Some(seq);
        Ok(Some(seq))
    }

    fn wait_resolution<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &ChannelWriter<P, L, A>,
        event: &BindingEvent,
    ) {
        let waited = Utc::now() - event.committed_at;
        let limit = TimeDelta::seconds(PENDING_BIND_ALARM_SECS);
        if waited > limit && self.pending_alarmed != Some(event.seq) {
            self.pending_alarmed = Some(event.seq);
            writer.alarm(WriterAlarm::BindingPending { seq: event.seq });
        }
    }

    /// The link and successor are durable before the cursor, and both before the checkpoint.
    fn bind<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        event: &BindingEvent,
        old: Option<&SourceId>,
        new: SourceId,
        cause: BindingCause,
        parent_hint: Option<&SourceId>,
    ) -> Result<(), WriterAlarm> {
        let key = source_key(&new);
        let cursor = writer.store().cursor(&new).cloned();
        if cursor.is_none() && !self.rotation.links.contains_key(&key) {
            let store = writer.store();
            let parent =
                parent_hint.filter(|parent| **parent != new && store.cursor(parent).is_some());
            let boundary = match (cause, parent) {
                (BindingCause::Startup | BindingCause::Clear, _) => Boundary::Owed { from: 0 },
                (BindingCause::Unknown, _) | (_, None) => Boundary::Pending {
                    candidates: vec![0],
                },
                _ => Boundary::Undecided,
            };
            if matches!(boundary, Boundary::Pending { .. }) {
                let source = new.clone();
                writer.alarm(WriterAlarm::BoundaryPending { source });
            }
            let link = SourceLink {
                source: new.clone(),
                seq: event.seq,
                parent: parent.cloned(),
                committed_at: event.committed_at,
                boundary,
            };
            self.rotation.links.insert(key.clone(), link);
        }
        self.rotation.successors.remove(&key);
        if let Some(old) = old.filter(|old| **old != new) {
            if writer.store().cursor(old).is_none() {
                return Err(halt("a bind names an old source this channel never read"));
            }
            self.rotation
                .successors
                .insert(source_key(old), new.clone());
            if let Some(reader) = self.readers.iter_mut().find(|r| r.source == *old) {
                (reader.rotated_at, reader.growth_alarmed) = (Some(Instant::now()), false);
            }
        }
        let written = writer.store().write_rotation(&self.rotation);
        written.map_err(halted("rotation"))?;
        match cursor {
            None => {
                let attached = writer.store().attach_source(&new);
                attached.map_err(halted("attach"))?;
                let opened = SourceCapture::open(new.clone(), 0);
                let capture = opened.map_err(|error| halt(format!("bound source: {error}")))?;
                self.readers.push(Reader::new(new, Some(capture)));
                Ok(())
            }
            Some(cursor) if cursor.retired => self.unretire(writer, &new),
            Some(_) => Ok(()),
        }
    }

    fn unretire<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        source: &SourceId,
    ) -> Result<(), WriterAlarm> {
        let unset = writer.store().set_retired(source, false);
        unset.map_err(halted("retire"))?;
        let Some(reader) = self.readers.iter_mut().find(|r| r.source == *source) else {
            return Err(halt("an attached source has no reader"));
        };
        (reader.watch_until, reader.quiet) = (None, None);
        if reader.capture.is_none() {
            let cursor = writer.store().cursor(source).cloned();
            let cursor = cursor.ok_or_else(|| halt("retired source lost its cursor"))?;
            let opened = SourceCapture::open(source.clone(), cursor.captured_through);
            let capture = opened.map_err(|error| halt(format!("source reopen: {error}")))?;
            if capture.prefix_hash() != cursor.prefix_hash {
                return Err(halt("source bytes before the cursor changed"));
            }
            reader.capture = Some(capture);
        }
        Ok(())
    }

    /// Spools each read source in bind order, so an old tail is owed before its successor.
    pub fn capture<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut VecDeque<Derived>,
    ) -> Result<(), WriterAlarm> {
        for index in 0..self.readers.len() {
            let reader = &mut self.readers[index];
            let Some(capture) = reader
                .capture
                .as_mut()
                .filter(|_| reader.watch_until.is_none())
            else {
                continue;
            };
            let retried = reader.pending.is_some();
            let batch = match reader.pending.take() {
                Some(batch) => batch,
                None => match capture.poll(MAX_READ_BYTES) {
                    CaptureOutcome::Batch(batch) => batch,
                    CaptureOutcome::Anomaly(anomaly) => {
                        let (kind, detail) = (anomaly.kind, anomaly.detail);
                        return Err(halt(format!("source {kind:?}: {detail}")));
                    }
                },
            };
            match writer.store().append_spool(&batch, &capture.prefix_hash()) {
                Ok(()) => reader.captured_any |= !batch.records.is_empty(),
                Err(StoreError::SpoolFull) => {
                    if !retried {
                        writer.alarm(WriterAlarm::SpoolFull);
                    }
                    reader.pending = Some(batch);
                    continue;
                }
                Err(error) => return Err(halt(format!("spool append: {error:?}"))),
            }
            let key = source_key(&batch.source);
            for record in &batch.records {
                self.owe(deriver, owed, &key, record);
            }
        }
        self.flush(writer)
    }

    /// Derives a record unless the source's boundary withholds it or its parent already has it.
    fn owe(
        &mut self,
        deriver: &mut UnitDeriver,
        owed: &mut VecDeque<Derived>,
        key: &str,
        record: &CapturedRecord,
    ) {
        let Some(link) = self.rotation.links.get(key).cloned() else {
            owed.extend(deriver.derive(record));
            return;
        };
        match link.boundary {
            Boundary::Pending { .. } => {}
            Boundary::Owed { from } => {
                if record.start >= from {
                    owed.extend(deriver.derive(record));
                }
            }
            Boundary::Undecided => {
                let keys = record_keys(self.provider, &record.line);
                if keys.is_some_and(|keys| keys.is_empty())
                    || self.inherited(&link, record, deriver)
                {
                    return;
                }
                let earliest = link.committed_at - TimeDelta::seconds(BIND_SLACK_SECS);
                let fresh = record_time(&record.line).is_some_and(|at| at >= earliest);
                let proven = link
                    .parent
                    .as_ref()
                    .is_some_and(|p| self.lineage_of(p).is_some());
                let boundary = if fresh && proven {
                    owed.extend(deriver.derive(record));
                    Boundary::Owed { from: record.start }
                } else {
                    let source = link.source.clone();
                    self.raised.push(WriterAlarm::BoundaryPending { source });
                    Boundary::Pending {
                        candidates: vec![0, record.start],
                    }
                };
                if let Some(link) = self.rotation.links.get_mut(key) {
                    link.boundary = boundary;
                }
                self.dirty = true;
            }
        }
    }

    /// Every unit key of the record is in the parent's lineage or already derived here.
    fn inherited(
        &mut self,
        link: &SourceLink,
        record: &CapturedRecord,
        deriver: &UnitDeriver,
    ) -> bool {
        let Some(parent) = link.parent.as_ref() else {
            return false;
        };
        let Some(keys) = record_keys(self.provider, &record.line) else {
            return false;
        };
        let Some(lineage) = self.lineage_of(parent) else {
            return false;
        };
        !keys.is_empty()
            && keys
                .iter()
                .all(|key| lineage.contains(key) || deriver.knows(&key.0, key.1))
    }

    fn lineage_of(&mut self, parent: &SourceId) -> Option<&Keys> {
        let provider = self.provider;
        let scanned = self.lineage.entry(source_key(parent));
        scanned
            .or_insert_with(|| scan_lineage(provider, parent))
            .as_ref()
    }

    fn flush<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<(), WriterAlarm> {
        if std::mem::take(&mut self.dirty) {
            let written = writer.store().write_rotation(&self.rotation);
            written.map_err(halted("rotation"))?;
        }
        self.raised.drain(..).for_each(|alarm| writer.alarm(alarm));
        Ok(())
    }

    /// Retires quiet drained old sources, alarms on growth and reader count, and watches retired ones.
    pub fn tend<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<(), WriterAlarm> {
        let now = Instant::now();
        let count = self.readers.iter().filter(|r| r.reading()).count();
        if count > MAX_READERS && !self.readers_alarmed {
            writer.alarm(WriterAlarm::TooManyReaders { count });
        }
        self.readers_alarmed = count > MAX_READERS;
        let captured = self.readers.iter().filter(|r| r.captured_any);
        let captured: HashSet<String> = captured.map(|r| source_key(&r.source)).collect();
        let mut grew_back = Vec::new();
        for reader in &mut self.readers {
            let Some(capture) = reader.capture.as_ref() else {
                continue;
            };
            let Ok(len) = capture.file_len() else {
                continue;
            };
            let through = capture.captured_through();
            if let Some(until) = reader.watch_until {
                if len > through {
                    grew_back.push(reader.source.clone());
                } else if now >= until {
                    reader.capture = None;
                }
                continue;
            }
            let successor = self.rotation.successors.get(&source_key(&reader.source));
            let Some(successor) = successor else {
                continue;
            };
            let successor_captured = captured.contains(&source_key(successor));
            let drained = through == len && reader.pending.is_none();
            let grew = reader.quiet.is_some_and(|(held, _)| held != len);
            let late = reader
                .rotated_at
                .is_some_and(|at| now - at > OLD_GROWTH_ALARM);
            if grew && late && !reader.growth_alarmed {
                reader.growth_alarmed = true;
                let source = reader.source.clone();
                writer.alarm(WriterAlarm::SourceStillGrowing { source });
            }
            let since = match reader.quiet {
                Some((held, since)) if held == len && successor_captured => since,
                _ => now,
            };
            reader.quiet = Some((len, since));
            if successor_captured && drained && now - since >= RETIRE_QUIET {
                let retired = writer.store().set_retired(&reader.source, true);
                retired.map_err(halted("retire"))?;
                (reader.watch_until, reader.quiet) = (Some(now + RETIRED_WATCH), None);
            }
        }
        for source in grew_back {
            self.unretire(writer, &source)?;
            writer.alarm(WriterAlarm::RetiredSourceGrew { source });
        }
        Ok(())
    }

    /// Sources whose segments may be collected, with how many to keep; undecided and pending
    /// boundaries keep their spool.
    pub fn collectable(&self) -> Vec<(SourceId, usize)> {
        let decided = |reader: &&Reader| {
            let link = self.rotation.link(&reader.source);
            link.is_none_or(|link| matches!(link.boundary, Boundary::Owed { .. }))
        };
        let keep = |reader: &Reader| usize::from(reader.pending.is_none());
        let readers = self.readers.iter().filter(decided);
        readers.map(|r| (r.source.clone(), keep(r))).collect()
    }
}
