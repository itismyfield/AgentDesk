mod authorization;
mod control_intent;
mod dispatch_trigger;
mod intake_gate;
mod message_handler;
mod response_format;
mod thread_binding;
mod turn_start;

pub(super) use authorization::TurnKind;
pub(super) use intake_gate::{handle_event, should_process_turn_message};
pub(super) use message_handler::{
    handle_text_message, start_headless_turn, start_reserved_headless_turn,
};
pub(super) use turn_start::reserve_headless_turn;
pub(crate) use turn_start::{
    HeadlessTurnReservation, HeadlessTurnStartError, HeadlessTurnStartOutcome,
};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use message_handler::test_harness_exports as message_handler_test_harness_exports;

// Re-export items used across submodules
use thread_binding::{link_dispatch_thread, lookup_dispatch_info, verify_thread_accessible};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests;
