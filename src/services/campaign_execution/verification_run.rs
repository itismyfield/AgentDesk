pub use crate::domain::campaign::verification_run::*;

#[cfg(any(test, feature = "test-support"))]
#[path = "verification_run_test_support.rs"]
pub mod test_support;

#[cfg(test)]
#[path = "verification_run_tests.rs"]
pub(crate) mod tests;
