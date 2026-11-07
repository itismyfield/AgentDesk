use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

const MINUTE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum LeaseResult {
    Acquired,
    Held,
    Err,
    AcquiredThenReleased,
    Waiting,
    RefusedWhileFree,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub(in crate::services::discord) struct Holder {
    pub pid: Option<i32>,
    pub client_addr: Option<String>,
    pub backend_start: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum SnapshotStatus {
    #[default]
    Unknown,
    Held,
    Free,
    Err,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(in crate::services::discord) struct Projection {
    pub provider: String,
    pub tries: u64,
    pub results: BTreeMap<LeaseResult, u64>,
    pub last_result: Option<LeaseResult>,
    pub loop_state: &'static str,
    pub waiter_refreshed_at: Option<String>,
    pub pg_target: String,
    pub holder: Option<Holder>,
    pub snapshot_at: Option<String>,
    pub snapshot_status: SnapshotStatus,
    pub snapshot_attempts: u64,
}

#[derive(Default)]
struct State {
    view: Projection,
    next_snapshot: Option<Instant>,
    next_line: Option<Instant>,
    failure_result: Option<LeaseResult>,
    pending: bool,
    in_flight: bool,
    revision: u64,
}

#[derive(Default)]
pub(in crate::services::discord) struct Diagnostics(Mutex<State>);

struct Reservation<'a> {
    diagnostics: &'a Diagnostics,
    completed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut state = self.diagnostics.0.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight = false;
        state.next_snapshot = Some(Instant::now() + MINUTE);
        state.pending |= !self.completed;
    }
}

impl Diagnostics {
    pub fn projection(&self) -> Projection {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .view
            .clone()
    }

    pub fn context(&self, pool: &sqlx::PgPool, loop_state: &'static str, provider: &str) {
        let options = pool.connect_options();
        let target = format!(
            "{}:{}/{}",
            options.get_host(),
            options.get_port(),
            options.get_database().unwrap_or_default()
        );
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.view.pg_target != target {
            state.revision += 1;
            state.pending = true;
            state.view.holder = None;
            state.view.snapshot_at = None;
            state.view.snapshot_status = SnapshotStatus::Unknown;
        }
        state.view.provider = provider.to_owned();
        state.view.loop_state = loop_state;
        state.view.pg_target = target;
    }

    pub fn attempt(&self, event: Option<Result<bool, ()>>) {
        match event {
            None => self.0.lock().unwrap_or_else(|e| e.into_inner()).view.tries += 1,
            Some(result) => self.record(match result {
                Ok(true) => LeaseResult::Acquired,
                Ok(false) => LeaseResult::Held,
                Err(()) => LeaseResult::Err,
            }),
        }
    }

    pub fn record(&self, result: LeaseResult) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.revision += 1;
        let transitioned = state.view.last_result != Some(result);
        state.pending |= transitioned;
        state.view.last_result = Some(result);
        *state.view.results.entry(result).or_default() += 1;
        if result == LeaseResult::Acquired {
            tracing::info!(
                provider = state.view.provider,
                last_result = "acquired",
                pg_target = state.view.pg_target,
                "GATEWAY-LEASE: PostgreSQL granted advisory"
            );
        }
    }

    pub fn adopted(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.failure_result = None;
        state.pending = false;
    }

    fn reserve(&self, now: Instant, result: LeaseResult) -> Option<(u64, String, bool)> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let new_streak = state.failure_result.replace(result).is_none();
        state.pending |= new_streak;
        if state.in_flight || state.next_snapshot.is_some_and(|next| now < next) {
            return None;
        }
        state.next_snapshot = Some(now + MINUTE);
        state.in_flight = true;
        state.view.snapshot_attempts += 1;
        let triggered = std::mem::take(&mut state.pending);
        Some((state.revision, state.view.pg_target.clone(), triggered))
    }

    pub async fn failure(
        &self,
        pool: &sqlx::PgPool,
        key: i64,
        result: LeaseResult,
        short_circuit: bool,
    ) {
        let Some((revision, target, triggered)) = self.reserve(Instant::now(), result) else {
            self.summary();
            return;
        };
        // Cancellation releases the in-flight fence, while retaining the reserved minute budget.
        let mut reservation = Reservation {
            diagnostics: self,
            completed: false,
        };
        // Prepared transactions can retain a lock without an activity row.
        let observed = sqlx::query_as::<_, Holder>(
            "SELECT l.pid, a.client_addr::text AS client_addr, a.backend_start
               FROM pg_locks l LEFT JOIN pg_stat_activity a ON a.pid = l.pid
              WHERE l.locktype = 'advisory' AND l.granted
                AND l.classid = (($1::BIGINT >> 32) & 4294967295)::OID
                AND l.objid = ($1::BIGINT & 4294967295)::OID AND l.objsubid = 1
                AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
              LIMIT 1",
        )
        .bind(key)
        .fetch_optional(pool)
        .await;
        {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if state.view.pg_target != target {
                state.pending = true;
                return;
            }
            state.view.snapshot_at = Some(chrono::Utc::now().to_rfc3339());

            match observed {
                Ok(holder) => {
                    state.view.snapshot_status = if holder.is_some() {
                        SnapshotStatus::Held
                    } else {
                        SnapshotStatus::Free
                    };
                    let changed = state
                        .view
                        .holder
                        .as_ref()
                        .map(|h| (h.pid, &h.backend_start))
                        != holder.as_ref().map(|h| (h.pid, &h.backend_start));
                    // Only this observation can establish free; a cached free value is never evidence.
                    if short_circuit && holder.is_none() && state.revision == revision {
                        state.view.last_result = Some(LeaseResult::RefusedWhileFree);
                        *state
                            .view
                            .results
                            .entry(LeaseResult::RefusedWhileFree)
                            .or_default() += 1;
                    }
                    state.view.holder = holder;
                    if triggered || changed {
                        tracing::info!(
                            ?result,
                            triggered,
                            holder_identity_changed = changed,
                            snapshot = ?state.view,
                            "GATEWAY-LEASE: holder snapshot"
                        );
                    }
                }
                Err(error) => {
                    state.view.snapshot_status = SnapshotStatus::Err;
                    state.pending = true;
                    state.view.holder = None;
                    tracing::warn!(
                        provider = state.view.provider,
                        %error,
                        "GATEWAY-LEASE: holder snapshot unavailable"
                    );
                }
            }
        }
        reservation.completed = true;
        drop(reservation);
        self.summary();
    }

    pub fn summary(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if state.next_line.is_some_and(|next| now < next) {
            return;
        }
        state.next_line = Some(now + MINUTE);
        let result = serde_json::to_value(state.view.last_result).unwrap_or_default();
        tracing::info!(
            tries = state.view.tries,
            last_result = result.as_str().unwrap_or("unknown"),
            diagnostic = ?state.view,
            "⏳ GATEWAY-LEASE: waiting for adoption"
        );
    }

    pub fn waiter_refreshed(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.view.waiter_refreshed_at = Some(chrono::Utc::now().to_rfc3339());
        tracing::info!(
            provider = state.view.provider,
            waiter_refreshed_at = state.view.waiter_refreshed_at,
            "GATEWAY-LEASE: waiter refresh completed"
        );
    }
}

#[cfg(test)]
#[path = "gateway_lease_diagnostics_tests.rs"]
mod tests;
