//! KRX (logging/observability/platform) tests that call KE items, declared here to leave KRX.
//! `#[path]` files keep their original cfg chain; the other children hold moved inline tests.

#[path = "../logging/auto_queue_tests.rs"]
mod auto_queue_tests;
#[path = "observability/events/capture_stress_tests.rs"]
mod capture_stress_tests;
mod logging_filter_tests;
#[cfg(unix)]
mod tmux_readiness_tests;
