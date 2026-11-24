//! KRX services tests that call root-crate items, declared here so they leave the KRX crate.
//! Each file stays in place; every `mod` keeps the `cfg(test)` chain of its original declaration.

#[path = "services/cluster/intake_router_hook/o_input/fallback_route_tests.rs"]
mod cluster_fallback_route;
#[path = "services/cluster/intake_worker/o_route_tests.rs"]
mod cluster_intake_worker_o_route;
#[path = "services/cluster/intake_worker/owner_fence_pg_tests.rs"]
mod cluster_intake_worker_owner_fence_pg;
#[path = "services/cluster/intake_worker/replay_hold_tests.rs"]
mod cluster_intake_worker_replay_hold;
#[path = "services/cluster/intake_router_hook/owner_record/event_tests.rs"]
mod cluster_owner_record_event;
#[path = "services/legacy_body_census/entry_tests.rs"]
mod legacy_body_census;
#[path = "services/session_forwarding/probe/tests.rs"]
mod session_forwarding_probe;
