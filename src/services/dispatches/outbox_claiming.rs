//! Dispatch outbox claim orchestration.
//!
//! This service owns capability matching and routing diagnostics semantics.
//! The DB outbox repository only selects locked claim candidates, marks
//! claimed rows, and persists the diagnostics payload this module builds.

use serde_json::Value;
use sqlx::PgPool;

use crate::db::dispatches::outbox::{
    DispatchOutboxRow, mark_dispatch_outbox_claimed_pg, record_routing_diagnostics_pg,
    select_pending_dispatch_outbox_claim_candidates_pg,
};
use crate::server::cluster::CapabilityRouteDecision;
use crate::services::dispatches::routing_constraint::{
    RoutingDispatch, RoutingEngine, RoutingEngineDecision,
};

pub(crate) async fn claim_pending_dispatch_outbox_batch_pg(
    pool: &PgPool,
    claim_owner: &str,
) -> Vec<DispatchOutboxRow> {
    let lease_ttl_secs = 60u64;
    let worker_nodes = match crate::server::cluster::list_worker_nodes(pool, lease_ttl_secs).await {
        Ok(nodes) => nodes,
        Err(error) => {
            tracing::warn!(
                claim_owner,
                error,
                "[dispatch-outbox] failed to list worker nodes for routing"
            );
            Vec::new()
        }
    };
    let owner_node = worker_nodes
        .iter()
        .find(|node| node.get("instance_id").and_then(|value| value.as_str()) == Some(claim_owner))
        .cloned();
    let routing_config = crate::config::load_graceful().cluster.dispatch_routing;
    let cluster_default = cluster_default_required_capabilities(&routing_config);
    let routing_engine = RoutingEngine::from_config(&routing_config);

    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!("[dispatch-outbox] failed to begin postgres claim transaction: {error}");
            return Vec::new();
        }
    };

    let candidates =
        match select_pending_dispatch_outbox_claim_candidates_pg(&mut tx, claim_owner).await {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::warn!("[dispatch-outbox] failed to select postgres outbox rows: {error}");
                let _ = tx.rollback().await;
                return Vec::new();
            }
        };

    let mut pending = Vec::new();
    for candidate in candidates {
        let dispatch_required = candidate.required_capabilities.clone();
        let routing_origin: &'static str =
            if non_empty_required_capabilities(dispatch_required.as_ref()).is_some() {
                "dispatch"
            } else if cluster_default.is_some() {
                "cluster_default"
            } else {
                "none"
            };
        let effective_required: Option<Value> = match routing_origin {
            "dispatch" => dispatch_required.clone(),
            "cluster_default" => cluster_default.clone(),
            _ => None,
        };

        if let Some(required) = effective_required.as_ref() {
            let owner_decision =
                capability_decision_for_claim_owner(owner_node.as_ref(), claim_owner, required);
            let dispatch = RoutingDispatch::new(
                candidate.dispatch_id.clone(),
                None,
                effective_required.clone(),
            );
            let routing_decision = routing_engine.route(&worker_nodes, required, &dispatch);
            let selected = routing_decision.selected_instance_id();
            let preference_mismatch = selected.is_some() && selected != Some(claim_owner);
            let owner_constraint_blocked = routing_decision
                .candidate_for_instance(claim_owner)
                .is_some_and(|candidate| !candidate.is_available());
            let no_available_route_due_to_constraints =
                selected.is_none() && routing_decision.has_constraint_blocked_candidates();

            if !owner_decision.eligible
                || preference_mismatch
                || owner_constraint_blocked
                || no_available_route_due_to_constraints
            {
                let mut decision = owner_decision.clone();
                if preference_mismatch && decision.eligible && decision.reasons.is_empty() {
                    decision.reasons.push(format!(
                        "claim owner is not preferred route owner; selected {}",
                        selected.unwrap_or("unknown")
                    ));
                }
                if owner_constraint_blocked {
                    decision.eligible = false;
                    if let Some(candidate) = routing_decision.candidate_for_instance(claim_owner) {
                        decision.reasons.push(format!(
                            "claim owner blocked by routing constraint: {:?}",
                            candidate.final_outcome
                        ));
                    }
                }
                if no_available_route_due_to_constraints && decision.reasons.is_empty() {
                    decision.eligible = false;
                    decision
                        .reasons
                        .push("no route candidate is currently available".to_string());
                }
                let diagnostics = routing_diagnostics(
                    claim_owner,
                    &decision,
                    dispatch_required.as_ref(),
                    effective_required.as_ref(),
                    routing_origin,
                    &routing_decision,
                );
                record_routing_diagnostics_pg(
                    &mut tx,
                    candidate.id,
                    &candidate.dispatch_id,
                    &diagnostics,
                )
                .await;
                continue;
            }
        }

        if let Err(error) =
            mark_dispatch_outbox_claimed_pg(&mut tx, candidate.id, claim_owner).await
        {
            tracing::warn!(
                outbox_id = candidate.id,
                dispatch_id = candidate.dispatch_id,
                error = %error,
                "[dispatch-outbox] failed to claim postgres outbox row"
            );
            continue;
        }

        pending.push(candidate.into_outbox_row());
        if pending.len() >= 5 {
            break;
        }
    }

    if let Err(error) = tx.commit().await {
        tracing::warn!("[dispatch-outbox] failed to commit postgres outbox claims: {error}");
        return Vec::new();
    }

    pending.sort_by_key(|row| row.0);
    pending
}

fn cluster_default_required_capabilities(
    routing: &crate::config::ClusterDispatchRoutingConfig,
) -> Option<Value> {
    if routing.default_preferred_labels.is_empty() {
        None
    } else {
        Some(serde_json::json!({
            "preferred": { "labels": routing.default_preferred_labels.clone() }
        }))
    }
}

fn non_empty_required_capabilities(required: Option<&Value>) -> Option<&Value> {
    match required {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) if map.is_empty() => None,
        Some(required) => Some(required),
    }
}

fn capability_decision_for_claim_owner(
    owner_node: Option<&Value>,
    claim_owner: &str,
    required_capabilities: &Value,
) -> CapabilityRouteDecision {
    owner_node
        .map(|node| crate::server::cluster::explain_capability_match(node, required_capabilities))
        .unwrap_or_else(|| CapabilityRouteDecision {
            instance_id: Some(claim_owner.to_string()),
            eligible: false,
            reasons: vec!["claim owner is not registered in worker_nodes".to_string()],
        })
}

fn routing_diagnostics(
    claim_owner: &str,
    decision: &CapabilityRouteDecision,
    dispatch_required_capabilities: Option<&Value>,
    effective_required_capabilities: Option<&Value>,
    routing_origin: &str,
    routing_decision: &RoutingEngineDecision,
) -> Value {
    serde_json::json!({
        "claim_owner": claim_owner,
        "decision": decision,
        "selected": &routing_decision.selected,
        "candidates": &routing_decision.candidates,
        "constraint_results": routing_decision.constraint_results_json(),
        "required_capabilities": dispatch_required_capabilities,
        "effective_required_capabilities": effective_required_capabilities,
        "routing_origin": routing_origin,
        "checked_at": chrono::Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dispatches::outbox::DispatchOutboxClaimCandidate;
    use crate::services::dispatches::routing_constraint::{NoOpConstraint, RoutingEngine};
    use serde_json::json;

    #[test]
    fn non_empty_required_capabilities_handles_null_and_empty_object() {
        assert!(non_empty_required_capabilities(None).is_none());
        assert!(non_empty_required_capabilities(Some(&Value::Null)).is_none());
        assert!(non_empty_required_capabilities(Some(&json!({}))).is_none());
        assert!(non_empty_required_capabilities(Some(&json!({"provider": "codex"}))).is_some());
        assert!(non_empty_required_capabilities(Some(&json!(["codex"]))).is_some());
    }

    #[test]
    fn unregistered_claim_owner_is_ineligible() {
        let decision =
            capability_decision_for_claim_owner(None, "missing-node", &json!({"labels": ["mac"]}));
        assert!(!decision.eligible);
        assert_eq!(decision.instance_id.as_deref(), Some("missing-node"));
        assert_eq!(
            decision.reasons,
            vec!["claim owner is not registered in worker_nodes".to_string()]
        );
    }

    #[test]
    fn routing_diagnostics_contains_required_payload() {
        let decision = CapabilityRouteDecision {
            instance_id: Some("worker-a".to_string()),
            eligible: false,
            reasons: vec!["missing required label mac-book".to_string()],
        };
        let required = json!({"labels": ["mac-book"]});
        let route_nodes = vec![json!({
            "instance_id": "worker-a",
            "status": "online",
            "labels": ["mac-book"],
            "capabilities": {},
            "last_heartbeat_at": "2026-05-08T00:00:00Z"
        })];
        let route_decision = RoutingEngine::new(vec![Box::new(NoOpConstraint)]).route(
            &route_nodes,
            &required,
            &RoutingDispatch::new(
                "dispatch-a",
                Some("implementation".to_string()),
                Some(required.clone()),
            ),
        );
        let diagnostics = routing_diagnostics(
            "worker-a",
            &decision,
            Some(&required),
            Some(&required),
            "dispatch",
            &route_decision,
        );

        assert_eq!(diagnostics["claim_owner"], "worker-a");
        assert_eq!(diagnostics["decision"]["eligible"], false);
        assert_eq!(diagnostics["required_capabilities"], required);
        assert_eq!(diagnostics["effective_required_capabilities"], required);
        assert_eq!(diagnostics["routing_origin"], "dispatch");
        assert_eq!(
            diagnostics["constraint_results"][0]["constraints"][0]["constraint"],
            "noop"
        );
        assert_eq!(
            diagnostics["constraint_results"][0]["constraints"][0]["outcome"]["outcome"],
            "available"
        );
        assert!(diagnostics["checked_at"].is_string());
    }

    #[test]
    fn cluster_default_required_capabilities_returns_none_when_no_labels() {
        let routing = crate::config::ClusterDispatchRoutingConfig::default();
        assert!(routing.default_preferred_labels.is_empty());
    }

    #[test]
    fn claim_candidate_converts_to_legacy_row_shape() {
        let candidate = DispatchOutboxClaimCandidate {
            id: 7,
            dispatch_id: "dispatch-7".to_string(),
            action: "notify".to_string(),
            agent_id: Some("agent".to_string()),
            card_id: Some("card".to_string()),
            title: Some("title".to_string()),
            retry_count: 2,
            required_capabilities: Some(json!({"providers": ["codex"]})),
        };

        let row = candidate.into_outbox_row();
        assert_eq!(row.0, 7);
        assert_eq!(row.1, "dispatch-7");
        assert_eq!(row.2, "notify");
        assert_eq!(row.6, 2);
        assert_eq!(row.7, Some(json!({"providers": ["codex"]})));
    }
}
