pub(crate) mod agent_channels;
pub use adk_runtime::services::agent_identity;
pub use adk_runtime::services::agent_protocol;
pub use adk_runtime::services::agent_recovery;
pub mod agents;
pub(crate) use adk_runtime::services::agy_acp;
pub use adk_runtime::services::analytics;
pub mod api_friction;
pub mod auto_queue;
pub mod automation_candidate_contract;
pub mod automation_candidate_materializer;
pub mod campaign_execution;
pub(crate) use adk_runtime::services::channel_roles;
pub use adk_runtime::services::claude;
pub(crate) use adk_runtime::services::claude_command;
pub use adk_runtime::services::claude_compact_context;
pub use adk_runtime::services::claude_compact_trigger;
pub use adk_runtime::services::claude_tui;
pub use adk_runtime::services::cluster;
pub use adk_runtime::services::codex;
pub mod codex_tmux_wrapper;
pub use adk_runtime::services::codex_tui;
// Storage primitives remain dormant until the input actor is wired.
pub use adk_runtime::services::cswap;
pub(crate) use adk_runtime::services::dcserver_singleton;
pub mod decision_gate;
pub mod discord;
pub mod discord_config_audit;
pub(crate) use adk_runtime::services::tui_input;
// #1693: `discord_delivery` moved to `dispatches::discord_delivery`. The
// flat path is preserved as a re-export so existing import sites and
// tests keep working without churn.
#[allow(unused_imports)]
pub(crate) use dispatches::discord_delivery;
pub(crate) mod calendar_sync;
pub mod discord_dm_reply_store;
pub use adk_runtime::services::disk_monitor;
pub use adk_runtime::services::dispatch_gate;
pub mod dispatch_watchdog;
pub mod dispatched_sessions;
pub mod dispatches;
pub mod dispatches_followup;
pub(crate) mod engine_runtime;
pub(crate) mod escalation_cooldown;
pub mod explicit_auth_route;
pub use adk_runtime::services::git;
pub mod github_issue_creation;
pub(crate) mod hang_forensics;
pub mod health_diagnostics;
pub(crate) use adk_runtime::services::herdr_admission;
pub(crate) use adk_runtime::services::herdr_launch;
pub(crate) mod imsg_reply;
pub mod issue_announcements;
pub mod kakao;
pub mod kakao_message;
pub mod kanban;
pub mod kanban_cards;
pub mod maintenance;
pub(crate) mod manual_steer;
pub use adk_runtime::services::mcp_config;
pub use adk_runtime::services::memory;
pub use adk_runtime::services::message_outbox;
pub(crate) mod message_outbox_circuit_authority;
#[cfg(test)]
mod message_outbox_circuit_authority_tests;
pub mod message_outbox_recovery;
pub(crate) mod message_outbox_recovery_support;
#[cfg(test)]
mod message_outbox_recovery_tests;
pub use adk_runtime::services::monitoring_store;
pub use adk_runtime::services::observability;
pub mod onboarding;
pub use adk_runtime::services::opencode;
pub(crate) use adk_runtime::services::org_catalog;
pub(crate) use adk_runtime::services::pane_readiness;
// #3034: 1 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during operator_connectors dead-code cleanup.
#[allow(dead_code)]
pub mod operator_connectors;
pub use adk_runtime::services::pipeline_override;
pub use adk_runtime::services::pipeline_routes;
pub use adk_runtime::services::platform;
// #3034: 2 residual dead-code items; scoped here so the lint stays
// live on clean sibling modules. Remove during pr_summary dead-code cleanup.
pub(crate) use adk_runtime::services::legacy_body_census;
#[allow(dead_code)]
pub mod pr_summary;
pub use adk_runtime::services::process;
pub(crate) use adk_runtime::services::process_boot;
pub use adk_runtime::services::provider;
pub use adk_runtime::services::provider_auth;
pub use adk_runtime::services::provider_auth_profile;
pub use adk_runtime::services::provider_cli;
pub(crate) mod provider_error_transcript;
pub mod provider_exec;
pub use adk_runtime::services::provider_hosting;
pub(crate) use adk_runtime::services::provider_output_guard;
pub mod provider_runtime;
pub(crate) use adk_runtime::services::provider_teardown;
pub mod queue;
pub mod qwen;
pub mod qwen_tmux_wrapper;
// Scheduling contracts remain unavailable to runtime callers until activation.
#[cfg(test)]
pub(crate) mod recovery_defer;
pub(crate) use adk_runtime::services::release_source;
pub use adk_runtime::services::remote_stub;
pub(crate) use adk_runtime::services::resident_loops;
pub mod retrospectives;
pub mod review_decision;
pub mod routines;
pub(crate) mod running_binary;
pub mod scheduled_messages;
pub(crate) mod scheduling;
pub mod service_error;
pub mod session_activity;
pub(crate) mod session_selector_validity;
pub use adk_runtime::services::session_backend;
pub use adk_runtime::services::session_forwarding;
pub use adk_runtime::services::session_host;
pub use adk_runtime::services::stream_json_cli;
pub mod session_resume;
pub use adk_runtime::services::settings;
pub mod slo;
pub(crate) mod stale_turn_reconciler;
pub(crate) use adk_runtime::services::task_completion_v1;
pub(crate) use adk_runtime::services::terminal_status_formatting;
pub use adk_runtime::services::termination_audit;
pub use adk_runtime::services::tmux_common;
pub use adk_runtime::services::tmux_diagnostics;
pub(crate) mod tmux_turn_liveness;
pub mod tmux_wrapper;
pub mod tool_output_guard;
// Output track: the shadow and the dormant writer store, each behind an off-by-default flag.
pub(crate) mod tui_o {
    pub(crate) mod cutover {
        pub(crate) use adk_runtime::services::tui_o::cutover::*;
        #[cfg(all(test, unix))]
        mod child_funnel_tests;
    }
    pub(crate) use adk_runtime::services::tui_o::{
        alarm, channel_policy, exact_episode, exact_pg, exact_submission, ownership, repost,
        shadow, store, topology, turn_mode, writer,
    };
}
pub(crate) use adk_runtime::services::tui_prompt_control;
pub(crate) use adk_runtime::services::tui_prompt_dedupe;
pub(crate) use adk_runtime::services::tui_turn_state;
pub mod turn_cancel_finalizer;
pub mod turn_cancel_queue_guard;
pub(crate) use adk_runtime::services::turn_host;
pub mod turn_lifecycle;
pub mod turn_orchestrator;
pub(crate) use adk_runtime::services::turn_summary_line;

#[cfg(test)]
mod krx_claude_tests;
#[cfg(test)]
mod krx_config_tests;
#[cfg(test)]
mod krx_db_b_tests;
#[cfg(test)]
mod krx_db_tests;
#[cfg(all(test, unix))]
mod krx_db_unix_tests;
#[cfg(test)]
mod provider_output_guard_tests;

pub(crate) use adk_runtime::services::session_identity_metadata;
