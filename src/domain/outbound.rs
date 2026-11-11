//! Outbound delivery value contracts and transport port, free of Serenity types.
//! Dedup reservation, nonce use and Serenity id conversion stay in the Discord adapter.

pub(crate) mod ids;
pub(crate) mod key;
pub(crate) mod policy;
pub(crate) mod port;
pub(crate) mod result;
