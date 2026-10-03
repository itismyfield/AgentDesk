//! Dormant input persistence, row semantics and bounded effects; the channel actor supplies execution.

pub mod blob;
pub mod bounded_tmux;
mod durable;
pub mod handover;
pub mod ledger;
pub mod rows;

#[cfg(test)]
mod bounded_tmux_tests;
#[cfg(test)]
mod durability_tests;
#[cfg(test)]
mod rows_tests;
