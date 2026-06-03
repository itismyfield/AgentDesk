pub mod agents;
pub mod auto_queue;
pub mod automation_candidates;
pub mod cancel_tombstones;
pub mod dispatch_semaphores;
pub mod dispatched_sessions;
pub mod dispatches;
pub mod idempotency;
pub mod intake_outbox;
pub mod kanban;
pub mod kanban_cards;
pub mod memento_feedback_stats;
pub mod postgres;
pub mod prompt_manifests;
pub(crate) mod session_agent_resolution;
pub mod session_observability;
pub mod session_status;
pub mod session_transcripts;
pub mod table_metadata;
pub mod turns;

use std::sync::Arc;

#[derive(Debug)]
pub enum LegacySqliteDisabled {}

#[derive(Debug, Clone, Copy)]
pub struct LegacySqliteError;

impl std::fmt::Display for LegacySqliteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "legacy sqlite backend is unavailable in production")
    }
}

impl std::error::Error for LegacySqliteError {}

pub struct LegacySqliteConnection;

// reason: production-side shim mirroring the legacy-sqlite-tests API surface;
// constructed only under the legacy-sqlite-tests feature. See #3034 / #3035.
#[allow(dead_code)]
pub struct LegacySqliteStatement;

// reason: production-side shim mirroring the legacy-sqlite-tests API surface;
// constructed only under the legacy-sqlite-tests feature. See #3034 / #3035.
#[allow(dead_code)]
pub struct LegacySqliteRows;

pub struct LegacySqliteRow;

impl LegacySqliteDisabled {
    pub fn lock(&self) -> Result<LegacySqliteConnection, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    // reason: legacy-sqlite API parity shim; exercised only under
    // legacy-sqlite-tests. See #3034 / #3035.
    #[allow(dead_code)]
    pub fn read_conn(&self) -> Result<LegacySqliteConnection, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    // reason: legacy-sqlite API parity shim; exercised only under
    // legacy-sqlite-tests. See #3034 / #3035.
    #[allow(dead_code)]
    pub fn separate_conn(&self) -> Result<LegacySqliteConnection, LegacySqliteError> {
        Err(LegacySqliteError)
    }
}

impl LegacySqliteConnection {
    pub fn execute<P>(&self, _sql: &str, _params: P) -> Result<usize, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    // reason: legacy-sqlite API parity shim; exercised only under
    // legacy-sqlite-tests. See #3034 / #3035.
    #[allow(dead_code)]
    pub fn execute_batch(&self, _sql: &str) -> Result<(), LegacySqliteError> {
        Err(LegacySqliteError)
    }

    // reason: legacy-sqlite API parity shim; exercised only under
    // legacy-sqlite-tests. See #3034 / #3035.
    #[allow(dead_code)]
    pub fn prepare(&self, _sql: &str) -> Result<LegacySqliteStatement, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    pub fn query_row<P, F, T>(&self, _sql: &str, _params: P, _f: F) -> Result<T, LegacySqliteError>
    where
        F: FnOnce(&LegacySqliteRow) -> Result<T, LegacySqliteError>,
    {
        Err(LegacySqliteError)
    }
}

impl LegacySqliteStatement {
    // reason: legacy-sqlite API parity shim; exercised only under
    // legacy-sqlite-tests. See #3034 / #3035.
    #[allow(dead_code)]
    pub fn query<P>(&mut self, _params: P) -> Result<LegacySqliteRows, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    // reason: legacy-sqlite API parity shim; exercised only under
    // legacy-sqlite-tests. See #3034 / #3035.
    #[allow(dead_code)]
    pub fn query_map<P, F, T>(
        &mut self,
        _params: P,
        _f: F,
    ) -> Result<std::vec::IntoIter<Result<T, LegacySqliteError>>, LegacySqliteError>
    where
        F: FnMut(&LegacySqliteRow) -> Result<T, LegacySqliteError>,
    {
        Err(LegacySqliteError)
    }
}

impl LegacySqliteRows {
    // reason: legacy-sqlite API parity shim; exercised only under
    // legacy-sqlite-tests. See #3034 / #3035.
    #[allow(dead_code)]
    pub fn next(&mut self) -> Result<Option<LegacySqliteRow>, LegacySqliteError> {
        Err(LegacySqliteError)
    }
}

impl LegacySqliteRow {
    pub fn get<I, T: Default>(&self, _idx: I) -> Result<T, LegacySqliteError> {
        Err(LegacySqliteError)
    }
}

pub type Db = Arc<LegacySqliteDisabled>;
