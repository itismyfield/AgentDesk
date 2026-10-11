//! Pure contracts for a future boot collector; no configuration keys or effects are installed.
//! Declared under the dormant supervisor, which reserves boot plans from them.

#[path = "activation/plan.rs"]
pub(crate) mod plan;
#[path = "activation/scope.rs"]
pub(crate) mod scope;

#[cfg(test)]
#[path = "activation/scope_tests.rs"]
mod scope_tests;
#[cfg(test)]
#[path = "activation/selection_tests.rs"]
mod selection_tests;
