//! Bounded in-memory structured event log with periodic JSONL flush.
//!
//! Introduced by #1070. Complements the heavier SQL-backed event path already
//! in `observability::mod` by providing a lock-light ring buffer suitable for
//! very cheap hot-path writes and quick inspection via
//! `/api/analytics/observability`.
//!
//! The buffer is bounded (`MAX_EVENTS`) — the oldest event is dropped when full.
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

/// Bounded ring buffer for structured events.
#[derive(Debug)]
pub struct EventLog {
    capacity: usize,
    buffer: Mutex<VecDeque<StructuredEvent>>,
    /// Index into `buffer` marking the first un-flushed event. All events with
    /// smaller logical indices have already been written out to disk.
    /// We use a monotonic absolute counter: `next_logical_idx` is the total
    /// count ever pushed, `last_flushed_idx` is the last absolute index
    /// written (exclusive upper bound).
    next_logical_idx: Mutex<u64>,
    last_flushed_idx: Mutex<u64>,
}

impl EventLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            buffer: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
            next_logical_idx: Mutex::new(0),
            last_flushed_idx: Mutex::new(0),
        }
    }

    pub fn push(&self, event: StructuredEvent) {
        if let Ok(mut buf) = self.buffer.lock() {
            if buf.len() == self.capacity {
                buf.pop_front();
            }
            buf.push_back(event);
        }
        if let Ok(mut idx) = self.next_logical_idx.lock() {
            *idx = idx.saturating_add(1);
        }
    }

    /// Return up to `limit` most recent events (newest last).
    pub fn recent(&self, limit: usize) -> Vec<StructuredEvent> {
        let Ok(buf) = self.buffer.lock() else {
            return Vec::new();
        };
        let len = buf.len();
        let take = limit.min(len);
        buf.iter().skip(len - take).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.buffer.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Drain any events that haven't been flushed to disk yet. Returns
    /// `(events, new_flushed_idx)` — caller must advance via
    /// `commit_flushed(new_flushed_idx)` on successful persistence so that the
    /// same events aren't emitted twice in a subsequent cycle.
    pub fn drain_unflushed(&self) -> (Vec<StructuredEvent>, u64) {
        let Ok(buf) = self.buffer.lock() else {
            return (Vec::new(), 0);
        };
        let Ok(next_idx) = self.next_logical_idx.lock() else {
            return (Vec::new(), 0);
        };
        let Ok(last_flushed) = self.last_flushed_idx.lock() else {
            return (Vec::new(), 0);
        };

        // Buffer holds at most `capacity` most recent events. The absolute
        // index of `buf.front()` is `*next_idx - buf.len() as u64`.
        let buf_start_abs = next_idx.saturating_sub(buf.len() as u64);
        let begin_abs = (*last_flushed).max(buf_start_abs);
        if begin_abs >= *next_idx {
            return (Vec::new(), *next_idx);
        }
        let skip = (begin_abs - buf_start_abs) as usize;
        let events: Vec<StructuredEvent> = buf.iter().skip(skip).cloned().collect();
        (events, *next_idx)
    }

    pub fn commit_flushed(&self, new_idx: u64) {
        if let Ok(mut last_flushed) = self.last_flushed_idx.lock() {
            if new_idx > *last_flushed {
                *last_flushed = new_idx;
            }
        }
    }

    #[cfg(test)]
    pub fn clear(&self) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.clear();
        }
        if let Ok(mut idx) = self.next_logical_idx.lock() {
            *idx = 0;
        }
        if let Ok(mut flushed) = self.last_flushed_idx.lock() {
            *flushed = 0;
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

/// Append the given events to the JSONL target. Returns `Ok(())` on success.
pub fn flush_events_to_disk(events: &[StructuredEvent]) -> std::io::Result<()> {
    use std::fs::{OpenOptions, create_dir_all};
    use std::io::Write;
    if events.is_empty() {
        return Ok(());
    }
    let path = flush_target_path();
    if let Some(parent) = path.parent() {
        let _ = create_dir_all(parent);
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    for ev in events {
        // Serialize in a way that never fails for sane Value payloads.
        let line = serde_json::to_string(ev)
            .unwrap_or_else(|_| "{\"event_type\":\"_serialize_error\"}".to_string());
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

/// Spawn the background flush task (idempotent).
pub fn ensure_flusher() {
    if FLUSHER_STARTED.set(()).is_err() {
        return;
    }
    let log = global();
    // If there's no runtime yet, try_spawn. If we're outside tokio context,
    // silently skip — tests or short-lived tools don't require flushing.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the first immediate tick.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let (events, new_idx) = log.drain_unflushed();
                if events.is_empty() {
                    continue;
                }
                if let Err(error) = flush_events_to_disk(&events) {
                    tracing::warn!(%error, "observability events flush failed");
                } else {
                    log.commit_flushed(new_idx);
                }
            }
        });
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn reset_for_tests() {
    global().clear();
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn buffer_evicts_oldest_beyond_capacity() {
        let log = EventLog::new(4);
        for i in 0..10 {
            log.push(StructuredEvent::new(
                format!("e{i}"),
                Some(1),
                Some("codex"),
                json!({ "i": i }),
            ));
        }
        assert_eq!(log.len(), 4);
        let recent = log.recent(10);
        let names: Vec<_> = recent.iter().map(|e| e.event_type.clone()).collect();
        assert_eq!(names, vec!["e6", "e7", "e8", "e9"]);
    }

    #[test]
    fn drain_unflushed_respects_commit() {
        let log = EventLog::new(100);
        for i in 0..5 {
            log.push(StructuredEvent::new(
                format!("e{i}"),
                None,
                None,
                json!({"i": i}),
            ));
        }
        let (first, idx) = log.drain_unflushed();
        assert_eq!(first.len(), 5);
        log.commit_flushed(idx);

        // Nothing new → empty drain.
        let (second, _) = log.drain_unflushed();
        assert!(second.is_empty());

        // Push more, drain should only return new ones.
        log.push(StructuredEvent::new("e5", None, None, json!({})));
        let (third, _) = log.drain_unflushed();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].event_type, "e5");
    }

    #[test]
    fn drain_unflushed_handles_ring_eviction() {
        // Capacity 4, push 10 without flush → drain returns only the 4 still
        // in-memory. The 6 evicted events are considered "lost" (ring buffer
        // semantics), but the logical index is still respected so subsequent
        // drains don't re-emit events.
        let log = EventLog::new(4);
        for i in 0..10 {
            log.push(StructuredEvent::new(
                format!("e{i}"),
                None,
                None,
                json!({"i": i}),
            ));
        }
        let (events, idx) = log.drain_unflushed();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].event_type, "e6");
        log.commit_flushed(idx);
        let (second, _) = log.drain_unflushed();
        assert!(second.is_empty());
    }

    #[test]
    fn recent_returns_newest_last() {
        let log = EventLog::new(10);
        log.push(StructuredEvent::new("a", None, None, json!({})));
        log.push(StructuredEvent::new("b", None, None, json!({})));
        log.push(StructuredEvent::new("c", None, None, json!({})));
        let got = log.recent(2);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].event_type, "b");
        assert_eq!(got[1].event_type, "c");
    }
}
