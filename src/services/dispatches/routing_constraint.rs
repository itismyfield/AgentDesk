use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::config::{ClusterConfig, ClusterDispatchRoutingConfig, ClusterNodeConfig};
use crate::server::cluster::CapabilityRouteCandidate;

pub(crate) const NOOP_CONSTRAINT_NAME: &str = "noop";
pub(crate) const NODE_CONCURRENCY_CAP_CONSTRAINT_NAME: &str = "node_concurrency_cap";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum ConstraintOutcome {
    Available,
    Wait { reason: String },
    Reject { reason: String },
}

impl ConstraintOutcome {
    pub(crate) fn wait(reason: impl Into<String>) -> Self {
        Self::Wait {
            reason: reason.into(),
        }
    }

    pub(crate) fn reject(reason: impl Into<String>) -> Self {
        Self::Reject {
            reason: reason.into(),
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Wait { reason } | Self::Reject { reason } => Some(reason),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RoutingDispatch {
    pub(crate) dispatch_id: String,
    pub(crate) dispatch_type: Option<String>,
    pub(crate) required_capabilities: Option<Value>,
}

impl RoutingDispatch {
    pub(crate) fn new(
        dispatch_id: impl Into<String>,
        dispatch_type: Option<String>,
        required_capabilities: Option<Value>,
    ) -> Self {
        Self {
            dispatch_id: dispatch_id.into(),
            dispatch_type,
            required_capabilities,
        }
    }
}

pub(crate) trait RoutingConstraint: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, node: &Value, dispatch: &RoutingDispatch) -> ConstraintOutcome;
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ConstraintCheckResult {
    pub(crate) constraint: String,
    pub(crate) outcome: ConstraintOutcome,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct RoutingEngineCandidate {
    pub(crate) decision: crate::server::cluster::CapabilityRouteDecision,
    pub(crate) score: i64,
    pub(crate) last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) constraints: Vec<ConstraintCheckResult>,
    pub(crate) final_outcome: ConstraintOutcome,
}

impl RoutingEngineCandidate {
    pub(crate) fn instance_id(&self) -> Option<&str> {
        self.decision.instance_id.as_deref()
    }

    pub(crate) fn is_available(&self) -> bool {
        self.final_outcome.is_available()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct RoutingEngineDecision {
    pub(crate) selected: Option<RoutingEngineCandidate>,
    pub(crate) candidates: Vec<RoutingEngineCandidate>,
}

impl RoutingEngineDecision {
    pub(crate) fn selected_instance_id(&self) -> Option<&str> {
        self.selected
            .as_ref()
            .and_then(|candidate| candidate.instance_id())
    }

    pub(crate) fn candidate_for_instance(
        &self,
        instance_id: &str,
    ) -> Option<&RoutingEngineCandidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.instance_id() == Some(instance_id))
    }

    pub(crate) fn has_constraint_blocked_candidates(&self) -> bool {
        self.candidates
            .iter()
            .any(|candidate| !candidate.is_available())
    }

    pub(crate) fn constraint_results_json(&self) -> Value {
        json!(
            self.candidates
                .iter()
                .map(|candidate| {
                    json!({
                        "instance_id": candidate.instance_id(),
                        "final_outcome": candidate.final_outcome,
                        "constraints": candidate.constraints,
                    })
                })
                .collect::<Vec<_>>()
        )
    }
}

pub(crate) struct RoutingEngine {
    constraints: Vec<Box<dyn RoutingConstraint>>,
}

impl RoutingEngine {
    pub(crate) fn from_config(config: &ClusterDispatchRoutingConfig) -> Self {
        Self::new(constraints_from_config(config))
    }

    pub(crate) fn from_cluster_config(config: &ClusterConfig) -> Self {
        Self::new(constraints_from_cluster_config(config))
    }

    pub(crate) fn new(constraints: Vec<Box<dyn RoutingConstraint>>) -> Self {
        Self { constraints }
    }

    pub(crate) fn route(
        &self,
        nodes: &[Value],
        required_capabilities: &Value,
        dispatch: &RoutingDispatch,
    ) -> RoutingEngineDecision {
        let capability_candidates =
            crate::server::cluster::select_capability_route(nodes, required_capabilities);
        let mut selected = None;
        let mut candidates = Vec::new();

        for capability_candidate in capability_candidates {
            let Some(node) = node_for_candidate(nodes, &capability_candidate) else {
                continue;
            };
            let constraints = self.evaluate_constraints(node, dispatch);
            let final_outcome = aggregate_outcome(&constraints);
            let candidate = RoutingEngineCandidate {
                decision: capability_candidate.decision,
                score: capability_candidate.score,
                last_heartbeat_at: capability_candidate.last_heartbeat_at,
                constraints,
                final_outcome,
            };
            if selected.is_none() && candidate.is_available() {
                selected = Some(candidate.clone());
            }
            candidates.push(candidate);
        }

        RoutingEngineDecision {
            selected,
            candidates,
        }
    }

    fn evaluate_constraints(
        &self,
        node: &Value,
        dispatch: &RoutingDispatch,
    ) -> Vec<ConstraintCheckResult> {
        self.constraints
            .iter()
            .map(|constraint| ConstraintCheckResult {
                constraint: constraint.name().to_string(),
                outcome: constraint.check(node, dispatch),
            })
            .collect()
    }
}

#[derive(Debug, Default)]
pub(crate) struct NoOpConstraint;

impl RoutingConstraint for NoOpConstraint {
    fn name(&self) -> &'static str {
        NOOP_CONSTRAINT_NAME
    }

    fn check(&self, _node: &Value, _dispatch: &RoutingDispatch) -> ConstraintOutcome {
        ConstraintOutcome::Available
    }
}

#[derive(Debug, Default)]
pub(crate) struct NodeConcurrencyCapConstraint {
    caps_by_node: BTreeMap<String, u32>,
}

impl NodeConcurrencyCapConstraint {
    fn from_node_configs(node_configs: Option<&BTreeMap<String, ClusterNodeConfig>>) -> Self {
        let caps_by_node = node_configs
            .into_iter()
            .flat_map(|configs| configs.iter())
            .filter_map(|(node, config)| {
                config
                    .max_concurrent_dispatches
                    .map(|cap| (node.clone(), cap))
            })
            .collect();
        Self { caps_by_node }
    }

    fn cap_for_node(&self, node: &Value) -> Option<(String, u32)> {
        let instance_id = node.get("instance_id").and_then(Value::as_str);
        if let Some(instance_id) = instance_id
            && let Some(cap) = self.caps_by_node.get(instance_id)
        {
            return Some((instance_id.to_string(), *cap));
        }

        let hostname = node.get("hostname").and_then(Value::as_str);
        if let Some(hostname) = hostname
            && let Some(cap) = self.caps_by_node.get(hostname)
        {
            return Some((hostname.to_string(), *cap));
        }

        None
    }

    fn active_dispatch_count(node: &Value) -> u32 {
        node.get("active_dispatch_count")
            .and_then(Value::as_u64)
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or(0)
    }
}

impl RoutingConstraint for NodeConcurrencyCapConstraint {
    fn name(&self) -> &'static str {
        NODE_CONCURRENCY_CAP_CONSTRAINT_NAME
    }

    fn check(&self, node: &Value, _dispatch: &RoutingDispatch) -> ConstraintOutcome {
        let Some((node_key, cap)) = self.cap_for_node(node) else {
            return ConstraintOutcome::Available;
        };
        let active = Self::active_dispatch_count(node);
        if active >= cap {
            return ConstraintOutcome::wait(format!(
                "node {node_key} active dispatches {active}/{cap} at capacity"
            ));
        }
        ConstraintOutcome::Available
    }
}

fn noop_constraint() -> Box<dyn RoutingConstraint> {
    Box::new(NoOpConstraint)
}

pub(crate) fn constraints_from_config(
    config: &ClusterDispatchRoutingConfig,
) -> Vec<Box<dyn RoutingConstraint>> {
    constraints_from_names(&config.constraints, None)
}

pub(crate) fn constraints_from_cluster_config(
    config: &ClusterConfig,
) -> Vec<Box<dyn RoutingConstraint>> {
    let mut constraints =
        constraints_from_names(&config.dispatch_routing.constraints, Some(&config.nodes));
    if node_concurrency_caps_configured(&config.nodes)
        && !constraints
            .iter()
            .any(|constraint| constraint.name() == NODE_CONCURRENCY_CAP_CONSTRAINT_NAME)
    {
        constraints.push(Box::new(NodeConcurrencyCapConstraint::from_node_configs(
            Some(&config.nodes),
        )));
    }
    constraints
}

fn constraints_from_names(
    names: &[String],
    node_configs: Option<&BTreeMap<String, ClusterNodeConfig>>,
) -> Vec<Box<dyn RoutingConstraint>> {
    names
        .iter()
        .filter_map(|name| match name.as_str() {
            NOOP_CONSTRAINT_NAME => Some(noop_constraint()),
            NODE_CONCURRENCY_CAP_CONSTRAINT_NAME => Some(Box::new(
                NodeConcurrencyCapConstraint::from_node_configs(node_configs),
            )
                as Box<dyn RoutingConstraint>),
            _ => {
                tracing::warn!(
                    constraint = name.as_str(),
                    "[dispatch-routing] unknown routing constraint configured"
                );
                None
            }
        })
        .collect()
}

fn node_concurrency_caps_configured(nodes: &BTreeMap<String, ClusterNodeConfig>) -> bool {
    nodes
        .values()
        .any(|config| config.max_concurrent_dispatches.is_some())
}

fn aggregate_outcome(results: &[ConstraintCheckResult]) -> ConstraintOutcome {
    if let Some(result) = results
        .iter()
        .find(|result| matches!(result.outcome, ConstraintOutcome::Reject { .. }))
    {
        return ConstraintOutcome::reject(format!(
            "{}: {}",
            result.constraint,
            result.outcome.reason().unwrap_or("rejected")
        ));
    }
    if let Some(result) = results
        .iter()
        .find(|result| matches!(result.outcome, ConstraintOutcome::Wait { .. }))
    {
        return ConstraintOutcome::wait(format!(
            "{}: {}",
            result.constraint,
            result.outcome.reason().unwrap_or("waiting")
        ));
    }
    ConstraintOutcome::Available
}

fn node_for_candidate<'a>(
    nodes: &'a [Value],
    candidate: &CapabilityRouteCandidate,
) -> Option<&'a Value> {
    let instance_id = candidate.decision.instance_id.as_deref()?;
    nodes
        .iter()
        .find(|node| node.get("instance_id").and_then(Value::as_str) == Some(instance_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    struct FixedConstraint {
        name: &'static str,
        outcome: ConstraintOutcome,
    }

    impl RoutingConstraint for FixedConstraint {
        fn name(&self) -> &'static str {
            self.name
        }

        fn check(&self, _node: &Value, _dispatch: &RoutingDispatch) -> ConstraintOutcome {
            self.outcome.clone()
        }
    }

    fn node(instance_id: &str, label: &str, heartbeat: &str) -> Value {
        json!({
            "instance_id": instance_id,
            "status": "online",
            "labels": [label],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": heartbeat
        })
    }

    fn dispatch() -> RoutingDispatch {
        RoutingDispatch::new("dispatch-1", Some("implementation".to_string()), None)
    }

    fn node_caps(entries: &[(&str, u32)]) -> BTreeMap<String, ClusterNodeConfig> {
        entries
            .iter()
            .map(|(node, cap)| {
                (
                    (*node).to_string(),
                    ClusterNodeConfig {
                        max_concurrent_dispatches: Some(*cap),
                    },
                )
            })
            .collect()
    }

    fn node_with_active(instance_id: &str, label: &str, active_dispatch_count: u32) -> Value {
        json!({
            "instance_id": instance_id,
            "hostname": label,
            "status": "online",
            "labels": [label],
            "capabilities": {"providers": ["codex"]},
            "active_dispatch_count": active_dispatch_count,
            "last_heartbeat_at": "2026-05-08T00:00:00Z"
        })
    }

    #[test]
    fn noop_constraint_returns_available() {
        let outcome = NoOpConstraint.check(
            &node("mac-book", "mac-book", "2026-05-08T00:00:00Z"),
            &dispatch(),
        );

        assert_eq!(outcome, ConstraintOutcome::Available);
    }

    #[test]
    fn routing_engine_selects_first_available_candidate() {
        let nodes = vec![
            node("mac-mini", "mac-mini", "2026-05-08T00:00:00Z"),
            node("mac-book", "mac-book", "2026-05-08T00:00:01Z"),
        ];
        let engine = RoutingEngine::new(vec![Box::new(NoOpConstraint)]);
        let decision = engine.route(
            &nodes,
            &json!({"preferred": {"labels": ["mac-book"]}}),
            &dispatch(),
        );

        assert_eq!(decision.selected_instance_id(), Some("mac-book"));
        assert_eq!(
            decision.candidates[0].constraints[0].outcome,
            ConstraintOutcome::Available
        );
    }

    #[test]
    fn wait_outcome_blocks_selection_and_is_recorded() {
        let nodes = vec![node("mac-book", "mac-book", "2026-05-08T00:00:00Z")];
        let engine = RoutingEngine::new(vec![Box::new(FixedConstraint {
            name: "blackout_window",
            outcome: ConstraintOutcome::wait("scheduled blackout"),
        })]);
        let decision = engine.route(&nodes, &json!({}), &dispatch());

        assert_eq!(decision.selected_instance_id(), None);
        assert_eq!(
            decision.candidates[0].final_outcome,
            ConstraintOutcome::wait("blackout_window: scheduled blackout")
        );
        assert_eq!(
            decision.constraint_results_json()[0]["constraints"][0]["outcome"]["outcome"],
            "wait"
        );
    }

    #[test]
    fn reject_outcome_blocks_selection_and_is_recorded() {
        let nodes = vec![node("mac-book", "mac-book", "2026-05-08T00:00:00Z")];
        let engine = RoutingEngine::new(vec![Box::new(FixedConstraint {
            name: "named_semaphore",
            outcome: ConstraintOutcome::reject("resource held"),
        })]);
        let decision = engine.route(&nodes, &json!({}), &dispatch());

        assert_eq!(decision.selected_instance_id(), None);
        assert_eq!(
            decision.candidates[0].final_outcome,
            ConstraintOutcome::reject("named_semaphore: resource held")
        );
        assert_eq!(
            decision.constraint_results_json()[0]["constraints"][0]["outcome"]["outcome"],
            "reject"
        );
    }

    #[test]
    fn node_concurrency_cap_selects_fallback_when_preferred_node_is_full() {
        let nodes = vec![
            node_with_active("mac-mini-release", "mac-mini", 2),
            node_with_active("mac-book-release", "mac-book", 0),
        ];
        let caps = node_caps(&[("mac-mini-release", 2), ("mac-book-release", 4)]);
        let engine = RoutingEngine::new(vec![Box::new(
            NodeConcurrencyCapConstraint::from_node_configs(Some(&caps)),
        )]);
        let decision = engine.route(
            &nodes,
            &json!({
                "providers": ["codex"],
                "preferred": {"labels": ["mac-mini", "mac-book"]}
            }),
            &dispatch(),
        );

        assert_eq!(decision.selected_instance_id(), Some("mac-book-release"));
        let mini = decision
            .candidate_for_instance("mac-mini-release")
            .expect("mac-mini candidate");
        assert_eq!(
            mini.final_outcome,
            ConstraintOutcome::wait(
                "node_concurrency_cap: node mac-mini-release active dispatches 2/2 at capacity"
            )
        );
    }

    #[test]
    fn node_concurrency_cap_waits_when_all_candidates_are_full() {
        let nodes = vec![
            node_with_active("mac-mini-release", "mac-mini", 2),
            node_with_active("mac-book-release", "mac-book", 4),
        ];
        let caps = node_caps(&[("mac-mini-release", 2), ("mac-book-release", 4)]);
        let engine = RoutingEngine::new(vec![Box::new(
            NodeConcurrencyCapConstraint::from_node_configs(Some(&caps)),
        )]);
        let decision = engine.route(&nodes, &json!({"providers": ["codex"]}), &dispatch());

        assert_eq!(decision.selected_instance_id(), None);
        assert!(decision.has_constraint_blocked_candidates());
        assert!(decision.candidates.iter().all(|candidate| {
            matches!(candidate.final_outcome, ConstraintOutcome::Wait { .. })
        }));
    }

    #[test]
    fn node_concurrency_cap_is_unlimited_when_cap_is_unset() {
        let nodes = vec![node_with_active("mac-mini-release", "mac-mini", 99)];
        let engine = RoutingEngine::new(vec![Box::new(
            NodeConcurrencyCapConstraint::from_node_configs(None),
        )]);
        let decision = engine.route(
            &nodes,
            &json!({"providers": ["codex"], "preferred": {"labels": ["mac-mini"]}}),
            &dispatch(),
        );

        assert_eq!(decision.selected_instance_id(), Some("mac-mini-release"));
        assert_eq!(
            decision.candidates[0].constraints[0].outcome,
            ConstraintOutcome::Available
        );
    }
}
