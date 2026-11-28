#![recursion_limit = "256"]
#![deny(clippy::drop_non_drop)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::mistyped_literal_suffixes)]
#![deny(clippy::correctness)]
// Non-dead-code clippy debt predates the bin/lib split. Keep the existing
// style/complexity debt explicit here so this change can remove the crate-wide
// dead_code blanket without rewriting unrelated modules.
//
// Structural complexity lints are denied crate-wide through Cargo.toml. Legacy
// occurrences carry narrow item/module allows so newly introduced debt remains
// visible to Clippy.
//
// Test environment wrappers intentionally hold their lock across await to serialize per-test runtimes.
// Clippy cannot see the MutexGuard stored inside those wrappers.
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

mod bootstrap;
mod cli;
// Legacy path shims remain available while runtime-layout migrations settle.
pub(crate) mod compat;
// Config helpers are shared by CLI/server/tests, with some provider-onboarding
// helpers only called from rollout flows.
use adk_runtime::config;
pub(crate) use adk_runtime::config_live_reload;
pub(crate) use adk_runtime::credential;
// Database repositories intentionally expose cross-route helpers that are only
// wired from selected API/maintenance paths.
use adk_runtime::db;
mod dispatch;
use adk_runtime::domain;
#[cfg(unix)]
use adk_runtime::intake_consent_authority;
// Policy-engine loader/runtime helpers cover hot-reload and test-only entry
// points outside the default server launch path.
mod engine;
// Error-code helpers are kept for API response boundaries that only some
// routes currently surface.
use adk_runtime::error;
// In-process broadcast event bus shared by the WS server layer and background
// services; lives at crate root to avoid a service→server backflow (#3037).
use adk_runtime::eventbus;
mod github;
// Shared HTTP route handler state; lives at crate root (below server+services)
// so service-layer handlers reference it without a service→server backflow (#3037).
pub(crate) mod api_caller_observability;
mod app_state;
pub(crate) mod kanban;
mod launch;
use adk_runtime::logging;
pub(crate) mod manual_intervention;
// Pipeline policy structs include retry/override fields retained for policy
// compatibility across staged rollout paths.
pub(crate) use adk_runtime::phase_gate;
pub(crate) use adk_runtime::pipeline;
pub(crate) use adk_runtime::queue_contract;
pub(crate) use adk_runtime::receipt;
// Reconciliation sweep jobs are invoked by maintenance scheduling, not by every
// compile target.
pub(crate) mod reconcile;
// Runtime layout exposes migration helpers used by setup and repair commands.
pub(crate) use adk_runtime::runtime_layout;
mod server;
// Service modules contain provider, Discord, maintenance, and observability
// feature surfaces that are enabled by runtime config rather than all targets.
// #3034: the crate-wide dead_code blanket has been lowered into `services::mod`
// as per-submodule scoped allows, so the lint is now live on the ~37 clean
// service submodules. The remaining dirty submodules each carry a scoped allow
// (with a residual count) to be retired subtree-by-subtree.
mod services;

#[cfg(feature = "test-support")]
pub use services::routines::agent_executor::test_support as routine_agent_executor_test_support;

/// Opaque capability for a resolved Claude executable.
///
/// ```compile_fail
/// use agentdesk::ClaudeBinary;
/// use std::process::Command;
///
/// fn bypass_the_claude_builder(binary: ClaudeBinary) {
///     let _raw_command = Command::new(binary);
/// }
/// ```
///
/// The capability remains sealed through aliases, references, helpers, closures,
/// and renamed imports as well:
///
/// ```compile_fail
/// use agentdesk::ClaudeBinary;
/// use std::process::Command as ProcessCommand;
///
/// type BinaryAlias = ClaudeBinary;
///
/// fn bypass_alias(binary: ClaudeBinary) {
///     let alias: BinaryAlias = binary;
///     let _raw_command = ProcessCommand::new(alias);
/// }
/// ```
///
/// ```compile_fail
/// use agentdesk::ClaudeBinary;
/// use std::process::Command as ProcessCommand;
///
/// fn bypass_reference(binary: ClaudeBinary) {
///     let reference = &binary;
///     let _raw_command = ProcessCommand::new(reference);
/// }
/// ```
///
/// ```compile_fail
/// use agentdesk::ClaudeBinary;
/// use std::process::Command as ProcessCommand;
///
/// fn helper(binary: ClaudeBinary) {
///     let _raw_command = ProcessCommand::new(binary);
/// }
///
/// fn bypass_helper(binary: ClaudeBinary) {
///     helper(binary);
/// }
/// ```
///
/// ```compile_fail
/// use agentdesk::ClaudeBinary;
/// use std::process::Command as ProcessCommand;
///
/// fn bypass_closure(binary: ClaudeBinary) {
///     let spawn = |candidate: ClaudeBinary| ProcessCommand::new(candidate);
///     let _raw_command = spawn(binary);
/// }
/// ```
///
/// A renamed `Command` import does not provide an escape hatch:
///
/// ```compile_fail
/// use agentdesk::ClaudeBinary;
/// use std::process::Command as ProcessCommand;
///
/// fn bypass_renamed_command(binary: ClaudeBinary) {
///     let _raw_command = ProcessCommand::new(binary);
/// }
/// ```

/// `ClaudeBinary` intentionally does not implement `AsRef<OsStr>`, `Deref`,
/// `Display`, or a public path getter. It can therefore be consumed only through
/// guarded Claude launch APIs rather than passed directly to `Command::new`.
///
/// This capability is created at trusted Claude launch boundaries and consumed
/// by the guarded launch builder; raw process construction is intentionally not
/// part of its public API.
///
/// ```compile_fail
/// use agentdesk::ClaudeBinary;
/// use std::process::Command as ProcessCommand;
///
/// fn bypass_public_capability(binary: ClaudeBinary) {
///     let _raw_command = ProcessCommand::new(binary);
/// }
/// ```
///
/// Resolver-layer sealing (#4627) closes the last gap. `ClaudeBinary::resolve`
/// obtains the raw executable path through a `pub(crate)` seam that lives behind
/// the crate-private `services` module, so no external crate can reach it — the
/// module path itself does not resolve from outside the crate. The public generic
/// `resolve_provider_binary("claude")` scrubs the raw path to `None` and redacts
/// the raw-path components in its `attempts` diagnostics, so there is no external
/// route to a raw Claude executable path:
///
/// ```compile_fail
/// // The sealed raw-path Claude resolver seam lives behind the crate-private
/// // `services` module, so an external crate cannot even name the module that
/// // contains it: this `use` fails to resolve, sealing the seam by construction.
/// use agentdesk::services::platform::binary_resolver::BinaryResolution;
///
/// fn wants_raw_claude_path(_: BinaryResolution) {}
/// ```
pub use services::claude_command::ClaudeBinary;

// Supervisor test hooks are intentionally retained for dispatch/runtime tests.
pub(crate) mod supervisor;
mod ui;
// Utility detectors are shared opportunistically across provider paths.
use adk_runtime::utils;
// `#[macro_export]` puts the macro at the runtime crate root; callers keep `crate::auto_queue_log!`.
pub(crate) use adk_runtime::auto_queue_log;

#[cfg(test)]
mod high_risk_recovery;

use anyhow::{Context, Result};

#[cfg(test)]
thread_local! {
    static TEST_PARSE_OUTCOME: std::cell::RefCell<Option<cli::args::ParseOutcome>> = const {
        std::cell::RefCell::new(None)
    };
}

pub fn run_from_args() -> Result<()> {
    config::test_instance::initialize().map_err(anyhow::Error::msg)?;
    services::process_boot::initialize();
    services::discord::install_codex_absence_reader();
    services::discord::install_engine_probes();
    services::discord::outbound::install_engine_ports();
    services::discord::settings::install_dispatch_ports();
    // A failure-marker handoff restarts this executable as a short-lived writer.
    // Dispatch it before Clap so no public/hidden CLI surface is needed and the
    // writer can outlive the one-shot hook relay process that launched it.
    if let Some(worker_result) =
        services::claude_tui::hook_relay::run_failure_marker_worker_from_env()
    {
        return worker_result.map_err(anyhow::Error::msg);
    }
    #[cfg(not(test))]
    let outcome = cli::args::parse();
    #[cfg(test)]
    let outcome = TEST_PARSE_OUTCOME
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_else(cli::args::parse);
    match outcome {
        cli::args::ParseOutcome::Command { command, json } => cli::execute(command, json),
        cli::args::ParseOutcome::RunServer => {
            config::test_instance::environment::refuse_service_management()
                .map_err(anyhow::Error::msg)?;
            let state = bootstrap::initialize().context("Bootstrap failed")?;
            launch::run(state).context("Launch failed")
        }
    }
}

#[cfg(test)]
include!("config/runtime_root_guard/test_process.rs");

#[cfg(test)]
mod engine_probe_tests;
#[cfg(test)]
mod ke_assembly_tests;
#[cfg(test)]
mod ke_routines_tests;
#[cfg(test)]
mod krx_claude_tests;
#[cfg(test)]
mod krx_config_tests;
#[cfg(all(test, unix))]
mod krx_config_unix_tests;
#[cfg(test)]
mod krx_db_b_tests;
#[cfg(test)]
mod krx_db_tests;
#[cfg(test)]
mod krx_services_tests;
#[cfg(test)]
mod krx_session_host_tests;
#[cfg(test)]
mod krx_tui_o_tests;
#[cfg(all(test, unix))]
mod krx_tui_o_unix_tests;
#[cfg(test)]
use crate::config::test_env::panic_probe as test_env_panic_probe;
#[cfg(any(test, feature = "test-support"))]
use adk_test_support::test_support;

/// Managed writer lifetime contracts; hosting and ownership admission remain internal.
pub mod home_runtime {
    pub use crate::services::tui_o::writer::host::{ManagedWriterHandle, WriterStopped};
}

/// Dormant server-campaign ADK adapter contracts; initiation stays behind the final host gate.
pub mod campaign_adapter {
    pub use crate::db::server_campaigns::evidence_store;
    pub use crate::domain::campaign::lifecycle::{HoldReason, NextAction, StepKey};
    pub use crate::domain::campaign::{pipeline, pipeline_fields};
    #[cfg(any(test, feature = "test-support"))]
    pub use crate::services::campaign_execution::stage_fsm;
    pub use crate::services::campaign_execution::{
        adk_adapter, adk_observe, adk_profile, adk_resources, adk_workspace, candidate_seal,
        ci_exception, deploy_observe, effect_envelope, evidence, evidence_gate, execution_pin,
        host_gate, host_worker, landing_effects, landing_merge, landing_prepare, main_integration,
        pipeline_rules, profile, resource_admission, review_dispatch, review_gate, runtime_profile,
        verification_driver, verification_run, verification_runner, verification_spec,
    };
}

/// Dormant Decision input contracts: an approved event's single delivery, its boot recovery and
/// its byte-keeping plans. The service that approves events calls them; no request reaches them.
pub mod decision_input {
    pub use crate::services::claude_tui::input::plan_preserved_submit as claude_preserved_plan;
    pub use crate::services::codex_tui::input::owned_preserved_plan as codex_preserved_plan;
    pub use crate::services::discord::decision_drain::recover_boot;
    pub use crate::services::discord::health::decision_target::{
        FirstAttempt, Normalized, Normalizer, deliver_once,
    };
    pub use crate::services::tui_input::human_frame::{PreservedFrame, preserved_decision_body};
}

/// Dormant iMessage reply contracts for admission and claim/ACK integration.
pub mod imsg_reply {
    pub use crate::db::imsg_reply::{
        AckOutcome, Action, ActionAck, ClaimAction, Obligation, Purpose, ack_action, bind_consumed,
        claim_action, read_action, read_obligation,
    };
}
