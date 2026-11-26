//! Re-posting a piece whose first post went unconfirmed and is later found absent, within a fixed
//! per-piece budget.

pub mod admission;
pub mod dispatch;
pub mod identity;
pub mod projection;
pub mod provenance;
pub mod runner;
pub mod send;

// Discord side of `send`, sharing the writer's transport.
#[path = "../../discord/outbound/o_writer_repost_io.rs"]
pub mod io;

// The shared PostgreSQL budget.
#[path = "../../../db/o_piece_attempts.rs"]
pub mod o_piece_attempts;
#[path = "../../../db/o_piece_delivery.rs"]
pub mod o_piece_delivery;
#[cfg(test)]
#[path = "../../../db/o_piece_ledger_tests.rs"]
mod o_piece_ledger_tests;
