//! KRX (tui_o) tests that call root-crate items, declared here so they leave the KRX crate.
//! Each `#[path]` file stays in place and keeps the cfg chain of its original declaration.

#[cfg(unix)]
#[path = "services/tui_o/writer/transition_apply_tests.rs"]
mod apply_tests;
#[cfg(unix)]
#[path = "services/tui_o/writer/deferred_pg_tests.rs"]
mod deferred_pg;
#[cfg(unix)]
#[path = "services/tui_o/writer/progress_drain_actor_tests.rs"]
mod progress_drain_actor;
#[cfg(unix)]
#[path = "services/tui_o/writer/transition_route_tests.rs"]
mod route_tests;
#[cfg(unix)]
#[path = "services/tui_o/writer/transition_automatic_tests.rs"]
mod transition_automatic;
