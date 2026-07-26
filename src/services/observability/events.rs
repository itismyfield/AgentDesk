//! Bounded in-memory structured event log with periodic JSONL flush.
//!
//! Introduced by #1070. Complements the heavier SQL-backed event path already
//! in `observability::mod` by providing a lock-light ring buffer suitable for
//! very cheap hot-path writes and quick inspection via
//! `/api/analytics/observability`.
//!
//! The buffer is bounded (`MAX_EVENTS`) and retains the most recent history.
//! Evicting history already confirmed on disk is normal retention; only
//! overwriting an event that has not been confirmed flushed is counted as loss.
//! A background task (spawned by `ensure_flusher`) flushes new events to
//! `~/.adk/release/logs/observability-events.jsonl` every 60 seconds.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// Capacity of the in-memory ring buffer (per issue spec: last N=10000).
pub const MAX_EVENTS: usize = 10_000;
/// Background flush interval.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// Structured event record. Callers provide a free-form JSON `payload`; the
/// infrastructure timestamps with milliseconds since the Unix epoch.
#[derive(Debug, Clone, Serialize)]
pub struct StructuredEvent {
    pub event_type: String,
    pub channel_id: Option<u64>,
    pub provider: Option<String>,
    pub timestamp_ms: i64,
    pub payload: Value,
}

impl StructuredEvent {
    pub fn new(
        event_type: impl Into<String>,
        channel_id: Option<u64>,
        provider: Option<&str>,
        payload: Value,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            channel_id,
            provider: provider.map(|p| p.trim().to_ascii_lowercase()),
            timestamp_ms: now_millis(),
            payload,
        }
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Buffer contents, sequence allocation, durability state, and loss counters
/// move under one mutex so snapshots and acknowledgements observe one order.
#[derive(Debug)]
struct SequencedEvent {
    sequence: u64,
    event: StructuredEvent,
    flushed: bool,
}

#[derive(Debug, Default)]
struct EventLogInner {
    buffer: VecDeque<SequencedEvent>,
    next_sequence: u64,
    /// Events overwritten before their sequence was confirmed durable.
    dropped_total: u64,
    /// Confirmed-durable history removed solely to retain the newest events.
    retention_evicted_total: u64,
}

/// Immutable flush snapshot. A caller may acknowledge `flushed_through` only
/// after every event in `events` has been written and the writer has flushed.
#[derive(Debug, Clone)]
pub struct FlushBatch {
    events: Vec<StructuredEvent>,
    first_sequence: u64,
    flushed_through: u64,
}

impl FlushBatch {
    pub fn events(&self) -> &[StructuredEvent] {
        &self.events
    }
}

/// Bounded ring buffer for structured events.
#[derive(Debug)]
pub struct EventLog {
    capacity: usize,
    inner: Mutex<EventLogInner>,
}

impl EventLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(EventLogInner {
                buffer: VecDeque::with_capacity(capacity.max(1)),
                ..EventLogInner::default()
            }),
        }
    }

    pub fn push(&self, event: StructuredEvent) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        self.push_locked(&mut inner, event);
    }

    fn push_locked(&self, inner: &mut EventLogInner, event: StructuredEvent) {
        let Some(sequence) = inner.next_sequence.checked_add(1) else {
            inner.dropped_total = inner.dropped_total.saturating_add(1);
            tracing::warn!(
                "[observability] event sequence exhausted; dropping new event (total_dropped={})",
                inner.dropped_total,
            );
            return;
        };
        if inner.buffer.len() == self.capacity {
            let evicted = inner
                .buffer
                .pop_front()
                .expect("a full event ring must have a front entry");
            if evicted.flushed {
                inner.retention_evicted_total = inner.retention_evicted_total.saturating_add(1);
            } else {
                inner.dropped_total = inner.dropped_total.saturating_add(1);
                tracing::warn!(
                    sequence = evicted.sequence,
                    capacity = self.capacity,
                    total_dropped = inner.dropped_total,
                    "[observability] event ring overwrote unflushed event",
                );
            }
        }
        inner.buffer.push_back(SequencedEvent {
            sequence,
            event,
            flushed: false,
        });
        inner.next_sequence = sequence;
    }

    /// Return up to `limit` most recent events (newest last).
    pub fn recent(&self, limit: usize) -> Vec<StructuredEvent> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let len = inner.buffer.len();
        let take = limit.min(len);
        inner
            .buffer
            .iter()
            .skip(len - take)
            .map(|entry| entry.event.clone())
            .collect()
    }

    /// Snapshot every retained event not yet confirmed flushed.
    /// Concurrent pushes receive higher sequences and are excluded until the
    /// next snapshot. Failed or partial writes must leave this batch unacked.
    pub fn snapshot_unflushed(&self) -> Option<FlushBatch> {
        let inner = self.inner.lock().ok()?;
        let unflushed: Vec<&SequencedEvent> =
            inner.buffer.iter().filter(|entry| !entry.flushed).collect();
        let flushed_through = unflushed.last()?.sequence;
        let first_sequence = unflushed.first()?.sequence;
        Some(FlushBatch {
            events: unflushed
                .into_iter()
                .map(|entry| entry.event.clone())
                .collect(),
            first_sequence,
            flushed_through,
        })
    }

    /// Mark only the events represented by a completed immutable snapshot.
    /// Duplicate or out-of-order acknowledgements are harmless, including
    /// concurrent flushers that snapshot overlapping ranges.
    pub fn acknowledge_flushed(&self, batch: &FlushBatch) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for entry in &mut inner.buffer {
            if entry.sequence >= batch.first_sequence && entry.sequence <= batch.flushed_through {
                entry.flushed = true;
            }
        }
    }

    /// Compatibility metric: only events lost before JSONL confirmation.
    #[allow(dead_code)]
    pub fn dropped_total(&self) -> u64 {
        self.inner.lock().map(|i| i.dropped_total).unwrap_or(0)
    }

    /// Low-cardinality count of normal confirmed-history retention eviction.
    #[allow(dead_code)]
    pub fn retention_evicted_total(&self) -> u64 {
        self.inner
            .lock()
            .map(|i| i.retention_evicted_total)
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn with_next_sequence_for_test(capacity: usize, next_sequence: u64) -> Self {
        let log = Self::new(capacity);
        log.inner
            .lock()
            .expect("test event log mutex should be available")
            .next_sequence = next_sequence;
        log
    }

    #[cfg(test)]
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.buffer.clear();
            inner.next_sequence = 0;
            inner.dropped_total = 0;
            inner.retention_evicted_total = 0;
        }
    }
}

static GLOBAL_EVENT_LOG: OnceLock<Arc<EventLog>> = OnceLock::new();
static FLUSHER_STARTED: OnceLock<()> = OnceLock::new();

pub fn global() -> Arc<EventLog> {
    GLOBAL_EVENT_LOG
        .get_or_init(|| Arc::new(EventLog::new(MAX_EVENTS)))
        .clone()
}

pub fn record(event: StructuredEvent) {
    global().push(event);
}

pub(super) fn record_emitted(
    event_type: &str,
    channel_id: Option<u64>,
    provider: Option<&str>,
    payload: Value,
) {
    record(StructuredEvent::new(
        event_type, channel_id, provider, payload,
    ));
}

pub fn record_simple(
    event_type: &str,
    channel_id: Option<u64>,
    provider: Option<&str>,
    payload: Value,
) {
    global().push(StructuredEvent::new(
        event_type.to_string(),
        channel_id,
        provider,
        payload,
    ));
}

pub fn recent(limit: usize) -> Vec<StructuredEvent> {
    global().recent(limit)
}

/// Flush target path. Honors `ADK_OBSERVABILITY_EVENTS_PATH` for tests.
pub fn flush_target_path() -> PathBuf {
    if let Ok(override_path) = std::env::var("ADK_OBSERVABILITY_EVENTS_PATH") {
        if !override_path.trim().is_empty() {
            return PathBuf::from(override_path);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".adk")
        .join("release")
        .join("logs")
        .join("observability-events.jsonl")
}

fn write_events<W: std::io::Write>(
    writer: &mut W,
    events: &[StructuredEvent],
) -> std::io::Result<()> {
    for ev in events {
        let line = serde_json::to_string(ev)
            .unwrap_or_else(|_| "{\"event_type\":\"_serialize_error\"}".to_string());
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

/// Append the given events to the JSONL target and flush the userspace writer.
/// The caller may advance its durability watermark only after this returns.
pub fn flush_events_to_disk(events: &[StructuredEvent]) -> std::io::Result<()> {
    use std::fs::{OpenOptions, create_dir_all};
    if events.is_empty() {
        return Ok(());
    }
    let path = flush_target_path();
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    write_events(&mut file, events)
}

fn flush_once_with<F>(log: &EventLog, persist: F) -> std::io::Result<usize>
where
    F: FnOnce(&[StructuredEvent]) -> std::io::Result<()>,
{
    let Some(batch) = log.snapshot_unflushed() else {
        return Ok(0);
    };
    persist(batch.events())?;
    let count = batch.events().len();
    log.acknowledge_flushed(&batch);
    Ok(count)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Barrier};

    use serde_json::json;

    use super::*;

    fn event(name: impl Into<String>) -> StructuredEvent {
        StructuredEvent::new(name, None, None, json!({}))
    }

    fn event_names(events: &[StructuredEvent]) -> Vec<&str> {
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect()
    }

    #[test]
    fn confirmed_history_eviction_is_retention_not_loss() {
        let log = EventLog::new(2);
        log.push(event("e1"));
        log.push(event("e2"));
        assert_eq!(flush_once_with(&log, |_| Ok(())).unwrap(), 2);

        log.push(event("e3"));

        assert_eq!(log.dropped_total(), 0);
        assert_eq!(log.retention_evicted_total(), 1);
        assert_eq!(event_names(&log.recent(10)), vec!["e2", "e3"]);
        assert_eq!(
            event_names(log.snapshot_unflushed().unwrap().events()),
            vec!["e3"]
        );
    }

    #[test]
    fn failed_flush_does_not_confirm_and_overwrite_counts_loss() {
        let log = EventLog::new(2);
        log.push(event("e1"));
        log.push(event("e2"));

        let error = flush_once_with(&log, |_| Err(io::Error::other("injected failure")))
            .expect_err("failed persistence must be returned");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        log.push(event("e3"));

        assert_eq!(log.dropped_total(), 1);
        assert_eq!(log.retention_evicted_total(), 0);
        assert_eq!(
            event_names(log.snapshot_unflushed().unwrap().events()),
            vec!["e2", "e3"]
        );
    }

    struct PartialWriter {
        remaining: usize,
        bytes: Vec<u8>,
    }

    impl Write for PartialWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "injected partial write",
                ));
            }
            let written = self.remaining.min(buf.len());
            self.bytes.extend_from_slice(&buf[..written]);
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_jsonl_write_does_not_advance_confirmation() {
        let log = EventLog::new(2);
        log.push(event("event-with-enough-bytes"));
        let mut writer = PartialWriter {
            remaining: 8,
            bytes: Vec::new(),
        };

        assert!(flush_once_with(&log, |events| write_events(&mut writer, events)).is_err());
        assert_eq!(
            event_names(log.snapshot_unflushed().unwrap().events()),
            vec!["event-with-enough-bytes"]
        );
    }

    #[test]
    fn concurrent_push_after_snapshot_is_not_acknowledged() {
        let log = Arc::new(EventLog::new(4));
        log.push(event("e1"));
        let persisted = Arc::new(Barrier::new(2));
        let allow_return = Arc::new(Barrier::new(2));
        let worker_log = Arc::clone(&log);
        let worker_persisted = Arc::clone(&persisted);
        let worker_allow_return = Arc::clone(&allow_return);
        let worker = std::thread::spawn(move || {
            flush_once_with(&worker_log, |_| {
                worker_persisted.wait();
                worker_allow_return.wait();
                Ok(())
            })
        });

        persisted.wait();
        log.push(event("e2"));
        allow_return.wait();
        assert_eq!(worker.join().unwrap().unwrap(), 1);

        assert_eq!(
            event_names(log.snapshot_unflushed().unwrap().events()),
            vec!["e2"]
        );
    }

    #[test]
    fn overlapping_out_of_order_acknowledgements_mark_only_their_snapshots() {
        let log = EventLog::new(4);
        log.push(event("e1"));
        let first = log.snapshot_unflushed().unwrap();
        log.push(event("e2"));
        let second = log.snapshot_unflushed().unwrap();

        log.acknowledge_flushed(&second);
        log.acknowledge_flushed(&first);
        log.acknowledge_flushed(&second);
        assert!(log.snapshot_unflushed().is_none());

        log.push(event("e3"));
        assert_eq!(
            event_names(log.snapshot_unflushed().unwrap().events()),
            vec!["e3"]
        );
    }

    #[test]
    fn restart_initializes_without_claiming_existing_jsonl_history() {
        let previous = EventLog::new(2);
        previous.push(event("before-restart"));
        assert_eq!(flush_once_with(&previous, |_| Ok(())).unwrap(), 1);

        let restarted = EventLog::new(2);
        restarted.push(event("after-restart"));
        assert_eq!(
            event_names(restarted.snapshot_unflushed().unwrap().events()),
            vec!["after-restart"]
        );
        assert_eq!(restarted.dropped_total(), 0);
        assert_eq!(restarted.retention_evicted_total(), 0);
    }

    #[test]
    fn sequence_exhaustion_drops_new_event_without_aliasing() {
        let log = EventLog::with_next_sequence_for_test(2, u64::MAX - 1);
        log.push(event("last-sequence"));
        log.push(event("overflow"));

        assert_eq!(event_names(&log.recent(10)), vec!["last-sequence"]);
        assert_eq!(log.dropped_total(), 1);
        assert_eq!(
            event_names(log.snapshot_unflushed().unwrap().events()),
            vec!["last-sequence"]
        );
    }

    #[test]
    fn mutation_guard_unacknowledged_success_still_counts_overwrite_as_loss() {
        let log = EventLog::new(1);
        log.push(event("e1"));
        let batch = log.snapshot_unflushed().unwrap();
        assert_eq!(event_names(batch.events()), vec!["e1"]);

        log.push(event("e2"));

        assert_eq!(log.dropped_total(), 1);
        assert_eq!(log.retention_evicted_total(), 0);
    }
}

/// #2049 Finding 1: Dead-letter JSONL dump for event batches that failed to
/// flush to PostgreSQL. Output lives in the same logs directory as
/// `flush_target_path()`, with the file name
/// `observability-<suffix>-dlq.jsonl` so operators can grep by event family.
/// Callers pass arbitrary serializable rows so this helper can be reused for
/// both `observability_events` and `agent_quality_event` batches.
pub fn flush_dead_letter_jsonl<T: serde::Serialize>(
    suffix: &str,
    rows: &[T],
) -> std::io::Result<()> {
    use std::fs::{OpenOptions, create_dir_all};
    use std::io::Write;
    if rows.is_empty() {
        return Ok(());
    }
    let base = flush_target_path();
    let parent = base.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".adk")
            .join("release")
            .join("logs")
    });
    let _ = create_dir_all(&parent);
    let file_name = format!("observability-{suffix}-dlq.jsonl");
    let path = parent.join(file_name);
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    for row in rows {
        let line = serde_json::to_string(row)
            .unwrap_or_else(|_| "{\"_serialize_error\":true}".to_string());
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

/// Spawn the background flush task (idempotent).
pub fn ensure_flusher() {
    // Do not consume the one-shot start guard until a runtime can accept the
    // task; a later initialization call must still be able to start flushing.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if FLUSHER_STARTED.set(()).is_err() {
        return;
    }
    let log = global();
    handle.spawn(async move {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the first immediate tick.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) = flush_once_with(&log, flush_events_to_disk) {
                tracing::warn!(%error, "observability events flush failed");
            }
        }
    });
}
