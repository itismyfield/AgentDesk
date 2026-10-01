//! The sources one channel reads. A bind attaches its new source at offset 0 and the old one is
//! read until it provably stops; a resumed or forked source skips what its parent already holds.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use tokio::sync::watch;
use tokio::time::Instant;

use super::binding::{BindingCause, BindingEvent, BindingEvents, BindingRecord, BindingTarget};
use super::deliver::ChannelWriter;
use super::pieces::{Derived, UnitDeriver};
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm};
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use crate::services::tui_o::shadow::{
    CaptureBatch, CaptureOutcome, CaptureSource, CapturedRecord, MAX_READ_BYTES, ShadowProvider,
    SourceId, UnitKind,
};
use crate::services::tui_o::store::StoreError;
use crate::services::tui_o::store::rotation::{Boundary, Rotation, SourceLink, Successor};
use crate::services::tui_o::store::spool::{SpoolFrame, source_key};

/// A proven old source whose length holds this long after its successor's first record is retired.
pub const RETIRE_QUIET: Duration = Duration::from_secs(10);
/// An old source still growing this long after its rotation alarms; both stay read.
pub const OLD_GROWTH_ALARM: Duration = Duration::from_secs(600);
/// A retired source is watched this long; growth un-retires it.
pub const RETIRED_WATCH: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_READERS: usize = 3;
/// A parent consumed past this is not scanned, so a fork of it waits for the operator.
pub const LINEAGE_SCAN_CAP_BYTES: u64 = 256 << 20;
const PENDING_BIND_ALARM_SECS: i64 = 60;

#[path = "fork_lineage.rs"]
mod fork_lineage;
use fork_lineage::{Class, Lineage, RowIds};

/// How a record names a native key: as the unit itself or as an announcement of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Seen {
    Sealed,
    Announced,
}

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
    /// The old source's length at its rotation; its successor waits until it is read and spooled.
    drain_to: Option<u64>,
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
            drain_to: None,
        }
    }

    fn reading(&self) -> bool {
        self.capture.is_some() && self.watch_until.is_none()
    }

    fn backlog_end(&self) -> Option<u64> {
        let capture = self.capture.as_ref().filter(|_| self.watch_until.is_none());
        capture.and_then(|capture| capture.file_len().ok())
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
    /// Parent identities by spool key, kept at the parent's durable cursor.
    lineage: HashMap<String, Lineage>,
    /// Undecided sources that have shown at least one inherited row.
    prefix_seen: HashSet<String>,
    /// Boundary decisions and alarms made while deriving, flushed before delivery.
    dirty: bool,
    raised: Vec<WriterAlarm>,
    readers_alarmed: bool,
    pending_alarmed: Option<u64>,
    /// Set while the binding log cannot be read, so the outage alarms once.
    log_alarmed: bool,
    /// Hop seq and pane of successor records written without them, as the binding log names them.
    legacy_hops: HashMap<String, (u64, String)>,
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
fn record_keys(provider: ShadowProvider, line: &[u8]) -> Option<Vec<(String, UnitKind, Seen)>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Some(Vec::new());
    }
    let value = serde_json::from_slice(line).ok()?;
    let mut keys = Vec::new();
    for fact in classify(provider, &value) {
        match fact {
            RecordFact::Unit(key, kind, _) => keys.push((key, kind, Seen::Sealed)),
            RecordFact::Announced(key, kind) => keys.push((key, kind, Seen::Announced)),
            RecordFact::Blocked(_) => return None,
            _ => {}
        }
    }
    Some(keys)
}

/// How many successor links lead back from a source; a predecessor's spool replays before it.
fn predecessors(rotation: &Rotation, source: &SourceId) -> usize {
    let (mut key, mut count) = (source_key(source), 0);
    while count < rotation.successors.len() {
        let Some(prev) = rotation
            .successors
            .iter()
            .find(|(_, next)| source_key(&next.source) == key)
        else {
            break;
        };
        (key, count) = (prev.0.clone(), count + 1);
    }
    count
}

/// The session a provider record moves its pane to, when the provider sends that record only on
/// leaving the session the pane ran; no other record shows an old source stopped being written.
fn proving_session(event: &BindingEvent) -> Option<&str> {
    let BindingRecord::Bound {
        new,
        cause,
        evidence,
        ..
    } = &event.record
    else {
        return None;
    };
    if evidence.hook_event != HookEventKind::SessionStart.as_str() {
        return None;
    }
    let leaves = match event.provider {
        ShadowProvider::Claude => {
            matches!(
                cause,
                BindingCause::Clear | BindingCause::Resume | BindingCause::Fork
            )
        }
        ShadowProvider::Codex => *cause == BindingCause::Clear,
    };
    leaves.then(|| match new {
        BindingTarget::Source(source) => source.session_id.as_str(),
        BindingTarget::Pending {
            payload_session_id, ..
        } => payload_session_id.as_str(),
    })
}

/// The old and new source of a hop O applied for `event`: its own source, or the one the
/// `Resolved` of its Pending names.
fn hop<'a>(
    event: &'a BindingEvent,
    events: &'a [BindingEvent],
) -> Option<(&'a SourceId, &'a SourceId)> {
    let BindingRecord::Bound {
        old: Some(old),
        new,
        ..
    } = &event.record
    else {
        return None;
    };
    let new = match new {
        BindingTarget::Source(source) => source,
        BindingTarget::Pending { .. } => events.iter().find_map(|later| match &later.record {
            BindingRecord::Resolved {
                resolves_seq,
                source,
            } if *resolves_seq == event.seq => Some(source),
            _ => None,
        })?,
    };
    (old != new).then_some((old, new))
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

/// The last event binding a source `attached` accepts; the binding checkpoint starts there.
pub fn binding_baseline(
    events: &[BindingEvent],
    attached: impl Fn(&SourceId) -> bool,
) -> Option<u64> {
    let bound = events
        .iter()
        .rev()
        .find(|e| bound_source(e).is_some_and(&attached));
    bound.map(|event| event.seq)
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
            prefix_seen: HashSet::new(),
            dirty: false,
            raised: Vec::new(),
            readers_alarmed: false,
            pending_alarmed: None,
            log_alarmed: false,
            legacy_hops: HashMap::new(),
        }
    }

    /// Applies operator-resolved boundaries, re-derives every retained spool with predecessors
    /// first, then reopens each source at its cursor.
    pub fn resume<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut VecDeque<Derived>,
    ) -> Result<(), WriterAlarm> {
        let resolved = writer.store().apply_resolved_boundaries();
        resolved.map_err(halted("resolved boundary"))?;
        self.rotation = writer.store().rotation().map_err(halted("rotation"))?;
        let checkpoint = writer.store().binding_checkpoint();
        self.checkpoint = checkpoint.map_err(halted("binding checkpoint"))?;
        let mut cursors: Vec<_> = writer.store().cursors().cloned().collect();
        cursors.sort_by_key(|cursor| {
            let seq = self.rotation.link(&cursor.source).map_or(0, |l| l.seq);
            (predecessors(&self.rotation, &cursor.source), seq)
        });
        let now = Instant::now();
        let consumed: HashMap<String, u64> = (cursors.iter())
            .map(|cursor| (source_key(&cursor.source), cursor.captured_through))
            .collect();
        for cursor in cursors {
            let (key, mut captured_any) = (source_key(&cursor.source), false);
            self.sync_parent(&key, |parent| consumed.get(&source_key(parent)).copied());
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
            reader.drain_to = match self.rotation.successors.get(&key) {
                // A record written before the rotation length was kept holds to the length now.
                Some(next) if next.seq.is_none() => reader.backlog_end(),
                Some(next) => next.drain_to.filter(|end| cursor.captured_through < *end),
                None => None,
            };
            // A retired reader is never captured again, so its prefix-only end is judged here.
            let ended = (reader.capture.as_ref())
                .is_none_or(|capture| capture.file_len().ok() == Some(cursor.captured_through));
            if reader.watch_until.is_some() && ended {
                self.pend_at_end(&key, cursor.captured_through);
            }
            self.readers.push(reader);
        }
        self.restore_legacy_hops();
        self.flush(writer)
    }

    /// Takes each unproven record's missing pane and seq from the last applied hop of its old
    /// source; a record that hop does not match stays unproven.
    fn restore_legacy_hops(&mut self) {
        let legacy = |next: &Successor| next.seq.is_none() && next.proof.is_none();
        let Some(checkpoint) = self.checkpoint else {
            return;
        };
        if !self.rotation.successors.values().any(legacy) {
            return;
        }
        let Ok(events) = self.bindings.binding_events_since(self.channel, 0) else {
            return;
        };
        let applied = &events[..events.partition_point(|e| e.seq <= checkpoint)];
        for (key, next) in self.rotation.successors.iter().filter(|(_, n)| legacy(n)) {
            let last = applied.iter().rev().find_map(|e| {
                hop(e, &events)
                    .filter(|(old, _)| source_key(old) == *key)
                    .map(|h| (e, h))
            });
            if let Some((event, _)) = last.filter(|(_, (_, new))| **new == next.source) {
                let pane = (event.seq, event.tmux_session.clone());
                self.legacy_hops.insert(key.clone(), pane);
            }
        }
    }

    /// Applies binding events past the checkpoint in seq order, stopping at an unresolved bind that
    /// no later hook superseded.
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
        let Some(events) = self.read_log(writer, checkpoint) else {
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
        let pending = events.iter().filter(|event| {
            let target = match &event.record {
                BindingRecord::Bound { new, .. } => Some(new),
                _ => None,
            };
            matches!(target, Some(BindingTarget::Pending { .. }))
        });
        let superseded: HashSet<u64> = pending
            .filter(|p| {
                events
                    .iter()
                    .any(|later| super::adoption::supersedes(p, later))
            })
            .map(|p| p.seq)
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
                    (BindingTarget::Source(source), _) | (_, Some(source)) => Some(source.clone()),
                    // Nothing resolves a superseded Pending; the hook that superseded it binds.
                    (BindingTarget::Pending { .. }, None) if superseded.contains(&event.seq) => {
                        None
                    }
                    (BindingTarget::Pending { .. }, None) => {
                        self.wait_resolution(writer, event);
                        return Ok(());
                    }
                };
                if let Some(new) = new {
                    let (old, parent) = (old.as_ref(), parent_hint.as_ref());
                    self.bind(writer, event, old, new, *cause, parent)?;
                }
                self.prove(writer, event)?;
            }
            let moved = writer.store().set_binding_checkpoint(event.seq);
            moved.map_err(halted("binding checkpoint"))?;
            self.checkpoint = Some(event.seq);
            expected += 1;
        }
        Ok(())
    }

    /// Reads binding events past `after`; a failed read alarms once per outage and applies nothing.
    fn read_log<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &ChannelWriter<P, L, A>,
        after: u64,
    ) -> Option<Vec<BindingEvent>> {
        match self.bindings.binding_events_since(self.channel, after) {
            Ok(events) => {
                self.log_alarmed = false;
                Some(events)
            }
            Err(detail) => {
                if !std::mem::replace(&mut self.log_alarmed, true) {
                    let checkpoint = self.checkpoint;
                    writer.alarm(WriterAlarm::BindingLogUnavailable { checkpoint, detail });
                }
                None
            }
        }
    }

    /// Without a checkpoint, the last bind of a source attached at the switch is where O starts;
    /// a log naming none has no baseline, and nothing is captured until one is seeded.
    fn seed<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<Option<u64>, WriterAlarm> {
        let Some(events) = self.read_log(writer, 0) else {
            return Ok(None);
        };
        let store = writer.store();
        let seq = binding_baseline(&events, |source| store.cursor(source).is_some()).ok_or_else(
            || halt("no binding baseline: no event binds a source attached at the switch"),
        )?;
        let seeded = writer.store().set_binding_checkpoint(seq);
        seeded.map_err(halted("binding checkpoint"))?;
        self.checkpoint = Some(seq);
        Ok(Some(seq))
    }

    /// Marks each unproven hop of the record's pane that it shows was left, durably before the
    /// checkpoint passes the record.
    fn prove<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        event: &BindingEvent,
    ) -> Result<(), WriterAlarm> {
        let Some(session) = proving_session(event) else {
            return Ok(());
        };
        let mut proved = false;
        for (key, next) in &mut self.rotation.successors {
            let made = match (next.seq, next.tmux_session.as_deref()) {
                (Some(seq), Some(tmux)) => Some((seq, tmux)),
                _ => (self.legacy_hops.get(key)).map(|(seq, tmux)| (*seq, tmux.as_str())),
            };
            let old = self.readers.iter().find(|r| source_key(&r.source) == *key);
            let left = old.is_some_and(|old| old.source.session_id != session);
            let ours =
                made.is_some_and(|(seq, tmux)| seq <= event.seq && tmux == event.tmux_session);
            if next.proof.is_none() && ours && left {
                next.proof = Some(event.seq);
                proved = true;
            }
        }
        if proved {
            let written = writer.store().write_rotation(&self.rotation);
            written.map_err(halted("rotation"))?;
        }
        Ok(())
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
            let old_key = source_key(old);
            let reader = self.readers.iter_mut().find(|r| r.source == *old);
            // A record applied again after a crash keeps what its first application measured.
            let kept = (self.rotation.successors.get(&old_key))
                .filter(|next| next.seq == Some(event.seq) && next.source == new)
                .cloned();
            let next = kept.unwrap_or_else(|| Successor {
                source: new.clone(),
                seq: Some(event.seq),
                tmux_session: Some(event.tmux_session.clone()),
                drain_to: reader.as_ref().and_then(|reader| reader.backlog_end()),
                proof: None,
            });
            if let Some(reader) = reader {
                (reader.rotated_at, reader.growth_alarmed) = (Some(Instant::now()), false);
                reader.drain_to = next.drain_to;
            }
            self.rotation.successors.insert(old_key, next);
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

    /// Spools each read source once. A successor waits while a predecessor is still short of its
    /// rotation-time length, so that backlog is owed first whatever the reader order.
    pub fn capture<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut VecDeque<Derived>,
    ) -> Result<(), WriterAlarm> {
        if self.checkpoint.is_none() {
            return Ok(());
        }
        let mut left: Vec<usize> = (0..self.readers.len()).collect();
        while let Some(at) = left.iter().position(|&index| !self.held(index)) {
            let index = left.remove(at);
            self.capture_one(writer, deriver, owed, index)?;
        }
        self.flush(writer)?;
        if let Some(source) = self.stalled(deriver) {
            return Err(WriterAlarm::RotationStalled { source });
        }
        Ok(())
    }

    fn capture_one<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut VecDeque<Derived>,
        index: usize,
    ) -> Result<(), WriterAlarm> {
        let reader = &mut self.readers[index];
        let Some(capture) = reader
            .capture
            .as_mut()
            .filter(|_| reader.watch_until.is_none())
        else {
            reader.drain_to = None;
            return Ok(());
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
        let read_through = capture.read_through();
        let at_end = capture.file_len().ok() == Some(capture.captured_through());
        match writer.store().append_spool(&batch, &capture.prefix_hash()) {
            Ok(()) => reader.captured_any |= !batch.records.is_empty(),
            Err(StoreError::SpoolFull) => {
                if !retried {
                    writer.alarm(WriterAlarm::SpoolFull);
                }
                reader.pending = Some(batch);
                return Ok(());
            }
            Err(error) => return Err(halt(format!("spool append: {error:?}"))),
        }
        if !reader.drain_to.is_some_and(|end| read_through < end) {
            reader.drain_to = None;
        }
        let key = source_key(&batch.source);
        let store = writer.store();
        self.sync_parent(&key, |parent| {
            store.cursor(parent).map(|c| c.captured_through)
        });
        for record in &batch.records {
            self.owe(deriver, owed, &key, record);
        }
        if at_end {
            self.pend_at_end(&key, batch.captured_through);
        }
        Ok(())
    }

    /// A reading predecessor of this reader has not yet spooled through its rotation-time length.
    fn held(&self, index: usize) -> bool {
        let source = &self.readers[index].source;
        self.readers.iter().any(|reader| {
            reader.drain_to.is_some()
                && reader.reading()
                && (self.rotation.successors.get(&source_key(&reader.source)))
                    .is_some_and(|next| next.source == *source)
        })
    }

    /// An old source the full spool refuses while it holds a successor, with an announced unit
    /// keeping GC off: its sealing record may sit behind the barrier, so no step frees the spool.
    fn stalled(&self, deriver: &UnitDeriver) -> Option<SourceId> {
        if !deriver.has_unsealed() {
            return None;
        }
        let holds = |old: &Reader| {
            let successor = self.rotation.successors.get(&source_key(&old.source));
            let successor =
                successor.and_then(|s| self.readers.iter().find(|r| r.source == s.source));
            successor.is_some_and(Reader::reading)
        };
        let stuck = |old: &&Reader| old.pending.is_some() && old.drain_to.is_some() && holds(old);
        self.readers
            .iter()
            .find(stuck)
            .map(|old| old.source.clone())
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
                if record.start >= from && !self.masked(&link, record) {
                    owed.extend(deriver.derive(record));
                }
            }
            Boundary::Undecided => {
                let row = RowIds::of(self.provider, &record.line);
                if row.as_ref().is_some_and(RowIds::is_empty) {
                    return;
                }
                let parent = link.parent.as_ref();
                let lineage = parent.and_then(|parent| self.lineage.get(&source_key(parent)));
                let class = row.zip(lineage).map(|(row, lineage)| lineage.class(&row));
                match class {
                    Some(Class::Inherited) => {
                        self.prefix_seen.insert(key.to_owned());
                    }
                    Some(Class::New) if self.prefix_seen.contains(key) => {
                        owed.extend(deriver.derive(record));
                        self.decide(key, Boundary::Owed { from: record.start });
                    }
                    _ => self.pend(key, record.start),
                }
            }
        }
    }

    /// A row after a fork's start whose every identity the parent consumed is the parent's copy,
    /// unless the parent is pending or undecided and so may never post it.
    fn masked(&self, link: &SourceLink, record: &CapturedRecord) -> bool {
        let Some(parent) = link.parent.as_ref() else {
            return false;
        };
        let parent_link = self.rotation.link(parent).map(|l| &l.boundary);
        if !matches!(parent_link, None | Some(Boundary::Owed { .. })) {
            return false;
        }
        let Some(lineage) = self.lineage.get(&source_key(parent)) else {
            return false;
        };
        RowIds::of(self.provider, &record.line)
            .is_some_and(|row| !row.is_empty() && lineage.class(&row) == Class::Inherited)
    }

    /// Brings the parent identities a source is judged against to the parent's durable cursor.
    fn sync_parent(&mut self, key: &str, consumed: impl Fn(&SourceId) -> Option<u64>) {
        let Some(link) = self.rotation.links.get(key) else {
            return;
        };
        let Some(parent) = link.parent.clone() else {
            return;
        };
        let wanted = match &link.boundary {
            Boundary::Undecided => true,
            Boundary::Owed { .. } => matches!(
                self.rotation.link(&parent).map(|l| &l.boundary),
                None | Some(Boundary::Owed { .. })
            ),
            Boundary::Pending { .. } => false,
        };
        if !wanted {
            return;
        }
        let parent_key = source_key(&parent);
        let cached = self.lineage.remove(&parent_key);
        let through = consumed(&parent);
        let synced = through.and_then(|t| fork_lineage::sync(cached, self.provider, &parent, t));
        if let Some(lineage) = synced {
            self.lineage.insert(parent_key, lineage);
        }
    }

    /// A source still undecided once its file is read to the end has shown no new row.
    fn pend_at_end(&mut self, key: &str, end: u64) {
        let undecided = self.rotation.links.get(key);
        if undecided.is_some_and(|link| link.boundary == Boundary::Undecided) {
            self.pend(key, end);
        }
    }

    fn pend(&mut self, key: &str, at: u64) {
        if let Some(link) = self.rotation.links.get(key) {
            let source = link.source.clone();
            self.raised.push(WriterAlarm::BoundaryPending { source });
        }
        let mut candidates = vec![0, at];
        candidates.dedup();
        self.decide(key, Boundary::Pending { candidates });
    }

    fn decide(&mut self, key: &str, boundary: Boundary) {
        if let Some(link) = self.rotation.links.get_mut(key) {
            link.boundary = boundary;
        }
        self.prefix_seen.remove(key);
        self.dirty = true;
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

    /// Retires quiet drained old sources whose hop is proven, alarms on growth and reader count, and
    /// watches retired ones.
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
            let successor_captured = captured.contains(&source_key(&successor.source));
            // Only a provider record shows the old source stopped; a drained quiet one stays read.
            let proven = successor.proof.is_some();
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
            if proven && successor_captured && drained && now - since >= RETIRE_QUIET {
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
