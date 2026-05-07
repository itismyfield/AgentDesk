//! Kanban facade.
//!
//! Keep `crate::kanban::*` stable while the implementation is split into
//! smaller owner modules.

mod state_machine;
mod terminal_cleanup;
mod transition_cleanup;
mod transition_core;

pub(crate) use state_machine::*;
pub(crate) use transition_cleanup::*;
pub(crate) use transition_core::*;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) mod test_support;
