//! KRX (config/runtime_layout/settings) tests that call root-crate items, declared here.
//! `#[path]` files keep their original cfg chain; the other children hold moved inline tests.

#[path = "config/retired_providers_tests.rs"]
mod retired_providers;
mod runtime_validation_tests;
mod settings_cli_tests;
#[cfg(unix)]
mod skill_sync_shipped_tree_tests;
