//! Activity of confirmed turn-mode channels, whose direct turns leave no row or mailbox token.

pub(in crate::services::discord) mod activity;
#[cfg(all(test, unix))]
pub(in crate::services::discord) mod admission;
pub(in crate::services::discord) mod entrypoints;
#[cfg(all(test, unix))]
pub(crate) mod lifecycle;
pub(in crate::services::discord) mod supervisor;
