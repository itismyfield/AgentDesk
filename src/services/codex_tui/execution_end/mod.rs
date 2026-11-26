#[cfg(unix)]
pub mod attempt;
#[cfg(unix)]
pub mod capture;
#[cfg(unix)]
pub mod confirm;
pub(crate) mod identity;
#[cfg(unix)]
pub mod producer;

#[cfg(all(test, unix))]
mod attempt_tests;
