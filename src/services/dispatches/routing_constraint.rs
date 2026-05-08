use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::ClusterDispatchRoutingConfig;
use crate::server::cluster::CapabilityRouteCandidate;

pub(crate) const NOOP_CONSTRAINT_NAME: &str = "noop";

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

type ConstraintFactory = fn() -> Box<dyn RoutingConstraint>;

fn noop_constraint() -> Box<dyn RoutingConstraint> {
    Box::new(NoOpConstraint)
}

const ROUTING_CONSTRAINT_FACTORIES: &[(&str, ConstraintFactory)] =
    &[(NOOP_CONSTRAINT_NAME, noop_constraint)];

pub(crate) fn constraints_from_config(
    config: &ClusterDispatchRoutingConfig,
) -> Vec<Box<dyn RoutingConstraint>> {
    constraints_from_names(&config.constraints)
}

fn constraints_from_names(names: &[String]) -> Vec<Box<dyn RoutingConstraint>> {
    names
        .iter()
        .filter_map(|name| {
            ROUTING_CONSTRAINT_FACTORIES
                .iter()
                .find(|(registered, _)| registered == &name.as_str())
                .map(|(_, factory)| factory())
                .or_else(|| {
                    tracing::warn!(
                        constraint = name.as_str(),
                        "[dispatch-routing] unknown routing constraint configured"
                    );
                    None
                })
        })
        .collect()
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
}
