//! ROOT-declared routine and scheduled-message tests that reach ROOT discord health or routes.
#[path = "services/routines/agent_executor/execution_pg_tests.rs"]
mod agent_executor_execution_pg;
#[path = "services/routines/agent_executor/owner_input_tests.rs"]
mod agent_executor_owner_input;
#[path = "services/routines/agent_executor/provider_choice_tests.rs"]
mod agent_executor_provider_choice;
#[cfg(unix)]
#[path = "services/routines/agent_executor/reliability_probe_pg_tests.rs"]
mod agent_executor_reliability_probe_pg;
#[path = "services/routines/execution_mode_pg_tests.rs"]
mod execution_mode_pg;
#[path = "services/routines/reliability_pg_tests.rs"]
mod reliability_pg;
#[path = "services/scheduled_messages/postgres_tests.rs"]
mod scheduled_messages_postgres;
