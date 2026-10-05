// Dormant until the deliver route wires busy-turn injection.
#[allow(dead_code)]
pub(crate) mod busy_inject;
#[cfg(all(test, unix))]
mod busy_inject_tests;
pub(crate) mod composer_lock;
pub mod hook_bundle;
pub(crate) mod hook_output_guard;
#[cfg(test)]
mod hook_output_guard_tests;
#[cfg(test)]
mod hook_payload_fixture_tests;
pub mod hook_registry;
pub mod hook_relay;
pub mod hook_server;
#[cfg(test)]
mod hook_server_memento_tests;
pub(crate) mod host_input;
#[cfg(unix)]
pub(crate) mod hosting;
pub mod input;
pub(crate) mod memento_feedback;
pub(crate) mod memento_writer_hook;
pub(super) mod prompt_readiness;
pub mod session;
pub(crate) mod source_verify;
pub(crate) mod startup_dialog;
pub mod transcript_tail;
pub mod tui_relay;
