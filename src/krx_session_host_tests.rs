//! KRX session_host tests that call root-crate items, declared here so they leave the KRX crate.
//! Each `#[path]` file stays in place; every `mod` keeps the cfg chain of its original declaration.

// The operator terminate path against real Herdr servers its runner starts; ignored otherwise.
#[path = "services/session_host/herdr_terminate_real_tests.rs"]
pub(crate) mod herdr_terminate_real_tests;
#[cfg(unix)]
#[path = "services/session_host/herdr/launch_host_real_tests.rs"]
mod launch_host_real;
#[cfg(unix)]
#[path = "services/session_host/herdr/managed_config/maintenance_tests.rs"]
pub(crate) mod managed_config_maintenance;
