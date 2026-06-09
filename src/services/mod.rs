pub mod agent_protocol;
pub mod agent_quality;
pub mod agents;
pub mod analytics;
pub mod api_friction;
// #3034: 29 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during auto_queue dead-code cleanup.
#[allow(dead_code)]
pub mod auto_queue;
pub mod automation_candidate_contract;
pub mod automation_candidate_materializer;
// #3034: 5 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during claude dead-code cleanup.
#[allow(dead_code)]
pub mod claude;
pub mod claude_e;
// #3034: 16 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during claude_tui dead-code cleanup.
#[allow(dead_code)]
pub mod claude_tui;
// #3034: 15 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during cluster dead-code cleanup.
#[allow(dead_code)]
pub mod cluster;
// #3034: 10 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during codex dead-code cleanup.
#[allow(dead_code)]
pub mod codex;
pub mod codex_remote_policy;
#[cfg(unix)]
pub mod codex_tmux_wrapper;
// #3034: 31 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during codex_tui dead-code cleanup.
#[allow(dead_code)]
pub mod codex_tui;
// #3034: 113 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during discord dead-code cleanup.
#[allow(dead_code)]
pub mod discord;
// #3034: 3 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during discord_config_audit dead-code cleanup.
#[allow(dead_code)]
pub mod discord_config_audit;
// #1693: `discord_delivery` moved to `dispatches::discord_delivery`. The
// flat path is preserved as a re-export so existing import sites and
// tests keep working without churn.
#[allow(unused_imports)]
pub(crate) use dispatches::discord_delivery;
pub mod discord_dm_reply_store;
pub mod disk_monitor;
pub mod dispatch_watchdog;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during dispatched_sessions dead-code cleanup.
#[allow(dead_code)]
pub mod dispatched_sessions;
// #3034: 23 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during dispatches dead-code cleanup.
#[allow(dead_code)]
pub mod dispatches;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during dispatches_followup dead-code cleanup.
#[allow(dead_code)]
pub mod dispatches_followup;
// #3034: 3 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during envelope_dedup dead-code cleanup.
#[allow(dead_code)]
pub mod envelope_dedup;
pub mod escalation_settings;
// #3034: 4 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during gemini dead-code cleanup.
#[allow(dead_code)]
pub mod gemini;
// #3034: 2 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during git dead-code cleanup.
#[allow(dead_code)]
pub mod git;
pub mod issue_announcements;
pub mod kanban;
pub mod kanban_cards;
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
pub mod monitoring_store;
// #3034: 24 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during observability dead-code cleanup.
#[allow(dead_code)]
pub mod observability;
pub mod onboarding;
pub mod opencode;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during operator_connectors dead-code cleanup.
#[allow(dead_code)]
pub mod operator_connectors;
pub mod pipeline_override;
pub mod pipeline_routes;
// #3034: 5 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during platform dead-code cleanup.
#[allow(dead_code)]
pub mod platform;
// #3034: 2 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during pr_summary dead-code cleanup.
#[allow(dead_code)]
pub mod pr_summary;
// #3034: 14 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during process dead-code cleanup.
#[allow(dead_code)]
pub mod process;
// #3034: 8 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during provider dead-code cleanup.
#[allow(dead_code)]
pub mod provider;
pub mod provider_auth;
// #3034: 18 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during provider_cli dead-code cleanup.
#[allow(dead_code)]
pub mod provider_cli;
pub mod provider_exec;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during provider_hosting dead-code cleanup.
#[allow(dead_code)]
pub mod provider_hosting;
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
// #3034: 5 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during routines dead-code cleanup.
#[allow(dead_code)]
pub mod routines;
pub mod service_error;
pub mod session_activity;
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
// #3034: 10 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during turn_orchestrator dead-code cleanup.
#[allow(dead_code)]
pub mod turn_orchestrator;

// Compatibility alias: code referencing services::remote::* uses the stub
pub use remote_stub as remote;
