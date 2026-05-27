//! Phase 0 skeleton for the `claude-e` runtime adapter.
//!
//! See `docs/claude-e-rollout/` for the rollout plan and decision log.
//!
//! In Phase 0 this module compiles but is **never selected at runtime**:
//! `provider_hosting::resolve_provider_session_selection_with_channel`
//! still falls back to `ProviderSessionDriver::LegacyPrompt` whenever
//! `runtime: claude-e` is configured, with `fallback_reason =
//! Some("claude_e_adapter_unimplemented")`.
//!
//! Phase 1 will land the real implementation here:
//! - [`process`] — spawn `claude-e run`, stream stdout JSONL, manage stderr.
//! - [`jsonl_parser`] — parse normalized stream-json into AgentDesk
//!   `StreamMessage` records. Phase 1 will decide whether the existing
//!   transcript parser can be reused per `decision-log.md`.
//! - [`cancellation`] — SIGINT/SIGKILL escalation that reaps the child
//!   `claude` process and its MCP server children.
//! - [`spawn_queue`] — per-channel serialization gate so two Discord turns
//!   on the same channel never spawn `claude-e` concurrently.

pub mod cancellation;
pub mod jsonl_parser;
pub mod process;
pub mod spawn_queue;

/// Constant for stable telemetry labelling regardless of binary location.
pub const ADAPTER_LABEL: &str = "claude-e";

/// Phase 0 marker. Phase 1 replaces this with a real availability probe
/// (e.g. `which claude-e` + a `claude-e --version` smoke).
pub fn adapter_available() -> bool {
    false
}
