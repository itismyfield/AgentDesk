//! Pure contracts for a future boot collector; no configuration keys or effects are installed.

pub(crate) mod plan;
pub(crate) mod scope;

#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod selection_tests;
