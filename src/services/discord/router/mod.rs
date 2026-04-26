mod control_intent;
mod intake_gate;
mod message_handler;
mod thread_binding;

pub(super) use intake_gate::{handle_event, should_process_turn_message};
pub(crate) use message_handler::{HeadlessTurnStartError, HeadlessTurnStartOutcome};
pub(super) use message_handler::{TurnKind, handle_text_message, start_headless_turn};

#[cfg(test)]
pub(crate) use message_handler::test_harness_exports as message_handler_test_harness_exports;

// Re-export items used across submodules
use thread_binding::{link_dispatch_thread, lookup_dispatch_info, verify_thread_accessible};

#[cfg(test)]
mod tests;
