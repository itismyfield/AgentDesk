//! Compatibility facade for cluster node-registry / capability-routing infra.
//!
//! The implementation lives in
//! `crate::services::cluster::node_registry` (#3037 bucket 3): it is pure
//! cluster coordination (config + db + serde) with no route/axum dependency,
//! so it belongs beside the rest of the cluster services. Route-layer callers
//! keep this `crate::server::cluster::*` path through re-exports only.
#[allow(unused_imports)]
pub(crate) use crate::services::cluster::node_registry::*;
