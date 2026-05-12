//! Split routes_tests module (was a single 31,548-line file).
//!
//! All previous helpers, fixtures and `sqlite_params!` macro live in `common`.
//! Tests are partitioned into domain-specific submodules; their contents and
//! semantics are unchanged from the original `routes_tests.rs`.

// `sqlite_params!` is defined at module-root so it is in lexical scope for
// every submodule declared below (sibling modules see macros declared earlier
// in their parent module). This preserves the original macro semantics
// without needing `#[macro_use]` or `#[macro_export]`.
macro_rules! sqlite_params {
    ($($param:expr),* $(,)?) => {
        ($(&$param,)*)
    };
}

mod common;

mod infra_tests;
mod health_tests;
mod agents_tests;
mod kanban_tests;
mod dispatch_tests;
mod github_tests;
mod auto_queue_tests;
mod api_docs_tests;
