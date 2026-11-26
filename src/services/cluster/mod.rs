//! Cluster-level coordination primitives for the AgentDesk multi-node
//! Discord control plane. Phase 2 of intake-node-routing
//! (docs/design/intake-node-routing.md).
//!
//! Today this module hosts only `intake_routing` — pure decision logic
//! that picks a `target_instance_id` (or "stay local") for a given
//! Discord intake message based on the agent's
//! `preferred_intake_node_labels` and the live `worker_nodes` snapshot.
//! Phase 3 will add the worker-side polling loop in a sibling submodule.

pub(crate) mod agent_execution_node;
pub(crate) mod attachment_transfer;
pub(crate) mod capability_routing;
pub(crate) mod dispatch_hooks;
pub(crate) mod execution_capacity;
pub(crate) mod execution_requirements;
#[cfg(test)]
pub(crate) mod input_fallback;
pub(crate) mod input_fallback_config;
#[cfg(test)]
pub(crate) mod intake_preflight;
pub(crate) mod intake_router_hook;
pub(crate) mod intake_routing;
pub(crate) mod intake_routing_config;
pub(crate) mod intake_routing_telemetry;
pub(crate) mod intake_turn;
pub(crate) mod intake_worker;
pub(crate) mod intake_worker_capabilities;
pub(crate) mod machine_resources;
/// Runner-node registry and leader coordination.
pub(crate) mod node_registry;
pub(crate) mod pre_accept_sweep;
pub(crate) mod readiness;
pub mod registry_adapter_sink;
pub mod relay_producer_registry;
pub mod session_discovery;
pub mod session_matcher;
pub mod session_registry;
/// Session owner routing helpers.
pub(crate) mod session_routing;
pub mod stream_relay;
pub mod watcher_supervisor;

#[cfg(test)]
mod snapshot_tests;
