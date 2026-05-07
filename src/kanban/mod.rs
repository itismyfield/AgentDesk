//! Kanban facade.
//!
//! Keep `crate::kanban::*` stable while the implementation is split into
//! smaller owner modules.

mod state_machine;

pub(crate) use state_machine::*;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) mod test_support;
