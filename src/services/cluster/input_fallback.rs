//! Existing fixtures share the production routing primitives.
pub(crate) use crate::services::cluster::intake_router_hook::owner_record::fallback::{
    Availability, Held, Home, Snapshot, Target, choose_target,
};
pub(crate) mod notice;
#[cfg(test)]
mod tests;
