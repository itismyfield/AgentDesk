pub mod action;
/// Routines core runtime — durable store plus script-only runtime pieces.
///
/// This module is intentionally small at this stage. It exposes only
/// `RoutineStore` exposes PG-backed claim/finish/recovery transactions.
/// `RoutineScriptLoader` and `RoutineAction` are the ORDER-P0-002 foundation;
/// the worker tick loop and `/api/routines` route integration use the typed
/// runtime/store boundary exposed here.
///
/// SQLite users: this module is never instantiated when `pg_pool` is `None`,
/// so there are zero side effects for non-PG deployments.
pub mod agent_executor;
pub mod discord_log;
pub mod fresh_session_reaper;
pub mod loader;
pub mod migrated;
pub mod runtime;
pub mod runtime_config;
pub mod session_control;
pub mod store;

pub use action::{RoutineAction, validate_routine_action};
pub use agent_executor::RoutineAgentExecutor;
pub use discord_log::{RoutineDiscordLogger, RoutineLifecycleEvent};
pub use loader::RoutineScriptLoader;
pub use migrated::{is_migrated_launchd_script_ref, validate_migrated_launchd_activation};
pub use runtime::{execute_claimed_script_run, poll_agent_turns, run_due_tick};
pub use runtime_config::validate_routine_runtime_config;
pub use session_control::{RoutineSessionCommand, RoutineSessionController};
pub use store::{
    DeleteRoutineResult, NewRoutine, RoutinePatch, RoutineStore,
    is_resume_routine_requires_next_due_at, validate_routine_schedule,
};

pub(crate) fn fresh_context_guaranteed(
    execution_strategy: &str,
    provider_turn_started: bool,
) -> bool {
    provider_turn_started && execution_strategy == "fresh"
}

#[cfg(test)]
mod tests {
    use super::fresh_context_guaranteed;

    #[test]
    fn fresh_context_guarantee_requires_verified_provider_start() {
        assert!(!fresh_context_guaranteed("fresh", false));
        assert!(fresh_context_guaranteed("fresh", true));
        assert!(!fresh_context_guaranteed("persistent", true));
    }
}
