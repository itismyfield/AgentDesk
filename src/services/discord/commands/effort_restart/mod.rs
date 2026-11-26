//! `/effort` on a local tmux TUI or a Herdr Codex pane: the TUI ends at once, cutting only the
//! turn it captured, and that turn is owed one continuation on the same native session.
mod herdr;
mod notice;
mod recovery;
mod tmux;
mod witness;

pub(in crate::services::discord) use herdr::{
    cancel_idle_resume, herdr_resume, notice as herdr_notice, record_idle_resume,
};
pub(in crate::services::discord) use recovery::{kick, resume_owed};
pub(in crate::services::discord) use tmux::{Restart, restart};
