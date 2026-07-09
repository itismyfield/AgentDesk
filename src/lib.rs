#![recursion_limit = "256"]
// Non-dead-code clippy debt predates the bin/lib split. Keep the existing
// style/complexity debt explicit here so this change can remove the crate-wide
// dead_code blanket without rewriting unrelated modules.
//
// `too_many_arguments` is governed solely by this crate-wide allow; per-function
// `#[allow(clippy::too_many_arguments)]` attributes are redundant and removed.
//
// `await_holding_lock` is intentionally NOT suppressed here (#3034): production
// lock-across-await races must surface in clippy. The only legitimate holders
// are test-serialization guards (process-global env/metrics/PG-setup Mutexes),
// each carrying a narrow `#[allow(clippy::await_holding_lock)]` + `// SAFETY:`.
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
    clippy::large_enum_variant,
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
    clippy::result_large_err,
    clippy::single_char_add_str,
    clippy::single_match,
    clippy::suspicious_open_options,
    clippy::too_many_arguments,
    clippy::trim_split_whitespace,
    clippy::type_complexity,
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

mod bootstrap;
mod cli;
// Legacy path shims remain available while runtime-layout migrations settle.
pub(crate) mod compat;
// Config helpers are shared by CLI/server/tests, with some provider-onboarding
// helpers only called from rollout flows.
mod config;
pub(crate) mod config_live_reload;
pub(crate) mod credential;
// Database repositories intentionally expose cross-route helpers that are only
// wired from selected API/maintenance paths.
mod db;
mod dispatch;
// Policy-engine loader/runtime helpers cover hot-reload and test-only entry
// points outside the default server launch path.
mod engine;
// Error-code helpers are kept for API response boundaries that only some
// routes currently surface.
mod error;
// In-process broadcast event bus shared by the WS server layer and background
// services; lives at crate root to avoid a service→server backflow (#3037).
mod eventbus;
mod github;
// Shared HTTP route handler state; lives at crate root (below server+services)
// so service-layer handlers reference it without a service→server backflow (#3037).
pub(crate) mod api_caller_observability;
mod app_state;
pub(crate) mod kanban;
mod launch;
mod logging;
pub(crate) mod manual_intervention;
// Pipeline policy structs include retry/override fields retained for policy
// compatibility across staged rollout paths.
pub(crate) mod pipeline;
pub(crate) mod receipt;
// Reconciliation sweep jobs are invoked by maintenance scheduling, not by every
// compile target.
pub(crate) mod reconcile;
// Runtime layout exposes migration helpers used by setup and repair commands.
pub(crate) mod runtime_layout;
mod server;
// Service modules contain provider, Discord, maintenance, and observability
// feature surfaces that are enabled by runtime config rather than all targets.
// #3034: the crate-wide dead_code blanket has been lowered into `services::mod`
// as per-submodule scoped allows, so the lint is now live on the ~37 clean
// service submodules. The remaining dirty submodules each carry a scoped allow
// (with a residual count) to be retired subtree-by-subtree.
mod services;
// Supervisor test hooks are intentionally retained for dispatch/runtime tests.
pub(crate) mod supervisor;
mod ui;
// Utility detectors are shared opportunistically across provider paths.
mod utils;
// Voice runtime is an optional provider feature; most entry points are wired
// only when voice config is enabled.
pub(crate) mod voice;

#[cfg(test)]
mod high_risk_recovery;

// Re-export for crate-level access (used by services::discord::mod.rs)
pub(crate) use cli::agentdesk_runtime_root;

use anyhow::{Context, Result};

pub fn run_from_args() -> Result<()> {
    match cli::args::parse() {
        cli::args::ParseOutcome::Command { command, json } => cli::execute(command, json),
        cli::args::ParseOutcome::RunServer => {
            let state = bootstrap::initialize().context("Bootstrap failed")?;
            launch::run(state).context("Launch failed")
        }
    }
}
