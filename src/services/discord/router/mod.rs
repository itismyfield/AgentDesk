mod authorization;
mod dispatch_trigger;
#[cfg(unix)]
pub(in crate::services::discord) mod herdr_consent;
mod intake_dispatch;
mod intake_gate;
mod intake_queue_transaction;
pub(in crate::services::discord) mod message_handler;
pub(in crate::services::discord) use message_handler::input_payload;
mod queue_status_presentation;
mod response_format;
#[cfg(test)]
pub(in crate::services::discord) mod source_guard_support;
mod thread_binding;
mod turn_start;

pub(crate) use authorization::TurnKind;
pub(crate) use intake_dispatch::replay_admission::IntakeExecution;
pub(in crate::services::discord) fn replay_producers_dormant(channel: &str) -> bool {
    intake_dispatch::replay_admission::dormant_start(channel).is_some()
}

#[cfg(test)]
pub(in crate::services::discord) use intake_dispatch::replay_admission::test_activation as replay_test_activation;
pub(crate) use intake_dispatch::{
    IntakeOrigin, IntakeSubmission, QueuedAdmissionDisposition, admit_queued_intake,
    admit_text_intake, checkpoint_waits_for_admission, dispatch_live_text_intake,
    dispatch_skill_intake, dispatch_text_intake, finish_admitted_queued_intake,
    finish_admitted_text_intake, finish_nonlocal_attachment_admission, live_checkpoint_waits,
    prepare_admitted_live_attachments, resolve_attachment_admission, start_owner_front,
};
#[cfg(test)]
pub(in crate::services::discord) use intake_gate::busy_inject::test_support as busy_inject_support;
#[cfg(test)]
pub(in crate::services::discord) use intake_gate::stale_turn::thread_guard_test_api;
pub(super) use intake_gate::{handle_event, should_process_turn_message};
#[cfg(test)]
pub(super) use message_handler::set_hosted_tui_promote_busy_for_tests;
pub(super) use message_handler::{
    IntakeDeps, defer_promoted_dispatch_if_hosted_tui_busy, hosted_tui_promote_readiness_blocked,
    start_headless_turn, start_reserved_headless_turn, start_reserved_headless_turn_with_owner,
};
pub(crate) use message_handler::{IntakeRequest, execute_intake_turn_core, start_exact_retry};
pub(in crate::services::discord) use queue_status_presentation::queue_status_card_enabled;
#[cfg(test)]
pub(super) use turn_start::claim_reserved_headless_turn;
pub(super) use turn_start::reserve_headless_turn;
pub(crate) use turn_start::{
    HeadlessTurnReservation, HeadlessTurnStartError, HeadlessTurnStartOutcome,
    HeadlessTurnStartStatus,
};
#[cfg(test)]
pub(crate) use turn_start::{
    load_session_runtime_state, try_intake_runtime_transition_after_redirect,
};

// Re-export items used across submodules
use thread_binding::{link_dispatch_thread, lookup_dispatch_info, verify_thread_accessible};

pub(in crate::services::discord) use intake_gate::busy_inject::{
    covers as live_intake_inject_covers, queued_head_covers as queued_head_inject_covers,
};
