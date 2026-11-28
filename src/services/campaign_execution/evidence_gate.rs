pub use crate::domain::campaign::evidence_gate::*;

#[cfg(any(test, feature = "test-support"))]
#[path = "evidence_gate_test_support.rs"]
pub mod test_support;

#[cfg(test)]
#[path = "evidence_gate_tests.rs"]
pub(crate) mod tests;
