pub mod agent_protocol;
pub mod agent_quality;
pub mod agents;
pub mod analytics;
pub mod api_friction;
pub mod auto_queue;
pub mod automation_candidate_contract;
pub mod automation_candidate_materializer;
pub mod claude;
pub mod claude_compact_trigger;
pub mod claude_e;
pub mod claude_tui;
pub mod cluster;
pub mod codex;
pub mod codex_remote_policy;
#[cfg(unix)]
pub mod codex_tmux_wrapper;
pub mod codex_tui;
pub mod cswap;
pub mod discord;
pub mod discord_config_audit;
// #1693: `discord_delivery` moved to `dispatches::discord_delivery`. The
// flat path is preserved as a re-export so existing import sites and
// tests keep working without churn.
#[allow(unused_imports)]
pub(crate) use dispatches::discord_delivery;
pub mod discord_dm_reply_store;
pub mod disk_monitor;
pub mod dispatch_gate;
pub mod dispatch_watchdog;
pub mod dispatched_sessions;
pub mod dispatches;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during dispatches_followup dead-code cleanup.
#[allow(dead_code)]
pub mod dispatches_followup;
pub mod escalation_settings;
pub mod gemini;
pub mod git;
pub mod github_issue_creation;
pub mod health_active_session_audit;
pub mod health_diagnostics;
pub mod issue_announcements;
pub mod kanban;
pub mod kanban_cards;
pub mod long_turn_watchdog;
// #3034: 81 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during maintenance dead-code cleanup.
#[allow(dead_code)]
pub mod maintenance;
pub mod mcp_config;
pub mod memory;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during message_outbox dead-code cleanup.
#[allow(dead_code)]
pub mod message_outbox;
pub mod message_outbox_recovery;
pub(crate) mod message_outbox_recovery_support;
#[cfg(test)]
mod message_outbox_recovery_tests;
pub mod monitoring_store;
pub mod observability;
pub mod onboarding;
pub mod opencode;
pub(crate) mod pane_readiness;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during operator_connectors dead-code cleanup.
#[allow(dead_code)]
pub mod operator_connectors;
pub mod pipeline_override;
pub mod pipeline_routes;
pub mod platform;
// #3034: 2 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during pr_summary dead-code cleanup.
#[allow(dead_code)]
pub mod pr_summary;
pub mod process;
// #3034: residual dead code here is a dormant recovery-priming constructor;
// scoped allow retained pending provider recovery cleanup. The old #2662/#2668
// envelope/dev-role dedup stubs were retired by #3744.
#[allow(dead_code)]
pub mod provider;
pub mod provider_auth;
pub mod provider_cli;
pub mod provider_exec;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during provider_hosting dead-code cleanup.
#[allow(dead_code)]
pub mod provider_hosting;
pub(crate) mod provider_output_guard;
pub mod provider_runtime;
pub mod queue;
pub mod qwen;
#[cfg(unix)]
pub mod qwen_tmux_wrapper;
// #3034: 2 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during remote_stub dead-code cleanup.
#[allow(dead_code)]
pub mod remote_stub;
pub mod retrospectives;
pub mod review_decision;
pub mod routines;
pub mod service_error;
pub mod session_activity;
pub(crate) mod session_selector_validity;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during session_backend dead-code cleanup.
#[allow(dead_code)]
pub mod session_backend;
pub mod session_forwarding;
pub mod settings;
pub mod shell_guard;
pub mod slo;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during termination_audit dead-code cleanup.
#[allow(dead_code)]
pub mod termination_audit;
pub mod tmux_common;
pub mod tmux_diagnostics;
#[cfg(unix)]
pub mod tmux_wrapper;
pub mod tool_output_guard;
// #3034: 4 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during tui_prompt_dedupe dead-code cleanup.
#[allow(dead_code)]
pub(crate) mod tui_prompt_dedupe;
pub(crate) mod tui_turn_state;
pub mod turn_cancel_finalizer;
pub mod turn_lifecycle;
pub mod turn_orchestrator;

// Compatibility alias only: code referencing `services::remote::*` still
// compiles, but the target module is disabled per #1606/#2175/#2193. A real
// `services::remote` must replace `remote_stub` only when the ADR prerequisites
// in `docs/codex-remote-ssh-policy.md` are satisfied.
pub use remote_stub as remote;

#[cfg(test)]
mod provider_output_guard_tests;
