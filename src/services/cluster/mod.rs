//! Cluster-level coordination primitives for the AgentDesk multi-node
//! Discord control plane. Phase 2 of intake-node-routing
//! (docs/design/intake-node-routing.md).
//!
//! Today this module hosts only `intake_routing` — pure decision logic
//! that picks a `target_instance_id` (or "stay local") for a given
//! Discord intake message based on the agent's
//! `preferred_intake_node_labels` and the live `worker_nodes` snapshot.
//! Phase 3 will add the worker-side polling loop in a sibling submodule.

pub(crate) mod capability_routing;
pub(crate) mod intake_preflight;
pub(crate) mod intake_router_hook;
pub(crate) mod intake_routing;
pub(crate) mod intake_routing_config;
pub(crate) mod intake_routing_telemetry;
pub(crate) mod intake_worker;
pub(crate) mod intake_worker_capabilities;
/// Worker-node registry + capability routing infrastructure. Relocated from
/// `server::cluster` (#3037 bucket 3): it is pure cluster coordination
/// (config + db + serde) with no route/axum dependency, so it belongs beside
/// the rest of the cluster services. `server::cluster` re-exports it for the
/// route layer.
pub(crate) mod node_registry;
pub mod registry_adapter_sink;
pub mod relay_producer_registry;
pub mod session_discovery;
pub mod session_matcher;
pub mod session_registry;
/// Session owner routing helpers. Relocated from
/// `server::cluster_session_routing` (#3037 bucket 3); `server::cluster_session_routing`
/// re-exports it for the route layer.
pub(crate) mod session_routing;
pub mod stream_relay;
pub mod watcher_supervisor;
