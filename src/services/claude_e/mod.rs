//! Phase 1 entry point for the `claude-e` runtime adapter.
//!
//! See `docs/claude-e-rollout/` for the rollout plan and decision log.

pub mod cancellation;
pub mod jsonl_parser;
pub mod process;
pub mod spawn_queue;

pub use process::execute_streaming;
