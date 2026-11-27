//! ROOT-declared KE turn tests that reach ROOT discord health, routes or inflight lookups.
#[path = "../services/turn_orchestrator/queued_head/v2/origin_tests.rs"]
mod queued_head_v2_origin;
#[path = "../services/session_resume/host_rebind_tests.rs"]
mod session_resume_host_rebind;
#[path = "../services/stale_turn_reconciler/root_lookup_tests.rs"]
mod stale_turn_reconciler_root_lookup;
#[cfg(unix)]
#[path = "../services/turn_lifecycle/host_guard_tests.rs"]
mod turn_lifecycle_host_guard;
#[path = "../services/turn_lifecycle/queue_truth_tests.rs"]
mod turn_lifecycle_queue_truth;
