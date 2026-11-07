use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum LeaseResult {
    Acquired,
    Held,
    Err,
    AcquiredThenReleased,
    Waiting,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(in crate::services::discord) struct Projection {
    pub tries: u64,
    pub results: BTreeMap<LeaseResult, u64>,
    pub last_result: Option<LeaseResult>,
    pub loop_state: &'static str,
    pub waiter_refreshed_at: Option<String>,
    pub pg_target: String,
}

#[derive(Default)]
pub(in crate::services::discord) struct Diagnostics(Mutex<Projection>);

impl Diagnostics {
    pub fn projection(&self) -> Projection {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn context(&self, pool: &sqlx::PgPool, loop_state: &'static str) {
        let options = pool.connect_options();
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.loop_state = loop_state;
        state.pg_target = format!(
            "{}:{}/{}",
            options.get_host(),
            options.get_port(),
            options.get_database().unwrap_or_default()
        );
    }

    pub fn attempt(&self, event: Option<Result<bool, ()>>) {
        match event {
            None => self.0.lock().unwrap_or_else(|e| e.into_inner()).tries += 1,
            Some(result) => self.record(match result {
                Ok(true) => LeaseResult::Acquired,
                Ok(false) => LeaseResult::Held,
                Err(()) => LeaseResult::Err,
            }),
        }
    }

    pub fn record(&self, result: LeaseResult) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.last_result = Some(result);
        *state.results.entry(result).or_default() += 1;
        tracing::info!(
            last_result = serde_json::to_value(result)
                .unwrap_or_default()
                .as_str()
                .unwrap_or("err"),
            tries = state.tries,
            loop_state = state.loop_state,
            pg_target = state.pg_target,
            "GATEWAY-LEASE: acquisition observation"
        );
    }

    pub fn waiter_refreshed(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.waiter_refreshed_at = Some(chrono::Utc::now().to_rfc3339());
        tracing::info!(
            waiter_refreshed_at = state.waiter_refreshed_at,
            "GATEWAY-LEASE: waiter advertisement completed"
        );
    }
}
