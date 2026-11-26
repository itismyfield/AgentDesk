#[cfg(unix)]
pub(crate) mod attempt;
#[cfg(unix)]
pub mod capture;
#[cfg(unix)]
pub mod confirm;
pub(crate) mod identity;
#[cfg(unix)]
pub(crate) mod producer;

#[cfg(all(test, unix))]
mod attempt_tests;
