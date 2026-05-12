//! #2049: background worker loop + counter snapshot flush split out of
//! `mod.rs`. Owns batching, dead-letter JSONL fallback, and the snapshot
//! collection path. The Postgres write primitives live in `pg_io`; the
//! retention sweep lives in `retention`.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::events;
use super::helpers::matches_filters;
use super::pg_io::{
    insert_events_pg, insert_events_pg_row_isolated, insert_quality_events_pg,
    insert_quality_events_pg_row_isolated, insert_snapshots_pg,
};
use super::retention::run_retention_sweep;
use super::{
    AnalyticsFilters, EVENT_BATCH_SIZE, EVENT_FLUSH_INTERVAL, ObservabilityRuntime, QueuedEvent,
    QueuedQualityEvent, RETENTION_SWEEP_INTERVAL, SNAPSHOT_FLUSH_INTERVAL, SnapshotRow,
    StorageHandles, WorkerMessage, runtime,
};

pub(super) fn worker_sender() -> Option<mpsc::UnboundedSender<WorkerMessage>> {
    let runtime = runtime();
    ensure_worker(&runtime);
    runtime.sender.lock().ok().and_then(|sender| sender.clone())
}

pub(super) fn ensure_worker(runtime: &Arc<ObservabilityRuntime>) {
    let mut sender_guard = match runtime.sender.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if sender_guard
        .as_ref()
        .is_some_and(|sender| !sender.is_closed())
    {
        return;
    }

    let (tx, rx) = mpsc::unbounded_channel();
    *sender_guard = Some(tx);
    spawn_worker(runtime.clone(), rx);
}

fn spawn_worker(runtime: Arc<ObservabilityRuntime>, rx: mpsc::UnboundedReceiver<WorkerMessage>) {
    let task = async move {
        worker_loop(runtime, rx).await;
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(task);
        return;
    }

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => runtime.block_on(task),
            Err(error) => {
                tracing::warn!("[observability] failed to bootstrap worker runtime: {error}");
            }
        }
    });
}

async fn worker_loop(
    runtime: Arc<ObservabilityRuntime>,
    mut rx: mpsc::UnboundedReceiver<WorkerMessage>,
) {
    let mut batch = Vec::new();
    let mut quality_batch = Vec::new();
    let mut flush_tick = tokio::time::interval(EVENT_FLUSH_INTERVAL);
    let mut snapshot_tick = tokio::time::interval(SNAPSHOT_FLUSH_INTERVAL);
    // #2049 Finding 9: hourly retention sweep.
    let mut retention_tick = tokio::time::interval(RETENTION_SWEEP_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    retention_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_message = rx.recv() => {
                match maybe_message {
                    Some(WorkerMessage::Event(event)) => {
                        batch.push(event);
                        if batch.len() >= EVENT_BATCH_SIZE {
                            flush_event_batch(&runtime, &mut batch).await;
                        }
                    }
                    Some(WorkerMessage::QualityEvent(event)) => {
                        quality_batch.push(event);
                        if quality_batch.len() >= EVENT_BATCH_SIZE {
                            flush_quality_event_batch(&runtime, &mut quality_batch).await;
                        }
                    }
                    Some(WorkerMessage::Flush(done)) => {
                        flush_event_batch(&runtime, &mut batch).await;
                        flush_quality_event_batch(&runtime, &mut quality_batch).await;
                        flush_counter_snapshots(&runtime).await;
                        let _ = done.send(());
                    }
                    None => break,
                }
            }
            _ = flush_tick.tick() => {
                flush_event_batch(&runtime, &mut batch).await;
                flush_quality_event_batch(&runtime, &mut quality_batch).await;
            }
            _ = snapshot_tick.tick() => {
                flush_counter_snapshots(&runtime).await;
            }
            _ = retention_tick.tick() => {
                run_retention_sweep(&runtime).await;
            }
        }
    }

    flush_event_batch(&runtime, &mut batch).await;
    flush_quality_event_batch(&runtime, &mut quality_batch).await;
    flush_counter_snapshots(&runtime).await;
}

/// #2049 Finding 1: Flush observability events with multi-tier fallback.
/// (1) Try bulk insert in one tx. (2) On bulk failure, retry each row in its
/// own tx so a single broken row does not lose its 63 healthy neighbors.
/// (3) Rows that still fail are appended to a dead-letter JSONL on disk so
/// the sample is recoverable. (4) If JSONL fallback also fails, push the
/// events back to the front of the worker batch so the next tick retries.
async fn flush_event_batch(runtime: &Arc<ObservabilityRuntime>, batch: &mut Vec<QueuedEvent>) {
    if batch.is_empty() {
        return;
    }
    let drained = std::mem::take(batch);
    let handles = storage_handles(runtime);

    let Some(pool) = handles.pg_pool.as_ref() else {
        // No PG configured (standalone mode) — preserve pre-#2049 behaviour;
        // standalone mode never persisted these events.
        return;
    };

    if let Err(error) = insert_events_pg(pool, &drained).await {
        tracing::warn!(
            "[observability] postgres event bulk flush failed ({} events): {error}; retrying per-row",
            drained.len()
        );
        let failed = insert_events_pg_row_isolated(pool, &drained).await;
        if !failed.is_empty() {
            handle_event_flush_fallback(batch, failed, "events");
        }
    }
}

/// #2049 Finding 1 — same multi-tier fallback for `agent_quality_event`.
async fn flush_quality_event_batch(
    runtime: &Arc<ObservabilityRuntime>,
    batch: &mut Vec<QueuedQualityEvent>,
) {
    if batch.is_empty() {
        return;
    }
    let drained = std::mem::take(batch);
    let handles = storage_handles(runtime);

    let Some(pool) = handles.pg_pool.as_ref() else {
        return;
    };

    if let Err(error) = insert_quality_events_pg(pool, &drained).await {
        tracing::warn!(
            "[quality] postgres event bulk flush failed ({} events): {error}; retrying per-row",
            drained.len()
        );
        let failed = insert_quality_events_pg_row_isolated(pool, &drained).await;
        if !failed.is_empty() {
            handle_quality_event_flush_fallback(batch, failed);
        }
    }
}

/// #2049 Finding 1: Dump failed event rows to JSONL; if disk fallback fails,
/// push them back to the front of `batch` so the next worker tick retries.
fn handle_event_flush_fallback(
    batch: &mut Vec<QueuedEvent>,
    failed: Vec<QueuedEvent>,
    suffix: &str,
) {
    let count = failed.len();
    match events::flush_dead_letter_jsonl(suffix, &failed) {
        Ok(()) => {
            tracing::warn!(
                "[observability] {count} events dumped to dead-letter JSONL (suffix={suffix}) after PG flush failure"
            );
        }
        Err(disk_error) => {
            tracing::error!(
                "[observability] dead-letter JSONL also failed (suffix={suffix}, {count} events): {disk_error}; pushing back to worker batch"
            );
            let mut rescued = failed;
            rescued.extend(std::mem::take(batch));
            *batch = rescued;
        }
    }
}

fn handle_quality_event_flush_fallback(
    batch: &mut Vec<QueuedQualityEvent>,
    failed: Vec<QueuedQualityEvent>,
) {
    let count = failed.len();
    match events::flush_dead_letter_jsonl("quality-events", &failed) {
        Ok(()) => {
            tracing::warn!(
                "[quality] {count} events dumped to dead-letter JSONL after PG flush failure"
            );
        }
        Err(disk_error) => {
            tracing::error!(
                "[quality] dead-letter JSONL also failed ({count} events): {disk_error}; pushing back to worker batch"
            );
            let mut rescued = failed;
            rescued.extend(std::mem::take(batch));
            *batch = rescued;
        }
    }
}

async fn flush_counter_snapshots(runtime: &Arc<ObservabilityRuntime>) {
    let snapshots = snapshot_rows(runtime, None);
    if snapshots.is_empty() {
        return;
    }

    let handles = storage_handles(runtime);
    if let Some(pool) = handles.pg_pool.as_ref() {
        match insert_snapshots_pg(pool, &snapshots).await {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!("[observability] postgres snapshot flush failed: {error}");
            }
        }
    }
}

pub(super) fn storage_handles(runtime: &Arc<ObservabilityRuntime>) -> StorageHandles {
    runtime
        .storage
        .lock()
        .map(|handles| handles.clone())
        .unwrap_or_default()
}

pub(super) fn snapshot_rows(
    runtime: &Arc<ObservabilityRuntime>,
    filters: Option<&AnalyticsFilters>,
) -> Vec<SnapshotRow> {
    let mut rows = runtime
        .counters
        .iter()
        .filter_map(|entry| {
            if !matches_filters(
                filters,
                &entry.key().provider,
                &entry.key().channel_id,
                None,
            ) {
                return None;
            }
            Some(SnapshotRow {
                provider: entry.key().provider.clone(),
                channel_id: entry.key().channel_id.clone(),
                values: entry.value().snapshot(),
            })
        })
        .collect::<Vec<_>>();

    rows.sort_by(|left, right| {
        right
            .values
            .turn_attempts
            .cmp(&left.values.turn_attempts)
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.channel_id.cmp(&right.channel_id))
    });
    rows
}
