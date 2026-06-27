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
pub mod meetings;
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
pub enum DisabledDbBackend {}

#[derive(Debug, Clone, Copy)]
pub struct DisabledDbError;

impl std::fmt::Display for DisabledDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "disabled compatibility database is unavailable")
    }
}

impl std::error::Error for DisabledDbError {}

pub struct DisabledDbConnection;

pub struct DisabledDbRow;

impl DisabledDbBackend {
    pub fn lock(&self) -> Result<DisabledDbConnection, DisabledDbError> {
        Err(DisabledDbError)
    }
}

impl DisabledDbConnection {
    pub fn execute<P>(&self, _sql: &str, _params: P) -> Result<usize, DisabledDbError> {
        Err(DisabledDbError)
    }

    pub fn query_row<P, F, T>(&self, _sql: &str, _params: P, _f: F) -> Result<T, DisabledDbError>
    where
        F: FnOnce(&DisabledDbRow) -> Result<T, DisabledDbError>,
    {
        Err(DisabledDbError)
    }
}

impl DisabledDbRow {
    pub fn get<I, T: Default>(&self, _idx: I) -> Result<T, DisabledDbError> {
        Err(DisabledDbError)
    }
}

pub type Db = Arc<DisabledDbBackend>;
