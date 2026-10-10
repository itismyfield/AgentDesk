//! Dormant input persistence, row semantics and bounded effects; the channel actor supplies execution.

pub mod actor;
pub mod attempt;
pub mod blob;
pub mod bounded_tmux;
mod durable;
pub mod handover;
pub mod input_key;
pub mod ledger;
pub mod rows;
pub(crate) mod submission;
pub(crate) mod submission_tmux;
pub mod transition;

#[cfg(test)]
mod actor_tests;
#[cfg(test)]
mod bounded_tmux_tests;
#[cfg(test)]
pub(crate) mod durability_tests;
#[cfg(test)]
mod input_key_tests;
#[cfg(test)]
mod rows_tests;
#[cfg(test)]
mod transition_fixture_tests;
#[cfg(test)]
mod transition_tests;
