//! Kanban facade.
//!
//! Keep `crate::kanban::*` stable while the implementation is split into
//! smaller owner modules.

mod audit;
mod github_sync;
mod github_sync_target;
mod hooks;
mod review_tuning;
mod state_machine;
mod terminal_cleanup;
mod transition_cleanup;
mod transition_core;

pub(crate) use hooks::*;
pub(crate) use review_tuning::*;
pub(crate) use state_machine::*;
pub(crate) use transition_cleanup::*;
pub(crate) use transition_core::*;
