//! Existing notice fixtures share the production identity and renderer.
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
pub(crate) use crate::services::cluster::intake_router_hook::owner_record::fallback::notice::{
    Labels, Notice, Transition, fingerprint, prepare,
};
#[cfg(test)]
mod pg_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod transport_tests;
