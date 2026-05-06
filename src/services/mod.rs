pub mod agent_protocol;
pub mod agent_quality;
pub mod agents;
pub mod analytics;
pub mod api_friction;
pub mod auto_queue;
pub mod claude;
pub mod codex;
#[cfg(unix)]
pub mod codex_tmux_wrapper;
pub mod discord;
pub mod discord_config_audit;
// #1693: `discord_delivery` moved to `dispatches::discord_delivery`. The
// flat path is preserved as a re-export so existing import sites and
// tests keep working without churn.
pub(crate) use dispatches::discord_delivery;
pub mod discord_dm_reply_store;
pub mod disk_monitor;
pub mod dispatch_watchdog;
pub mod dispatched_sessions;
pub mod dispatches;
pub mod dispatches_followup;
pub mod gemini;
pub mod git;
pub mod issue_announcements;
pub mod kanban;
pub mod kanban_cards;
pub mod maintenance;
pub mod mcp_config;
pub mod memory;
pub mod message_outbox;
pub mod observability;
pub mod onboarding;
pub mod opencode;
pub mod pipeline_override;
pub mod pipeline_routes;
pub mod platform;
pub mod process;
pub mod provider;
pub mod provider_cli;
pub mod provider_exec;
pub mod provider_runtime;
pub mod queue;
pub mod qwen;
#[cfg(unix)]
pub mod qwen_tmux_wrapper;
pub mod remote_stub;
pub mod retrospectives;
pub mod routines;
pub mod service_error;
pub mod session_backend;
pub mod session_forwarding;
pub mod settings;
pub mod shell_guard;
pub mod slo;
pub mod termination_audit;
pub mod tmux_common;
pub mod tmux_diagnostics;
#[cfg(unix)]
pub mod tmux_wrapper;
pub mod tool_output_guard;
pub mod turn_cancel_finalizer;
pub mod turn_lifecycle;
pub mod turn_orchestrator;

// Compatibility alias: code referencing services::remote::* uses the stub
pub use remote_stub as remote;
