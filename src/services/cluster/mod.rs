//! Cluster-level coordination primitives for the AgentDesk multi-node
//! Discord control plane. Phase 2 of intake-node-routing
//! (docs/design/intake-node-routing.md).
//!
//! Today this module hosts only `intake_routing` — pure decision logic
//! that picks a `target_instance_id` (or "stay local") for a given
//! Discord intake message based on the agent's
//! `preferred_intake_node_labels` and the live `worker_nodes` snapshot.
//! Phase 3 will add the worker-side polling loop in a sibling submodule.

pub(crate) mod intake_router_hook;
pub(crate) mod intake_routing;
pub(crate) mod intake_worker;
pub mod session_matcher;
