//! Runtime crate built from the files in place; the parent re-exports each module under its old path.
#![recursion_limit = "256"]
#![deny(clippy::drop_non_drop)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::mistyped_literal_suffixes)]
#![deny(clippy::correctness)]
// The parent's crate-wide style/complexity debt list, carried with the modules it covered.
#![allow(
    clippy::absurd_extreme_comparisons,
    clippy::assertions_on_constants,
    clippy::bind_instead_of_map,
    clippy::bool_assert_comparison,
    clippy::clone_on_copy,
    clippy::collapsible_if,
    clippy::collapsible_str_replace,
    clippy::derivable_impls,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::double_ended_iterator_last,
    clippy::empty_line_after_doc_comments,
    clippy::err_expect,
    clippy::explicit_auto_deref,
    clippy::explicit_counter_loop,
    clippy::field_reassign_with_default,
    clippy::filter_next,
    clippy::if_same_then_else,
    clippy::items_after_test_module,
    clippy::iter_cloned_collect,
    clippy::let_and_return,
    clippy::let_unit_value,
    clippy::let_underscore_future,
    clippy::manual_clamp,
    clippy::manual_contains,
    clippy::manual_inspect,
    clippy::manual_is_multiple_of,
    clippy::manual_map,
    clippy::manual_pattern_char_comparison,
    clippy::manual_range_contains,
    clippy::manual_range_patterns,
    clippy::manual_repeat_n,
    clippy::manual_str_repeat,
    clippy::manual_strip,
    clippy::manual_unwrap_or,
    clippy::manual_unwrap_or_default,
    clippy::map_identity,
    clippy::needless_as_bytes,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_lifetimes,
    clippy::needless_option_as_deref,
    clippy::needless_range_loop,
    clippy::needless_return,
    clippy::nonminimal_bool,
    clippy::obfuscated_if_else,
    clippy::option_as_ref_deref,
    clippy::overly_complex_bool_expr,
    clippy::print_literal,
    clippy::ptr_arg,
    clippy::question_mark,
    clippy::redundant_closure,
    clippy::redundant_guards,
    clippy::redundant_locals,
    clippy::redundant_pattern_matching,
    clippy::single_char_add_str,
    clippy::single_match,
    clippy::suspicious_open_options,
    clippy::trim_split_whitespace,
    clippy::unnecessary_cast,
    clippy::unnecessary_filter_map,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_map_or,
    clippy::unnecessary_sort_by,
    clippy::unnecessary_to_owned,
    clippy::unnecessary_unwrap,
    clippy::unusual_byte_groupings,
    clippy::useless_conversion,
    clippy::useless_format
)]

pub mod config;
pub mod config_live_reload;
pub mod credential;
pub mod db;
pub mod domain;
pub mod error;
pub mod eventbus;
pub mod intake_consent_authority;
pub mod logging;
pub mod phase_gate;
pub mod pipeline;
pub mod queue_contract;
pub mod receipt;
pub mod runtime_layout;
pub mod utils;

// The scoped `dead_code` allows below moved here from the parent's `mod` lines unchanged.
pub mod services {
    pub mod agent_identity;
    pub mod agent_protocol;
    pub mod agent_recovery;
    pub mod agy_acp;
    pub mod analytics;
    pub mod channel_roles;
    pub mod claude;
    pub mod claude_command;
    pub mod claude_compact_context;
    pub mod claude_compact_trigger;
    pub mod claude_tui;
    pub mod cluster;
    pub mod codex;
    pub mod codex_tui;
    pub mod cswap;
    pub mod dcserver_singleton;
    pub mod disk_monitor;
    pub mod dispatch_gate;
    pub mod git;
    pub mod herdr_admission;
    pub mod herdr_launch;
    pub mod legacy_body_census;
    pub mod mcp_config;
    pub mod memory;
    #[allow(dead_code)]
    pub mod message_outbox;
    pub mod monitoring_store;
    pub mod observability;
    pub mod opencode;
    pub mod org_catalog;
    pub mod pane_readiness;
    pub mod pipeline_override;
    pub mod pipeline_routes;
    pub mod platform;
    pub mod process;
    pub mod process_boot;
    #[allow(dead_code)]
    pub mod provider;
    pub mod provider_auth;
    pub mod provider_auth_profile;
    pub mod provider_cli;
    #[allow(dead_code)]
    pub mod provider_hosting;
    pub mod provider_output_guard;
    pub mod provider_teardown;
    pub mod release_source;
    pub mod remote_stub;
    pub mod resident_loops;
    #[allow(dead_code)]
    pub mod session_backend;
    pub mod session_forwarding;
    // Consumer-free until call sites move onto the host boundary.
    #[allow(dead_code, unused_imports)]
    pub mod session_host;
    pub mod session_identity_metadata;
    pub mod settings;
    pub mod stream_json_cli;
    pub mod task_completion_v1;
    pub mod terminal_status_formatting;
    #[allow(dead_code)]
    pub mod termination_audit;
    pub mod tmux_common;
    pub mod tmux_diagnostics;
    #[allow(dead_code)]
    pub mod tui_input;
    #[allow(dead_code)]
    pub mod tui_o {
        pub mod alarm;
        pub mod channel_policy;
        pub mod cutover;
        pub mod exact_episode;
        pub mod exact_pg;
        pub mod exact_submission;
        #[cfg(all(test, unix))]
        mod gates_m1;
        pub mod ownership;
        pub mod repost;
        pub mod shadow;
        pub mod store;
        pub mod topology;
        pub mod turn_mode;
        pub mod writer;
    }
    pub mod tui_prompt_control;
    #[allow(dead_code)]
    pub mod tui_prompt_dedupe;
    pub mod tui_turn_state;
    pub mod turn_host;
    pub mod turn_summary_line;

    pub mod discord {
        pub mod codex_absence;
        pub mod org_schema;
        pub mod replay_policy;
        pub mod response_sanitizer;
        pub mod runtime_store;
        pub mod session_notice;
        pub mod session_transition;
        pub mod tui_task_card;

        pub mod outbound {
            pub mod notification_policy;
        }

        pub mod turn_bridge {
            pub mod stale_resume;
        }
    }
}

#[cfg(test)]
use crate::config::test_env::panic_probe as test_env_panic_probe;
#[cfg(any(test, feature = "test-support"))]
use adk_test_support::test_support;
