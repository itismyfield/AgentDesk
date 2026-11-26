//! The unix-only KE queue and session tests that call ROOT discord health or server routes; declared under
//! `all(test, unix)` like their original declarations so each keeps its test ID cfg chain.

#[path = "../../dispatched_sessions/raw_alias_pg_tests.rs"]
mod dispatched_sessions_raw_alias_pg;
#[path = "../../queue/cancel_effects_v2_tests.rs"]
mod queue_cancel_effects_v2;
