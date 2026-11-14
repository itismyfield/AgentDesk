//! Recovery witnesses retain their root custody separately from their observed identity.
#[path = "recovery/root.rs"]
pub(crate) mod root;
#[path = "recovery/snapshot.rs"]
pub(crate) mod snapshot;
pub(crate) use snapshot::LocalSnapshot;
