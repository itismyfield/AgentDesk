//! KRX (tui_o) test files originally declared under `cfg(all(test, unix))` that call root-crate
//! items; a separate host keeps that single cfg predicate so the declaration key stays the same.

#[path = "services/tui_o/writer/progress_drain_verified_tests.rs"]
mod progress_drain_verified;
#[path = "services/tui_o/store/thread_children_tests.rs"]
mod store_thread_children;
