//! Dormant input persistence and bounded effects; the channel actor supplies all execution policy.

pub mod blob;
pub mod bounded_tmux;
mod durable;
pub mod ledger;

#[cfg(test)]
mod bounded_tmux_tests;
#[cfg(test)]
mod durability_tests;
