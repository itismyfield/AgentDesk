//! `/effort` on a local tmux TUI: the TUI ends at once, cutting only the turn it captured, and
//! that turn is owed one continuation on the same native session.
mod notice;
mod recovery;
mod tmux;
mod witness;

pub(in crate::services::discord) use recovery::{kick, resume_owed};
pub(in crate::services::discord) use tmux::{Restart, restart};
