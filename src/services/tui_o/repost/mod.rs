//! Re-posting a piece whose first post went unconfirmed and is later found absent, within a fixed
//! per-piece budget.

pub(crate) mod admission;
pub(crate) mod dispatch;
pub(crate) mod identity;
pub(crate) mod projection;
pub(crate) mod provenance;
pub(crate) mod runner;
#[cfg(test)]
mod runner_tests;
pub(crate) mod send;

// Discord side of `send`, sharing the writer's transport.
#[path = "../../discord/outbound/o_writer_repost_io.rs"]
pub(crate) mod io;

// The shared PostgreSQL budget.
#[cfg(test)]
mod admission_tests;
#[path = "../../../db/o_piece_attempts.rs"]
pub(crate) mod o_piece_attempts;
#[path = "../../../db/o_piece_delivery.rs"]
pub(crate) mod o_piece_delivery;
#[cfg(test)]
#[path = "../../../db/o_piece_ledger_tests.rs"]
mod o_piece_ledger_tests;
