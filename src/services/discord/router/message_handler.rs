use super::super::gateway::{
    DiscordGateway, HeadlessGateway, LiveDiscordTurnContext, send_intake_placeholder,
};
use super::super::*;
pub(in crate::services::discord) use super::authorization::{
    TurnKind, classify_turn_kind_from_author,
};
use super::dispatch_trigger::{
    dispatch_session_path_should_update, dispatch_should_recover_session_worktree,
    parse_dispatch_context_hints, resolve_dispatch_target_repo_dir,
};
use super::response_format::{
    build_headless_trigger_context, build_memory_injection_plan, build_race_requeued_intervention,
    build_system_discord_context, dispatch_profile_label, memento_recall_gate_decision,
    merge_reply_contexts, should_note_memento_context_loaded, wrap_user_prompt_with_author,
};
pub(in crate::services::discord) use super::turn_start::reserve_headless_turn;
pub(crate) use super::turn_start::{
    HeadlessTurnReservation, HeadlessTurnStartError, HeadlessTurnStartOutcome,
    HeadlessTurnStartStatus,
};
use super::turn_start::{
    cli_just_spawned_for_emit, dispatch_reset_lifecycle_code, emit_session_strategy_lifecycle,
    load_session_runtime_state, log_session_strategy_diagnostic, put_back_session_retry_context,
    put_back_voluntary_feedback_reminder, refresh_session_strategy_after_pending_reset,
    release_mailbox_after_hosted_tui_busy_pre_submit,
    release_mailbox_after_placeholder_post_failure, session_runtime_state_after_redirect,
    take_and_merge_feedback_reminder, take_session_retry_context,
};
#[cfg(test)]
use super::turn_start::{session_strategy_lifecycle_event, should_emit_session_strategy_lifecycle};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::memory::{
    RecallMode, RecallRequest, RecallResponse, RecallSizeBucket, build_memory_backend,
    note_recall_context_size, resolve_memory_role_id, resolve_memory_session_id,
};
#[cfg(test)]
use crate::services::observability::turn_lifecycle::TurnEvent;
use crate::services::provider::CancelToken;
use std::future::Future;
use std::sync::Arc;
use url::Url;
mod attachments;
mod control;
mod goal_lifecycle;
mod headless_turn;
mod intake_turn;
mod latency_spans;
mod provider_isolation;
mod tui_followup;
mod turn_lifecycle;
mod voice_announcement_route;
mod voice_announcement_scope;
mod watchdog;

use self::goal_lifecycle::*;
use self::provider_isolation::*;
use self::tui_followup::*;
use self::turn_lifecycle::*;
use self::watchdog::*;

pub(super) use self::attachments::handle_file_upload;
pub(super) use self::control::{handle_shell_command_raw, handle_text_command};
#[allow(unused_imports)]
pub(in crate::services::discord) use self::headless_turn::{
    start_headless_turn, start_reserved_headless_turn, start_voice_headless_turn,
};
pub(in crate::services::discord) use self::intake_turn::{IntakeDeps, handle_text_message};
pub(crate) use self::intake_turn::{IntakeRequest, execute_intake_turn_core};
// #4270 — pre-teardown hosted-TUI readiness probe + live-dispatch defer for the
// queued-turn promote entrypoints (idle kickoff in discord/mod.rs, live
// dispatch in gateway.rs).
#[cfg(test)]
pub(in crate::services::discord) use self::tui_followup::set_hosted_tui_promote_busy_for_tests;
pub(in crate::services::discord) use self::tui_followup::{
    defer_promoted_dispatch_if_hosted_tui_busy, hosted_tui_promote_readiness_blocked,
};

#[cfg(test)]
mod session_strategy_lifecycle_tests;
