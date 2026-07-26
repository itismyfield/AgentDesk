//! Bounded in-memory structured event log with periodic JSONL flush.
//!
//! Introduced by #1070. Complements the heavier SQL-backed event path already
//! in `observability::mod` by providing a lock-light ring buffer suitable for
//! very cheap hot-path writes and quick inspection via
//! `/api/analytics/observability`.
//!
//! The buffer is bounded (`MAX_EVENTS`) and retains the most recent history.
//! Evicting history already confirmed durable is normal retention; only
//! overwriting an event that has not been confirmed durable is counted as loss.
//! A background task (spawned by `ensure_flusher`) appends new events to
//! `~/.adk/release/logs/observability-events.jsonl` every 60 seconds.
//!
//! The JSONL file is at-least-once: a process crash after the kernel accepts an
//! append but before `sync_all` returns can make retry outcome ambiguous. Every
//! newly recorded event therefore carries a stable `event_id`; consumers that
//! need exactly-once interpretation must deduplicate by that additive field.
//! In-process failures roll the whole framed batch back under a file lock. If a
//! prior process crashed mid-record, the next append first writes a newline
//! delimiter so later complete records remain independently parseable.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
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
    /// Stable identity across persistence retries. This additive field lets
    /// JSONL consumers deduplicate the log's documented at-least-once writes.
    pub event_id: String,
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
            event_id: uuid::Uuid::new_v4().to_string(),
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

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct EventRingAccounting {
    /// Events overwritten or rejected before durable JSONL confirmation.
    pub dropped_total: u64,
    /// Confirmed-durable events removed solely by recent-history retention.
    pub retention_evicted_total: u64,
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
/// after every event in `events` has crossed the required durability boundary.
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

    /// Return low-cardinality ring accounting for the observability API.
    pub fn accounting(&self) -> EventRingAccounting {
        self.inner
            .lock()
            .map(|inner| EventRingAccounting {
                dropped_total: inner.dropped_total,
                retention_evicted_total: inner.retention_evicted_total,
            })
            .unwrap_or_default()
    }

    /// Compatibility metric: only events lost before JSONL confirmation.
    pub fn dropped_total(&self) -> u64 {
        self.accounting().dropped_total
    }

    /// Low-cardinality count of normal confirmed-history retention eviction.
    pub fn retention_evicted_total(&self) -> u64 {
        self.accounting().retention_evicted_total
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

fn serialize_batch(events: &[StructuredEvent]) -> std::io::Result<Vec<u8>> {
    let mut batch = Vec::new();
    for event in events {
        serde_json::to_writer(&mut batch, event).map_err(std::io::Error::other)?;
        batch.push(b'\n');
    }
    Ok(batch)
}

trait TransactionalEventFile: Write + Seek {
    fn len(&self) -> std::io::Result<u64>;
    fn truncate(&mut self, len: u64) -> std::io::Result<()>;
    fn sync_durable(&mut self) -> std::io::Result<()>;
    fn ends_with_newline(&mut self, len: u64) -> std::io::Result<bool>;
}

impl TransactionalEventFile for std::fs::File {
    fn len(&self) -> std::io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        self.set_len(len)
    }

    fn sync_durable(&mut self) -> std::io::Result<()> {
        self.sync_all()
    }

    fn ends_with_newline(&mut self, len: u64) -> std::io::Result<bool> {
        use std::io::Read;

        if len == 0 {
            return Ok(true);
        }
        self.seek(SeekFrom::Start(len - 1))?;
        let mut byte = [0_u8; 1];
        self.read_exact(&mut byte)?;
        Ok(byte[0] == b'\n')
    }
}

fn rollback_batch<W: TransactionalEventFile>(
    writer: &mut W,
    batch_start: u64,
) -> std::io::Result<()> {
    writer.truncate(batch_start)?;
    writer.seek(SeekFrom::Start(batch_start))?;
    writer.sync_durable()
}

fn append_transaction<W: TransactionalEventFile>(
    writer: &mut W,
    events: &[StructuredEvent],
) -> std::io::Result<()> {
    let batch = serialize_batch(events)?;
    let existing_len = writer.len()?;
    let needs_recovery_delimiter = !writer.ends_with_newline(existing_len)?;
    writer.seek(SeekFrom::End(0))?;
    let append_start = writer.stream_position()?;

    let result = (|| {
        if needs_recovery_delimiter {
            writer.write_all(b"\n")?;
        }
        writer.write_all(&batch)?;
        writer.flush()?;
        writer.sync_durable()
    })();

    if let Err(write_error) = result {
        return match rollback_batch(writer, append_start) {
            Ok(()) => Err(write_error),
            Err(rollback_error) => Err(std::io::Error::other(format!(
                "observability append failed ({write_error}); rollback to {append_start} failed ({rollback_error})"
            ))),
        };
    }
    Ok(())
}

/// Serializes every in-process writer of the primary observability JSONL file.
/// `ensure_flusher` is one-shot, but tests and manual flush callers may overlap.
/// The opened file is also kernel-locked so cooperating AgentDesk processes
/// cannot interleave length capture, append, and rollback on the shared path.
static JSONL_APPEND_LOCK: Mutex<()> = Mutex::new(());

fn lock_jsonl_append() -> std::io::Result<MutexGuard<'static, ()>> {
    JSONL_APPEND_LOCK
        .lock()
        .map_err(|_| std::io::Error::other("observability JSONL append lock poisoned"))
}

/// Append the given events as one framed batch and synchronize file data and
/// metadata. In-process failures truncate and synchronize back to the exact
/// pre-batch length while holding the process-wide append lock. A prior crash
/// tail is delimited before appending so subsequent records remain parseable.
pub fn flush_events_to_disk(events: &[StructuredEvent]) -> std::io::Result<()> {
    use std::fs::{OpenOptions, create_dir_all};
    if events.is_empty() {
        return Ok(());
    }
    let _append_guard = lock_jsonl_append()?;
    let path = flush_target_path();
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    file.lock()?;
    append_transaction(&mut file, events)
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

    #[derive(Default)]
    struct FaultFile {
        bytes: Vec<u8>,
        position: u64,
        fail_after: Option<usize>,
        bytes_written: usize,
        fail_flush: bool,
        fail_sync_once: bool,
        sync_calls: usize,
    }

    impl FaultFile {
        fn with_prefix(prefix: &[u8]) -> Self {
            Self {
                bytes: prefix.to_vec(),
                position: prefix.len() as u64,
                ..Self::default()
            }
        }
    }

    impl Write for FaultFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(limit) = self.fail_after {
                if self.bytes_written >= limit {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "injected write failure",
                    ));
                }
                let allowed = (limit - self.bytes_written).min(buf.len());
                let end = self.position as usize + allowed;
                if end > self.bytes.len() {
                    self.bytes.resize(end, 0);
                }
                self.bytes[self.position as usize..end].copy_from_slice(&buf[..allowed]);
                self.position = end as u64;
                self.bytes_written += allowed;
                return Ok(allowed);
            }
            let end = self.position as usize + buf.len();
            if end > self.bytes.len() {
                self.bytes.resize(end, 0);
            }
            self.bytes[self.position as usize..end].copy_from_slice(buf);
            self.position = end as u64;
            self.bytes_written += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }

    impl Seek for FaultFile {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            let next = match position {
                SeekFrom::Start(offset) => offset as i128,
                SeekFrom::End(offset) => self.bytes.len() as i128 + offset as i128,
                SeekFrom::Current(offset) => self.position as i128 + offset as i128,
            };
            if next < 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
            }
            self.position = next as u64;
            Ok(self.position)
        }
    }

    impl TransactionalEventFile for FaultFile {
        fn len(&self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn truncate(&mut self, len: u64) -> io::Result<()> {
            self.bytes.truncate(len as usize);
            self.position = self.position.min(len);
            Ok(())
        }

        fn sync_durable(&mut self) -> io::Result<()> {
            self.sync_calls += 1;
            if self.fail_sync_once {
                self.fail_sync_once = false;
                Err(io::Error::other("injected sync failure"))
            } else {
                Ok(())
            }
        }

        fn ends_with_newline(&mut self, len: u64) -> io::Result<bool> {
            Ok(len == 0 || self.bytes[len as usize - 1] == b'\n')
        }
    }

    fn parse_jsonl(bytes: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    #[test]
    fn every_partial_batch_offset_rolls_back_before_retry() {
        let events = vec![event("first"), event("second")];
        let batch = serialize_batch(&events).unwrap();
        let prefix = b"{\"event_type\":\"existing\"}\n";

        for failure_offset in 0..batch.len() {
            let mut writer = FaultFile::with_prefix(prefix);
            writer.fail_after = Some(failure_offset);
            assert!(append_transaction(&mut writer, &events).is_err());
            assert_eq!(writer.bytes, prefix, "failure offset {failure_offset}");

            writer.fail_after = None;
            writer.bytes_written = 0;
            append_transaction(&mut writer, &events).unwrap();
            let contents = String::from_utf8_lossy(&writer.bytes);
            let lines: Vec<&str> = contents.lines().collect();
            assert_eq!(lines.len(), 3, "failure offset {failure_offset}");
            assert!(
                lines
                    .iter()
                    .all(|line| serde_json::from_str::<Value>(line).is_ok())
            );
            assert_eq!(
                parse_jsonl(&writer.bytes)[1]["event_id"],
                events[0].event_id
            );
            assert_eq!(
                parse_jsonl(&writer.bytes)[2]["event_id"],
                events[1].event_id
            );
        }
    }

    #[test]
    fn full_json_before_newline_failure_rolls_back_without_concatenation() {
        let event = event("newline-boundary");
        let batch = serialize_batch(std::slice::from_ref(&event)).unwrap();
        let json_len = batch.len() - 1;
        let mut writer = FaultFile::default();
        writer.fail_after = Some(json_len);

        assert!(append_transaction(&mut writer, std::slice::from_ref(&event)).is_err());
        assert!(writer.bytes.is_empty());

        writer.fail_after = None;
        writer.bytes_written = 0;
        append_transaction(&mut writer, std::slice::from_ref(&event)).unwrap();
        let parsed = parse_jsonl(&writer.bytes);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["event_id"], event.event_id);
    }

    #[test]
    fn flush_and_sync_failures_roll_back_before_stable_id_retry() {
        for fail_sync in [false, true] {
            let event = event("e1");
            let prefix = b"{\"event_type\":\"existing\"}\n";
            let mut writer = FaultFile::with_prefix(prefix);
            writer.fail_flush = !fail_sync;
            writer.fail_sync_once = fail_sync;

            assert!(append_transaction(&mut writer, std::slice::from_ref(&event)).is_err());
            assert_eq!(writer.bytes, prefix);

            writer.fail_flush = false;
            append_transaction(&mut writer, std::slice::from_ref(&event)).unwrap();
            let parsed = parse_jsonl(&writer.bytes);
            assert_eq!(parsed.len(), 2);
            assert_eq!(parsed[1]["event_id"], event.event_id);
        }
    }

    #[test]
    fn prior_crash_tail_is_delimited_before_valid_records() {
        let mut writer = FaultFile::with_prefix(b"{\"event_type\":\"crashed\"");
        let event = event("recovered");

        append_transaction(&mut writer, std::slice::from_ref(&event)).unwrap();

        let contents = String::from_utf8_lossy(&writer.bytes);
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(serde_json::from_str::<Value>(lines[0]).is_err());
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap()["event_id"],
            event.event_id
        );
    }

    #[test]
    fn sync_failure_followed_by_overwrite_counts_unconfirmed_loss() {
        let log = EventLog::new(1);
        log.push(event("e1"));
        assert!(flush_once_with(&log, |_| Err(io::Error::other("sync failed"))).is_err());
        log.push(event("e2"));

        assert_eq!(log.dropped_total(), 1);
        assert_eq!(log.retention_evicted_total(), 0);
    }

    #[test]
    fn durable_sync_success_is_required_before_confirmation() {
        let log = EventLog::new(1);
        log.push(event("e1"));
        let mut writer = FaultFile::default();

        assert_eq!(
            flush_once_with(&log, |events| append_transaction(&mut writer, events)).unwrap(),
            1
        );
        assert_eq!(writer.sync_calls, 1);
        assert!(log.snapshot_unflushed().is_none());

        log.push(event("e2"));
        assert_eq!(log.dropped_total(), 0);
        assert_eq!(log.retention_evicted_total(), 1);
    }

    #[test]
    fn serialized_event_id_is_additive_and_stable_across_snapshot_clones() {
        let log = EventLog::new(2);
        log.push(event("e1"));
        let first = log.snapshot_unflushed().unwrap();
        let second = log.snapshot_unflushed().unwrap();
        let first_value = serde_json::to_value(&first.events()[0]).unwrap();

        assert_eq!(first.events()[0].event_id, second.events()[0].event_id);
        assert_eq!(
            first_value["event_id"].as_str(),
            Some(first.events()[0].event_id.as_str())
        );
        assert_eq!(first_value["event_type"], "e1");
        assert!(first_value["payload"].is_object());
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
