//! KRX config tests under `cfg(all(test, unix))` that call root-crate items;
//! a separate host keeps that single cfg predicate so each declaration key stays the same.

#[path = "config/test_instance/api_boundary_tests.rs"]
mod api_boundary_tests;
#[path = "config/test_instance/bound_health_tests.rs"]
mod bound_health_tests;
#[path = "config/daemon_snapshot_pg_tests.rs"]
mod daemon_snapshot_pg_tests;
#[path = "config/test_instance/environment_tests.rs"]
mod environment_tests;
#[path = "config/test_instance/lifecycle_tests.rs"]
mod lifecycle_tests;
