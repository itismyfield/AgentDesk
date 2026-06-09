//! Phase 1 entry point for the `claude-e` runtime adapter.
//!
//! See `docs/claude-e-rollout/` for the rollout plan and decision log.

pub mod cancellation;
pub mod jsonl_parser;
pub mod process;
pub mod spawn_queue;

pub use process::execute_streaming;

/// Returns true when the `claude-e` binary can be located on PATH.
/// `provider_hosting::resolve_provider_session_selection_with_channel`
/// reads this to decide whether the operator's `runtime: claude-e`
/// request can be honoured or must fall back to `LegacyPrompt`.
pub fn adapter_available() -> bool {
    which::which("claude-e").is_ok()
}
