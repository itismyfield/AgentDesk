//! Re-posting a piece whose first post went unconfirmed and is later found absent, within a fixed
//! per-piece budget. Dormant: nothing here runs unless `tui_o.repost.enabled` is set and wired.

pub(crate) mod config;
pub(crate) mod send;

// Discord side of `send`, declared here so it stays dormant until the writer wires it in.
#[path = "../../discord/outbound/o_writer_repost_io.rs"]
pub(crate) mod io;
