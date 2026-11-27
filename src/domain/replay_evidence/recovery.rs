//! Recovery witnesses retain their root custody separately from their observed identity.
#[path = "recovery/root.rs"]
pub mod root;
#[path = "recovery/snapshot.rs"]
pub(crate) mod snapshot;
pub use snapshot::LocalSnapshot;
