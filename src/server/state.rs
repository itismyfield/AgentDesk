//! Compatibility facade for the per-channel monitoring store.
//!
//! The implementation lives in `crate::services::monitoring_store` (#3037
//! bucket 3): it is pure in-memory state (no axum / no route dependency), so it
//! belongs beside the rest of the service-layer infra. Route-layer callers keep
//! this `crate::server::routes::state::*` path (mounted via `#[path]` in
//! `server/routes/mod.rs`) through re-exports only.
#[allow(unused_imports)]
pub(crate) use crate::services::monitoring_store::{
    MonitoringEntry, MonitoringStore, global_monitoring_store,
};
