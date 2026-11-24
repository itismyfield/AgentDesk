//! KRX (memory/codex_tui/compact/observability/provider_cli) tests that call discord-layer items.
//! `#[path]` files keep their original cfg chain; the other children hold moved inline tests.

mod codex_submission_tests;
#[cfg(unix)]
mod codex_warm_followup_tests;
mod compact_trigger_tests;
#[path = "../memory/memento_scope_tests.rs"]
mod memento_scope_tests;
mod observability_cancel_tests;
#[cfg(unix)]
mod provider_cli_session_guard_tests;
