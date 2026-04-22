use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use dashmap::DashMap;
use libsql_rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::sync::{mpsc, oneshot};

use crate::db::Db;

const EVENT_BATCH_SIZE: usize = 64;
const EVENT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const SNAPSHOT_FLUSH_INTERVAL: Duration = Duration::from_secs(15);
const DEFAULT_EVENT_LIMIT: usize = 100;
const DEFAULT_COUNTER_LIMIT: usize = 200;
const MAX_EVENT_LIMIT: usize = 500;
const MAX_COUNTER_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CounterKey {
    provider: String,
    channel_id: String,
}

#[derive(Debug, Default)]
struct CounterBucket {
    turn_attempts: AtomicU64,
    guard_fires: AtomicU64,
    watcher_replacements: AtomicU64,
    recovery_fires: AtomicU64,
    turn_successes: AtomicU64,
    turn_failures: AtomicU64,
}

impl CounterBucket {
    fn apply(&self, delta: CounterDelta) {
        if delta.turn_attempts > 0 {
            self.turn_attempts
                .fetch_add(delta.turn_attempts, Ordering::Relaxed);
        }
        if delta.guard_fires > 0 {
            self.guard_fires
                .fetch_add(delta.guard_fires, Ordering::Relaxed);
        }
        if delta.watcher_replacements > 0 {
            self.watcher_replacements
                .fetch_add(delta.watcher_replacements, Ordering::Relaxed);
        }
        if delta.recovery_fires > 0 {
            self.recovery_fires
                .fetch_add(delta.recovery_fires, Ordering::Relaxed);
        }
        if delta.turn_successes > 0 {
            self.turn_successes
                .fetch_add(delta.turn_successes, Ordering::Relaxed);
        }
        if delta.turn_failures > 0 {
            self.turn_failures
                .fetch_add(delta.turn_failures, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> CounterValues {
        CounterValues {
            turn_attempts: self.turn_attempts.load(Ordering::Relaxed),
            guard_fires: self.guard_fires.load(Ordering::Relaxed),
            watcher_replacements: self.watcher_replacements.load(Ordering::Relaxed),
            recovery_fires: self.recovery_fires.load(Ordering::Relaxed),
            turn_successes: self.turn_successes.load(Ordering::Relaxed),
            turn_failures: self.turn_failures.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CounterDelta {
    turn_attempts: u64,
    guard_fires: u64,
    watcher_replacements: u64,
    recovery_fires: u64,
    turn_successes: u64,
    turn_failures: u64,
}

impl CounterDelta {
    fn is_zero(self) -> bool {
        self.turn_attempts == 0
            && self.guard_fires == 0
            && self.watcher_replacements == 0
            && self.recovery_fires == 0
            && self.turn_successes == 0
            && self.turn_failures == 0
    }
}

#[derive(Debug, Clone)]
struct QueuedEvent {
    event_type: String,
    provider: Option<String>,
    channel_id: Option<String>,
    dispatch_id: Option<String>,
    session_key: Option<String>,
    turn_id: Option<String>,
    status: Option<String>,
    payload_json: String,
}

#[derive(Debug)]
enum WorkerMessage {
    Event(QueuedEvent),
    Flush(oneshot::Sender<()>),
}

#[derive(Clone, Default)]
struct StorageHandles {
    db: Option<Db>,
    pg_pool: Option<PgPool>,
}

#[derive(Default)]
struct ObservabilityRuntime {
    counters: DashMap<CounterKey, Arc<CounterBucket>>,
    storage: Mutex<StorageHandles>,
    sender: Mutex<Option<mpsc::UnboundedSender<WorkerMessage>>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct CounterValues {
    turn_attempts: u64,
    guard_fires: u64,
    watcher_replacements: u64,
    recovery_fires: u64,
    turn_successes: u64,
    turn_failures: u64,
}

#[derive(Debug, Clone)]
struct SnapshotRow {
    provider: String,
    channel_id: String,
    values: CounterValues,
}

#[derive(Debug, Clone, Default)]
pub struct AnalyticsFilters {
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub event_type: Option<String>,
    pub event_limit: usize,
    pub counter_limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnalyticsEventRecord {
    pub id: i64,
    pub event_type: String,
    pub provider: Option<String>,
    pub channel_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub session_key: Option<String>,
    pub turn_id: Option<String>,
    pub status: Option<String>,
    pub payload: Value,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnalyticsCounterSnapshot {
    pub provider: String,
    pub channel_id: String,
    pub turn_attempts: u64,
    pub guard_fires: u64,
    pub watcher_replacements: u64,
    pub recovery_fires: u64,
    pub turn_successes: u64,
    pub turn_failures: u64,
    pub success_rate: f64,
    pub failure_rate: f64,
    pub snapshot_at: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnalyticsResponse {
    pub generated_at: String,
    pub counters: Vec<AnalyticsCounterSnapshot>,
    pub events: Vec<AnalyticsEventRecord>,
}

static OBSERVABILITY_RUNTIME: OnceLock<Arc<ObservabilityRuntime>> = OnceLock::new();

fn runtime() -> Arc<ObservabilityRuntime> {
    OBSERVABILITY_RUNTIME
        .get_or_init(|| Arc::new(ObservabilityRuntime::default()))
        .clone()
}

pub fn init_observability(db: Db, pg_pool: Option<PgPool>) {
    let runtime = runtime();
    if let Ok(mut storage) = runtime.storage.lock() {
        storage.db = Some(db);
        storage.pg_pool = pg_pool;
    }
    ensure_worker(&runtime);
}

pub fn emit_turn_started(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
) {
    emit_event(
        "turn_started",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        Some("started"),
        CounterDelta {
            turn_attempts: 1,
            ..CounterDelta::default()
        },
        json!({}),
    );
}

pub fn emit_turn_finished(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    outcome: &str,
    duration_ms: i64,
    tmux_handoff: bool,
) {
    let normalized_outcome = normalize_string(outcome);
    let is_success = matches!(
        normalized_outcome.as_deref(),
        Some("completed") | Some("tmux_handoff")
    );
    emit_event(
        "turn_finished",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalized_outcome.as_deref(),
        CounterDelta {
            turn_successes: u64::from(is_success),
            turn_failures: u64::from(!is_success),
            ..CounterDelta::default()
        },
        json!({
            "duration_ms": duration_ms.max(0),
            "tmux_handoff": tmux_handoff,
        }),
    );
}

pub fn emit_guard_fired(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    guard_type: &str,
) {
    emit_event(
        "guard_fired",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalize_string(guard_type).as_deref(),
        CounterDelta {
            guard_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "guard_type": normalize_string(guard_type),
        }),
    );
}

pub fn emit_watcher_replaced(provider: &str, channel_id: u64, source: &str) {
    emit_event(
        "watcher_replaced",
        Some(provider),
        Some(channel_id),
        None,
        None,
        None,
        Some("replaced"),
        CounterDelta {
            watcher_replacements: 1,
            ..CounterDelta::default()
        },
        json!({
            "source": normalize_string(source),
        }),
    );
}

pub fn emit_recovery_fired(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    reason: &str,
) {
    emit_event(
        "recovery_fired",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        None,
        normalize_string(reason).as_deref(),
        CounterDelta {
            recovery_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "reason": normalize_string(reason),
        }),
    );
}

pub fn emit_dispatch_result(
    dispatch_id: &str,
    kanban_card_id: Option<&str>,
    dispatch_type: Option<&str>,
    from_status: Option<&str>,
    to_status: &str,
    transition_source: &str,
    payload: Option<&Value>,
) {
    emit_event(
        "dispatch_result",
        None,
        None,
        Some(dispatch_id),
        None,
        None,
        Some(to_status),
        CounterDelta::default(),
        json!({
            "kanban_card_id": normalize_string(kanban_card_id.unwrap_or_default()),
            "dispatch_type": normalize_string(dispatch_type.unwrap_or_default()),
            "from_status": normalize_string(from_status.unwrap_or_default()),
            "to_status": normalize_string(to_status),
            "transition_source": normalize_string(transition_source),
            "payload": payload.cloned().unwrap_or_else(|| json!({})),
        }),
    );
}

fn emit_event(
    event_type: &str,
    provider: Option<&str>,
    channel_id: Option<u64>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    status: Option<&str>,
    counter_delta: CounterDelta,
    payload: Value,
) {
    let event_type = normalize_string(event_type);
    if event_type.is_none() {
        return;
    }

    let provider = provider.and_then(normalize_string);
    let channel_id = channel_id.map(|value| value.to_string());
    if !counter_delta.is_zero()
        && let (Some(provider), Some(channel_id)) = (provider.as_ref(), channel_id.as_ref())
    {
        let bucket = runtime()
            .counters
            .entry(CounterKey {
                provider: provider.clone(),
                channel_id: channel_id.clone(),
            })
            .or_insert_with(|| Arc::new(CounterBucket::default()))
            .clone();
        bucket.apply(counter_delta);
    }

    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let queued = QueuedEvent {
        event_type: event_type.unwrap_or_default(),
        provider,
        channel_id,
        dispatch_id: dispatch_id.and_then(normalize_string),
        session_key: session_key.and_then(normalize_string),
        turn_id: turn_id.and_then(normalize_string),
        status: status.and_then(normalize_string),
        payload_json,
    };

    if let Some(sender) = worker_sender() {
        let _ = sender.send(WorkerMessage::Event(queued));
    }
}

fn worker_sender() -> Option<mpsc::UnboundedSender<WorkerMessage>> {
    let runtime = runtime();
    ensure_worker(&runtime);
    runtime.sender.lock().ok().and_then(|sender| sender.clone())
}

fn ensure_worker(runtime: &Arc<ObservabilityRuntime>) {
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
    let mut flush_tick = tokio::time::interval(EVENT_FLUSH_INTERVAL);
    let mut snapshot_tick = tokio::time::interval(SNAPSHOT_FLUSH_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                    Some(WorkerMessage::Flush(done)) => {
                        flush_event_batch(&runtime, &mut batch).await;
                        flush_counter_snapshots(&runtime).await;
                        let _ = done.send(());
                    }
                    None => break,
                }
            }
            _ = flush_tick.tick() => {
                flush_event_batch(&runtime, &mut batch).await;
            }
            _ = snapshot_tick.tick() => {
                flush_counter_snapshots(&runtime).await;
            }
        }
    }

    flush_event_batch(&runtime, &mut batch).await;
    flush_counter_snapshots(&runtime).await;
}

async fn flush_event_batch(runtime: &Arc<ObservabilityRuntime>, batch: &mut Vec<QueuedEvent>) {
    if batch.is_empty() {
        return;
    }
    let events = std::mem::take(batch);
    let handles = storage_handles(runtime);

    if let Some(pool) = handles.pg_pool.as_ref() {
        match insert_events_pg(pool, &events).await {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!("[observability] postgres event flush failed: {error}");
            }
        }
    }

    if let Some(db) = handles.db.as_ref()
        && let Err(error) = insert_events_sqlite(db, &events)
    {
        tracing::warn!("[observability] sqlite event flush failed: {error}");
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

    if let Some(db) = handles.db.as_ref()
        && let Err(error) = insert_snapshots_sqlite(db, &snapshots)
    {
        tracing::warn!("[observability] sqlite snapshot flush failed: {error}");
    }
}

fn storage_handles(runtime: &Arc<ObservabilityRuntime>) -> StorageHandles {
    runtime
        .storage
        .lock()
        .map(|handles| handles.clone())
        .unwrap_or_default()
}

fn snapshot_rows(
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

pub async fn query_analytics(
    db: &Db,
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
) -> Result<AnalyticsResponse> {
    let event_limit = normalized_event_limit(filters.event_limit);
    let counter_limit = normalized_counter_limit(filters.counter_limit);
    let live_counter_rows = snapshot_rows(&runtime(), Some(filters))
        .into_iter()
        .take(counter_limit)
        .collect::<Vec<_>>();
    let mut counters = live_counter_rows
        .into_iter()
        .map(|row| {
            counter_snapshot_from_values(
                &row.provider,
                &row.channel_id,
                row.values,
                "live",
                now_kst(),
            )
        })
        .collect::<Vec<_>>();

    let persisted_counters =
        query_counter_snapshots_db(db, pg_pool, filters, counter_limit).await?;
    let mut seen = counters
        .iter()
        .map(|snapshot| (snapshot.provider.clone(), snapshot.channel_id.clone()))
        .collect::<HashSet<_>>();
    for snapshot in persisted_counters {
        if seen.insert((snapshot.provider.clone(), snapshot.channel_id.clone())) {
            counters.push(snapshot);
        }
    }
    counters.truncate(counter_limit);

    let events = query_events_db(db, pg_pool, filters, event_limit).await?;
    Ok(AnalyticsResponse {
        generated_at: now_kst(),
        counters,
        events,
    })
}

async fn query_events_db(
    db: &Db,
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsEventRecord>> {
    if let Some(pool) = pg_pool {
        match query_events_pg(pool, filters, limit).await {
            Ok(records) => return Ok(records),
            Err(error) => {
                tracing::warn!("[observability] postgres event query failed: {error}");
            }
        }
    }

    let conn = db
        .read_conn()
        .map_err(|error| anyhow!("db read connection for observability events: {error}"))?;
    query_events_sqlite(&conn, filters, limit)
}

async fn query_counter_snapshots_db(
    db: &Db,
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsCounterSnapshot>> {
    if let Some(pool) = pg_pool {
        match query_counter_snapshots_pg(pool, filters, limit).await {
            Ok(records) => return Ok(records),
            Err(error) => {
                tracing::warn!("[observability] postgres snapshot query failed: {error}");
            }
        }
    }

    let conn = db
        .read_conn()
        .map_err(|error| anyhow!("db read connection for observability snapshots: {error}"))?;
    query_counter_snapshots_sqlite(&conn, filters, limit)
}

fn query_events_sqlite(
    conn: &Connection,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsEventRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id,
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json,
                datetime(created_at, '+9 hours') AS created_at_kst
         FROM observability_events
         WHERE (?1 IS NULL OR provider = ?1)
           AND (?2 IS NULL OR channel_id = ?2)
           AND (?3 IS NULL OR event_type = ?3)
         ORDER BY id DESC
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        libsql_rusqlite::params![
            filters.provider.as_deref(),
            filters.channel_id.as_deref(),
            filters.event_type.as_deref(),
            limit as i64,
        ],
        |row| {
            let payload_json: Option<String> = row.get(8)?;
            Ok(AnalyticsEventRecord {
                id: row.get(0)?,
                event_type: row.get(1)?,
                provider: row.get(2)?,
                channel_id: row.get(3)?,
                dispatch_id: row.get(4)?,
                session_key: row.get(5)?,
                turn_id: row.get(6)?,
                status: row.get(7)?,
                payload: payload_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_else(|| json!({})),
                created_at: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
            })
        },
    )?;
    Ok(rows.collect::<libsql_rusqlite::Result<Vec<_>>>()?)
}

async fn query_events_pg(
    pool: &PgPool,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsEventRecord>> {
    let rows = sqlx::query(
        "SELECT id,
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json::text AS payload_json,
                to_char(created_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS created_at_kst
         FROM observability_events
         WHERE ($1::text IS NULL OR provider = $1)
           AND ($2::text IS NULL OR channel_id = $2)
           AND ($3::text IS NULL OR event_type = $3)
         ORDER BY id DESC
         LIMIT $4",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(filters.event_type.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres observability events: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let payload_json: Option<String> = row
                .try_get("payload_json")
                .map_err(|error| anyhow!("decode observability payload_json: {error}"))?;
            Ok(AnalyticsEventRecord {
                id: row
                    .try_get("id")
                    .map_err(|error| anyhow!("decode observability event id: {error}"))?,
                event_type: row
                    .try_get("event_type")
                    .map_err(|error| anyhow!("decode observability event_type: {error}"))?,
                provider: row
                    .try_get("provider")
                    .map_err(|error| anyhow!("decode observability provider: {error}"))?,
                channel_id: row
                    .try_get("channel_id")
                    .map_err(|error| anyhow!("decode observability channel_id: {error}"))?,
                dispatch_id: row
                    .try_get("dispatch_id")
                    .map_err(|error| anyhow!("decode observability dispatch_id: {error}"))?,
                session_key: row
                    .try_get("session_key")
                    .map_err(|error| anyhow!("decode observability session_key: {error}"))?,
                turn_id: row
                    .try_get("turn_id")
                    .map_err(|error| anyhow!("decode observability turn_id: {error}"))?,
                status: row
                    .try_get("status")
                    .map_err(|error| anyhow!("decode observability status: {error}"))?,
                payload: payload_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_else(|| json!({})),
                created_at: row
                    .try_get("created_at_kst")
                    .map_err(|error| anyhow!("decode observability created_at: {error}"))?,
            })
        })
        .collect()
}

fn query_counter_snapshots_sqlite(
    conn: &Connection,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsCounterSnapshot>> {
    let mut stmt = conn.prepare(
        "SELECT s.provider,
                s.channel_id,
                s.turn_attempts,
                s.guard_fires,
                s.watcher_replacements,
                s.recovery_fires,
                s.turn_successes,
                s.turn_failures,
                datetime(s.snapshot_at, '+9 hours') AS snapshot_at_kst
         FROM observability_counter_snapshots s
         JOIN (
             SELECT MAX(id) AS max_id
             FROM observability_counter_snapshots
             WHERE (?1 IS NULL OR provider = ?1)
               AND (?2 IS NULL OR channel_id = ?2)
             GROUP BY provider, channel_id
         ) latest
           ON latest.max_id = s.id
         ORDER BY s.turn_attempts DESC, s.provider, s.channel_id
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        libsql_rusqlite::params![
            filters.provider.as_deref(),
            filters.channel_id.as_deref(),
            limit as i64,
        ],
        |row| {
            let values = CounterValues {
                turn_attempts: row.get::<_, i64>(2)?.max(0) as u64,
                guard_fires: row.get::<_, i64>(3)?.max(0) as u64,
                watcher_replacements: row.get::<_, i64>(4)?.max(0) as u64,
                recovery_fires: row.get::<_, i64>(5)?.max(0) as u64,
                turn_successes: row.get::<_, i64>(6)?.max(0) as u64,
                turn_failures: row.get::<_, i64>(7)?.max(0) as u64,
            };
            Ok(counter_snapshot_from_values(
                &row.get::<_, String>(0)?,
                &row.get::<_, String>(1)?,
                values,
                "persisted",
                row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            ))
        },
    )?;
    Ok(rows.collect::<libsql_rusqlite::Result<Vec<_>>>()?)
}

async fn query_counter_snapshots_pg(
    pool: &PgPool,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsCounterSnapshot>> {
    let rows = sqlx::query(
        "SELECT provider,
                channel_id,
                turn_attempts,
                guard_fires,
                watcher_replacements,
                recovery_fires,
                turn_successes,
                turn_failures,
                to_char(snapshot_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS snapshot_at_kst
         FROM (
             SELECT DISTINCT ON (provider, channel_id)
                    provider,
                    channel_id,
                    turn_attempts,
                    guard_fires,
                    watcher_replacements,
                    recovery_fires,
                    turn_successes,
                    turn_failures,
                    snapshot_at,
                    id
             FROM observability_counter_snapshots
             WHERE ($1::text IS NULL OR provider = $1)
               AND ($2::text IS NULL OR channel_id = $2)
             ORDER BY provider, channel_id, id DESC
         ) snapshots
         ORDER BY turn_attempts DESC, provider, channel_id
         LIMIT $3",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres observability snapshots: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let values = CounterValues {
                turn_attempts: row
                    .try_get::<i64, _>("turn_attempts")
                    .map_err(|error| anyhow!("decode turn_attempts: {error}"))?
                    .max(0) as u64,
                guard_fires: row
                    .try_get::<i64, _>("guard_fires")
                    .map_err(|error| anyhow!("decode guard_fires: {error}"))?
                    .max(0) as u64,
                watcher_replacements: row
                    .try_get::<i64, _>("watcher_replacements")
                    .map_err(|error| anyhow!("decode watcher_replacements: {error}"))?
                    .max(0) as u64,
                recovery_fires: row
                    .try_get::<i64, _>("recovery_fires")
                    .map_err(|error| anyhow!("decode recovery_fires: {error}"))?
                    .max(0) as u64,
                turn_successes: row
                    .try_get::<i64, _>("turn_successes")
                    .map_err(|error| anyhow!("decode turn_successes: {error}"))?
                    .max(0) as u64,
                turn_failures: row
                    .try_get::<i64, _>("turn_failures")
                    .map_err(|error| anyhow!("decode turn_failures: {error}"))?
                    .max(0) as u64,
            };
            Ok(counter_snapshot_from_values(
                &row.try_get::<String, _>("provider")
                    .map_err(|error| anyhow!("decode snapshot provider: {error}"))?,
                &row.try_get::<String, _>("channel_id")
                    .map_err(|error| anyhow!("decode snapshot channel_id: {error}"))?,
                values,
                "persisted",
                row.try_get::<String, _>("snapshot_at_kst")
                    .map_err(|error| anyhow!("decode snapshot_at: {error}"))?,
            ))
        })
        .collect()
}

fn insert_events_sqlite(db: &Db, events: &[QueuedEvent]) -> Result<()> {
    let mut conn = db
        .separate_conn()
        .map_err(|error| anyhow!("open sqlite observability event connection: {error}"))?;
    let tx = conn
        .transaction()
        .map_err(|error| anyhow!("begin sqlite observability event tx: {error}"))?;
    for event in events {
        tx.execute(
            "INSERT INTO observability_events (
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            libsql_rusqlite::params![
                event.event_type,
                event.provider,
                event.channel_id,
                event.dispatch_id,
                event.session_key,
                event.turn_id,
                event.status,
                event.payload_json,
            ],
        )
        .map_err(|error| anyhow!("insert sqlite observability event: {error}"))?;
    }
    tx.commit()
        .map_err(|error| anyhow!("commit sqlite observability event tx: {error}"))?;
    Ok(())
}

async fn insert_events_pg(pool: &PgPool, events: &[QueuedEvent]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres observability event tx: {error}"))?;
    for event in events {
        sqlx::query(
            "INSERT INTO observability_events (
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS jsonb))",
        )
        .bind(&event.event_type)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.dispatch_id)
        .bind(&event.session_key)
        .bind(&event.turn_id)
        .bind(&event.status)
        .bind(&event.payload_json)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres observability event: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres observability event tx: {error}"))?;
    Ok(())
}

fn insert_snapshots_sqlite(db: &Db, snapshots: &[SnapshotRow]) -> Result<()> {
    let mut conn = db
        .separate_conn()
        .map_err(|error| anyhow!("open sqlite observability snapshot connection: {error}"))?;
    let tx = conn
        .transaction()
        .map_err(|error| anyhow!("begin sqlite observability snapshot tx: {error}"))?;
    for snapshot in snapshots {
        tx.execute(
            "INSERT INTO observability_counter_snapshots (
                provider,
                channel_id,
                turn_attempts,
                guard_fires,
                watcher_replacements,
                recovery_fires,
                turn_successes,
                turn_failures
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            libsql_rusqlite::params![
                snapshot.provider,
                snapshot.channel_id,
                saturating_i64(snapshot.values.turn_attempts),
                saturating_i64(snapshot.values.guard_fires),
                saturating_i64(snapshot.values.watcher_replacements),
                saturating_i64(snapshot.values.recovery_fires),
                saturating_i64(snapshot.values.turn_successes),
                saturating_i64(snapshot.values.turn_failures),
            ],
        )
        .map_err(|error| anyhow!("insert sqlite observability snapshot: {error}"))?;
    }
    tx.commit()
        .map_err(|error| anyhow!("commit sqlite observability snapshot tx: {error}"))?;
    Ok(())
}

async fn insert_snapshots_pg(pool: &PgPool, snapshots: &[SnapshotRow]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres observability snapshot tx: {error}"))?;
    for snapshot in snapshots {
        sqlx::query(
            "INSERT INTO observability_counter_snapshots (
                provider,
                channel_id,
                turn_attempts,
                guard_fires,
                watcher_replacements,
                recovery_fires,
                turn_successes,
                turn_failures
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&snapshot.provider)
        .bind(&snapshot.channel_id)
        .bind(saturating_i64(snapshot.values.turn_attempts))
        .bind(saturating_i64(snapshot.values.guard_fires))
        .bind(saturating_i64(snapshot.values.watcher_replacements))
        .bind(saturating_i64(snapshot.values.recovery_fires))
        .bind(saturating_i64(snapshot.values.turn_successes))
        .bind(saturating_i64(snapshot.values.turn_failures))
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres observability snapshot: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres observability snapshot tx: {error}"))?;
    Ok(())
}

fn counter_snapshot_from_values(
    provider: &str,
    channel_id: &str,
    values: CounterValues,
    source: &str,
    snapshot_at: String,
) -> AnalyticsCounterSnapshot {
    let attempt_count = values.turn_attempts.max(1) as f64;
    AnalyticsCounterSnapshot {
        provider: provider.to_string(),
        channel_id: channel_id.to_string(),
        turn_attempts: values.turn_attempts,
        guard_fires: values.guard_fires,
        watcher_replacements: values.watcher_replacements,
        recovery_fires: values.recovery_fires,
        turn_successes: values.turn_successes,
        turn_failures: values.turn_failures,
        success_rate: values.turn_successes as f64 / attempt_count,
        failure_rate: values.turn_failures as f64 / attempt_count,
        snapshot_at,
        source: source.to_string(),
    }
}

fn matches_filters(
    filters: Option<&AnalyticsFilters>,
    provider: &str,
    channel_id: &str,
    event_type: Option<&str>,
) -> bool {
    let Some(filters) = filters else {
        return true;
    };
    if let Some(expected) = filters.provider.as_deref()
        && expected != provider
    {
        return false;
    }
    if let Some(expected) = filters.channel_id.as_deref()
        && expected != channel_id
    {
        return false;
    }
    if let Some(expected) = filters.event_type.as_deref()
        && event_type != Some(expected)
    {
        return false;
    }
    true
}

fn normalize_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn normalized_event_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_EVENT_LIMIT,
        value => value.min(MAX_EVENT_LIMIT),
    }
}

fn normalized_counter_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_COUNTER_LIMIT,
        value => value.min(MAX_COUNTER_LIMIT),
    }
}

fn now_kst() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
pub(crate) async fn flush_for_tests() {
    let Some(sender) = worker_sender() else {
        return;
    };
    let (done_tx, done_rx) = oneshot::channel();
    let _ = sender.send(WorkerMessage::Flush(done_tx));
    let _ = tokio::time::timeout(Duration::from_secs(5), done_rx).await;
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let runtime = runtime();
    runtime.counters.clear();
    if let Ok(mut sender) = runtime.sender.lock() {
        *sender = None;
    }
    if let Ok(mut storage) = runtime.storage.lock() {
        *storage = StorageHandles::default();
    }
}

#[cfg(test)]
pub(crate) fn test_runtime_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn event_flush_persists_records_and_snapshots() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        let db = crate::db::test_db();
        init_observability(db.clone(), None);

        emit_turn_started(
            "codex",
            42,
            Some("dispatch-1"),
            Some("session-1"),
            Some("turn-1"),
        );
        emit_guard_fired(
            "codex",
            42,
            Some("dispatch-1"),
            Some("session-1"),
            Some("turn-1"),
            "review_dispatch_pending",
        );
        emit_turn_finished(
            "codex",
            42,
            Some("dispatch-1"),
            Some("session-1"),
            Some("turn-1"),
            "completed",
            321,
            false,
        );
        flush_for_tests().await;

        let response = query_analytics(
            &db,
            None,
            &AnalyticsFilters {
                provider: Some("codex".to_string()),
                channel_id: Some("42".to_string()),
                ..AnalyticsFilters::default()
            },
        )
        .await
        .expect("query analytics");

        assert_eq!(response.counters.len(), 1);
        assert_eq!(response.counters[0].turn_attempts, 1);
        assert_eq!(response.counters[0].guard_fires, 1);
        assert_eq!(response.counters[0].turn_successes, 1);
        assert!(
            response
                .events
                .iter()
                .any(|event| event.event_type == "turn_started")
        );
        assert!(
            response
                .events
                .iter()
                .any(|event| event.event_type == "turn_finished")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn counter_updates_are_thread_safe() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        let db = crate::db::test_db();
        init_observability(db.clone(), None);

        let iterations = 500usize;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            tasks.push(tokio::spawn(async move {
                for _ in 0..iterations {
                    emit_turn_started("claude", 99, None, None, None);
                }
            }));
        }
        for task in tasks {
            task.await.expect("counter task");
        }
        flush_for_tests().await;

        let response = query_analytics(
            &db,
            None,
            &AnalyticsFilters {
                provider: Some("claude".to_string()),
                channel_id: Some("99".to_string()),
                ..AnalyticsFilters::default()
            },
        )
        .await
        .expect("query analytics");

        assert_eq!(response.counters[0].turn_attempts, (iterations * 8) as u64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_overhead_stays_well_below_hot_path_budget() {
        let _guard = test_runtime_lock();
        reset_for_tests();
        let db = crate::db::test_db();
        init_observability(db, None);

        let iterations = 20_000usize;
        let baseline_start = std::time::Instant::now();
        let mut baseline_acc = 0usize;
        for idx in 0..iterations {
            baseline_acc = baseline_acc.wrapping_add(std::hint::black_box(idx));
        }
        let baseline_elapsed = baseline_start.elapsed();

        let emit_start = std::time::Instant::now();
        for idx in 0..iterations {
            std::hint::black_box(idx);
            emit_turn_started("codex", 7, None, None, None);
        }
        let emit_elapsed = emit_start.elapsed();
        flush_for_tests().await;

        let overhead = emit_elapsed.saturating_sub(baseline_elapsed);
        let overhead_per_emit_ns = overhead.as_nanos() / iterations as u128;
        println!("observability emit overhead: {overhead_per_emit_ns}ns/op");
        assert!(baseline_acc > 0);
        assert!(
            overhead_per_emit_ns < 50_000,
            "emit overhead too high: {overhead_per_emit_ns}ns/op"
        );
    }
}
