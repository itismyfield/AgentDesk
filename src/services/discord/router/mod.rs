mod authorization;
mod dispatch_trigger;
mod intake_dispatch;
mod intake_gate;
mod intake_queue_transaction;
mod message_handler;
mod response_format;
mod thread_binding;
mod turn_start;

pub(crate) use authorization::TurnKind;
pub(crate) use intake_dispatch::{
    IntakeOrigin, IntakeSubmission, QueuedAdmissionDisposition, admit_queued_intake,
    dispatch_skill_intake, dispatch_text_intake, finish_admitted_queued_intake,
};
pub(super) use intake_gate::{handle_event, should_process_turn_message};
#[cfg(test)]
pub(super) use message_handler::set_hosted_tui_promote_busy_for_tests;
pub(super) use message_handler::{
    IntakeDeps, defer_promoted_dispatch_if_hosted_tui_busy, hosted_tui_promote_readiness_blocked,
    mailbox_try_start_turn_with_terminal_marker_cleanup, start_headless_turn,
    start_reserved_headless_turn,
};
pub(crate) use message_handler::{IntakeRequest, execute_intake_turn_core};
pub(super) use turn_start::reserve_headless_turn;
pub(crate) use turn_start::{
    HeadlessTurnReservation, HeadlessTurnStartError, HeadlessTurnStartOutcome,
    HeadlessTurnStartStatus,
};

// Re-export items used across submodules
use thread_binding::{link_dispatch_thread, lookup_dispatch_info, verify_thread_accessible};
