//! Compatibility facade for cluster session-owner routing helpers.
//!
//! The implementation lives in
//! `crate::services::cluster::session_routing` (#3037 bucket 3); it is pure
//! routing logic (config + serde) with no route/axum dependency, so it belongs
//! beside the rest of the cluster services. Route-layer callers keep this
//! `crate::server::cluster_session_routing::*` path through re-exports only.
#[allow(unused_imports)]
pub(crate) use crate::services::cluster::session_routing::*;
