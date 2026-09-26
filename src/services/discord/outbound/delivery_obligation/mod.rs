//! Dormant schema stored in the existing delivery record.
#![allow(dead_code)]
pub(in crate::services::discord) mod schema;
pub(in crate::services::discord) mod state;
pub(in crate::services::discord) const LEDGER_PROTOCOL: u32 = 0;
pub(in crate::services::discord) const LEDGER_SCHEMA: u32 = 1;
pub(in crate::services::discord) const REDRIVE_CAPPED_SIGNAL: &str =
    "relay_obligation_redrive_capped";
