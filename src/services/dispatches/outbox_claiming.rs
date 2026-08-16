//! Dispatch outbox claim orchestration.
//!
//! This service owns capability matching and routing diagnostics semantics.
//! The DB outbox repository only selects locked claim candidates, marks
//! claimed rows, and persists the diagnostics payload this module builds.

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::db::dispatches::outbox::{
    DispatchOutboxRow, mark_dispatch_outbox_claimed_pg, record_routing_diagnostics_pg,
    record_task_dispatch_routing_diagnostics_pg,
    select_pending_dispatch_outbox_claim_candidates_pg,
    select_stale_dispatch_outbox_claim_owner_candidates_pg, update_dispatch_outbox_claim_owner_pg,
};
use crate::services::cluster::node_registry::CapabilityRouteDecision;
use crate::services::dispatches::routing_constraint::{
    RoutingDispatch, RoutingEngine, RoutingEngineDecision,
};

pub(crate) async fn claim_pending_dispatch_outbox_batch_pg(
    pool: &PgPool,
    claim_owner: &str,
) -> Vec<DispatchOutboxRow> {
    let cluster_config = crate::config::load_graceful().cluster;
    claim_pending_dispatch_outbox_batch_with_cluster_config_pg(pool, claim_owner, &cluster_config)
        .await
}

pub(crate) async fn claim_pending_dispatch_outbox_batch_with_cluster_config_pg(
    pool: &PgPool,
    claim_owner: &str,
    cluster_config: &crate::config::ClusterConfig,
) -> Vec<DispatchOutboxRow> {
    let lease_ttl_secs = cluster_config.lease_ttl_secs.max(1);
    let mut worker_nodes = match crate::services::cluster::node_registry::list_worker_nodes(
        pool,
        lease_ttl_secs,
    )
    .await
    {
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
    let routing_config = &cluster_config.dispatch_routing;
    let cluster_default = cluster_default_required_capabilities(routing_config);
    let routing_engine = RoutingEngine::from_cluster_config(cluster_config);

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

        let dispatch = RoutingDispatch::new(
            candidate.dispatch_id.clone(),
            None,
            effective_required.clone(),
        );
        if let Some(required) = effective_required.as_ref() {
            let owner_decision =
                capability_decision_for_claim_owner(owner_node.as_ref(), claim_owner, required);
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
                merge_offline_claim_owner_reason(
                    &mut decision,
                    owner_node.as_ref(),
                    lease_ttl_secs,
                );
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
        } else if routing_constraints_configured_for_unqualified_dispatch(cluster_config) {
            let required = json!({});
            let owner_decision =
                capability_decision_for_claim_owner(owner_node.as_ref(), claim_owner, &required);
            let routing_decision = routing_engine.route(&worker_nodes, &required, &dispatch);
            let owner_constraint_blocked = routing_decision
                .candidate_for_instance(claim_owner)
                .is_some_and(|candidate| !candidate.is_available());
            let no_available_route_due_to_constraints =
                routing_decision.selected_instance_id().is_none()
                    && routing_decision.has_constraint_blocked_candidates();

            if owner_constraint_blocked || no_available_route_due_to_constraints {
                let mut decision = owner_decision.clone();
                decision.eligible = false;
                if owner_constraint_blocked {
                    if let Some(candidate) = routing_decision.candidate_for_instance(claim_owner) {
                        decision.reasons.push(format!(
                            "claim owner blocked by routing constraint: {:?}",
                            candidate.final_outcome
                        ));
                    }
                }
                if no_available_route_due_to_constraints && decision.reasons.is_empty() {
                    decision
                        .reasons
                        .push("no route candidate is currently available".to_string());
                }
                merge_offline_claim_owner_reason(
                    &mut decision,
                    owner_node.as_ref(),
                    lease_ttl_secs,
                );
                let diagnostics = routing_diagnostics(
                    claim_owner,
                    &decision,
                    dispatch_required.as_ref(),
                    None,
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

        let claimed_at =
            match mark_dispatch_outbox_claimed_pg(&mut tx, candidate.id, claim_owner).await {
                Ok(claimed_at) => claimed_at,
                Err(error) => {
                    tracing::warn!(
                        outbox_id = candidate.id,
                        dispatch_id = candidate.dispatch_id,
                        error = %error,
                        "[dispatch-outbox] failed to claim postgres outbox row"
                    );
                    continue;
                }
            };

        pending.push(candidate.into_outbox_row(claim_owner.to_string(), claimed_at));
        increment_active_dispatch_count(&mut worker_nodes, claim_owner);
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

pub(crate) async fn reassign_stale_dispatch_outbox_claim_owners_with_cluster_config_pg(
    pool: &PgPool,
    cluster_config: &crate::config::ClusterConfig,
) -> Result<usize, String> {
    let stale_threshold_secs = stale_claim_owner_threshold_secs(cluster_config);
    let worker_nodes = crate::services::cluster::node_registry::list_worker_nodes(
        pool,
        stale_threshold_secs as u64,
    )
    .await?;
    let routing_config = &cluster_config.dispatch_routing;
    let cluster_default = cluster_default_required_capabilities(routing_config);
    let routing_engine = RoutingEngine::from_cluster_config(cluster_config);
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin stale claim-owner reassignment tx: {error}"))?;
    let candidates = select_stale_dispatch_outbox_claim_owner_candidates_pg(
        &mut tx,
        stale_threshold_secs,
        STALE_CLAIM_OWNER_REASSIGN_LIMIT,
    )
    .await
    .map_err(|error| format!("select stale dispatch outbox claim owners: {error}"))?;

    let mut reassigned = 0usize;
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

        let (new_owner, routing_decision) = if let Some(required) = effective_required.as_ref() {
            let dispatch =
                RoutingDispatch::new(candidate.dispatch_id.clone(), None, Some(required.clone()));
            let routing_decision = routing_engine.route(&worker_nodes, required, &dispatch);
            let new_owner = eligible_reassignment_owner(&routing_decision, required);
            (new_owner, Some(routing_decision))
        } else {
            (None, None)
        };

        let diagnostics = stale_claim_owner_reassignment_diagnostics(
            &candidate.stale_claim_owner,
            new_owner.as_deref(),
            candidate.stale_owner_last_heartbeat_at,
            dispatch_required.as_ref(),
            effective_required.as_ref(),
            routing_origin,
            routing_decision.as_ref(),
            &candidate.dispatch_id,
            &candidate.action,
            stale_threshold_secs,
        );
        let updated = update_dispatch_outbox_claim_owner_pg(
            &mut tx,
            candidate.id,
            new_owner.as_deref(),
            &diagnostics,
        )
        .await
        .map_err(|error| {
            format!(
                "update stale claim owner for dispatch {}: {error}",
                candidate.dispatch_id
            )
        })?;
        if updated == 0 {
            continue;
        }
        record_task_dispatch_routing_diagnostics_pg(&mut tx, &candidate.dispatch_id, &diagnostics)
            .await
            .map_err(|error| {
                format!(
                    "record stale claim owner diagnostics for dispatch {}: {error}",
                    candidate.dispatch_id
                )
            })?;
        reassigned += 1;
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit stale claim-owner reassignment tx: {error}"))?;
    Ok(reassigned)
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

fn node_concurrency_caps_configured(cluster_config: &crate::config::ClusterConfig) -> bool {
    cluster_config
        .nodes
        .values()
        .any(|node| node.max_concurrent_dispatches.is_some())
}

const STALE_CLAIM_OWNER_REASSIGN_LIMIT: i64 = 200;

fn stale_claim_owner_threshold_secs(cluster_config: &crate::config::ClusterConfig) -> i64 {
    let heartbeat = cluster_config.heartbeat_interval_secs.max(1);
    heartbeat.saturating_mul(3).clamp(1, i64::MAX as u64) as i64
}

fn eligible_reassignment_owner(
    routing_decision: &RoutingEngineDecision,
    required_capabilities: &Value,
) -> Option<String> {
    let selected = routing_decision.selected.as_ref()?;
    if !has_hard_required_capabilities(required_capabilities)
        && has_preferred_capabilities(required_capabilities)
        && selected.score <= 0
    {
        return None;
    }
    selected.instance_id().map(str::to_string)
}

fn has_hard_required_capabilities(required: &Value) -> bool {
    if let Some(hard_required) = required.get("required") {
        return capability_value_is_non_empty(hard_required);
    }
    match required {
        Value::Null => false,
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key != "preferred" && capability_value_is_non_empty(value)),
        _ => true,
    }
}

fn capability_value_is_non_empty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

fn has_preferred_capabilities(required: &Value) -> bool {
    required
        .get("preferred")
        .and_then(|value| value.as_object())
        .is_some_and(|map| !map.is_empty())
}

fn routing_constraints_configured_for_unqualified_dispatch(
    cluster_config: &crate::config::ClusterConfig,
) -> bool {
    node_concurrency_caps_configured(cluster_config)
        || !cluster_config.blackout_windows.is_empty()
        || cluster_config
            .dispatch_routing
            .constraints
            .iter()
            .any(|name| {
                name != crate::services::dispatches::routing_constraint::NOOP_CONSTRAINT_NAME
            })
}

fn increment_active_dispatch_count(worker_nodes: &mut [Value], instance_id: &str) {
    let Some(node) = worker_nodes
        .iter_mut()
        .find(|node| node.get("instance_id").and_then(Value::as_str) == Some(instance_id))
    else {
        return;
    };
    let active = node
        .get("active_dispatch_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(1);
    if let Some(object) = node.as_object_mut() {
        object.insert("active_dispatch_count".to_string(), json!(active));
    }
}

pub(crate) fn capability_decision_for_claim_owner(
    owner_node: Option<&Value>,
    claim_owner: &str,
    required_capabilities: &Value,
) -> CapabilityRouteDecision {
    owner_node
        .map(|node| {
            crate::services::cluster::node_registry::explain_capability_match(
                node,
                required_capabilities,
            )
        })
        .unwrap_or_else(|| CapabilityRouteDecision {
            instance_id: Some(claim_owner.to_string()),
            eligible: false,
            reasons: vec!["claim owner is not registered in worker_nodes".to_string()],
        })
}

/// Folds the claim owner's live registry status into a skip's reasons.
///
/// `capability_decision_for_claim_owner` explains the owner through
/// `explain_capability_match`, which answers exactly one question — do this
/// node's advertised labels/providers/MCP endpoints cover the requirement —
/// and never reads the node's computed `status`. An owner that had stopped
/// heart-beating therefore reached the skip payload as `eligible: true,
/// reasons: []`, so the operator was told only that some peer was the preferred
/// route owner and never that the owner itself was offline (#5407, the
/// `dispatch_outbox` twin of #5403).
///
/// Deliberately narrower than the #5403 fix in `claim_task_dispatches`. There
/// the owner decision fed nothing but the diagnostics payload; here what
/// `capability_decision_for_claim_owner` returns is one of the flags deciding
/// whether the row is skipped at all, and an offline owner still claims its own
/// outbox rows — it is, after all, the process running this code, whatever its
/// heartbeat says. So this merges into the cloned decision the diagnostics
/// payload is built from, once the skip has already been decided: reasons and
/// the recorded `eligible` gain the registry fact, no row changes hands.
///
/// Appends rather than prepends because `wait_reason_from_routing_diagnostics`
/// persists the first reason into `dispatch_outbox.wait_reason`, which gates
/// which rows the next claim round even looks at.
fn merge_offline_claim_owner_reason(
    decision: &mut CapabilityRouteDecision,
    owner_node: Option<&Value>,
    lease_ttl_secs: u64,
) {
    // An owner absent from `worker_nodes` already says so through
    // `capability_decision_for_claim_owner`; only the online check was missing.
    let Some(node) = owner_node else {
        return;
    };
    let status = node
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    if status == "online" {
        return;
    }
    decision.eligible = false;
    decision.reasons.push(format!(
        "claim owner worker node is not online (status '{status}'; {lease_ttl_secs}s heartbeat lease)"
    ));
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

#[allow(clippy::too_many_arguments)]
fn stale_claim_owner_reassignment_diagnostics(
    previous_owner: &str,
    new_owner: Option<&str>,
    previous_owner_last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
    dispatch_required_capabilities: Option<&Value>,
    effective_required_capabilities: Option<&Value>,
    routing_origin: &str,
    routing_decision: Option<&RoutingEngineDecision>,
    dispatch_id: &str,
    action: &str,
    stale_threshold_secs: i64,
) -> Value {
    let selected = routing_decision.and_then(|decision| decision.selected.as_ref());
    let candidates = routing_decision
        .map(|decision| json!(&decision.candidates))
        .unwrap_or_else(|| json!([]));
    let constraint_results = routing_decision
        .map(RoutingEngineDecision::constraint_results_json)
        .unwrap_or_else(|| json!([]));
    serde_json::json!({
        "event": "stale_claim_owner_reassigned",
        "reason": "claim_owner heartbeat stale",
        "dispatch_id": dispatch_id,
        "action": action,
        "previous_claim_owner": previous_owner,
        "new_claim_owner": new_owner,
        "selected": selected,
        "candidates": candidates,
        "constraint_results": constraint_results,
        "required_capabilities": dispatch_required_capabilities,
        "effective_required_capabilities": effective_required_capabilities,
        "routing_origin": routing_origin,
        "stale_threshold_secs": stale_threshold_secs,
        "previous_owner_last_heartbeat_at": previous_owner_last_heartbeat_at,
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

    /// #5407: capability matching cannot see a node's computed `status`, so the
    /// offline fact has to be merged in afterwards. What this pins is the merge's
    /// own contract and nothing more — the order of the reason vector, which is
    /// all a unit test can reach. That the appended order is what leaves
    /// `dispatch_outbox.wait_reason` on the pre-existing reason is a claim about
    /// a persisted column, and it is pinned by the
    /// `offline_owner_capability_mismatch_without_peer` scenario in
    /// `outbox_claim_verdicts_are_unchanged_by_skip_reason_diagnostics`.
    #[test]
    fn offline_claim_owner_reason_names_status_and_heartbeat_lease() {
        let owner_node = json!({
            "instance_id": "mac-mini-release",
            "status": "offline",
            "labels": ["mac-mini"],
            "capabilities": {"providers": ["codex"]},
        });
        let mut decision = capability_decision_for_claim_owner(
            Some(&owner_node),
            "mac-mini-release",
            &json!({"required": {"providers": ["codex"]}}),
        );
        assert!(
            decision.eligible && decision.reasons.is_empty(),
            "capability matching alone cannot see the node status; got {decision:?}"
        );
        decision.reasons.push(
            "claim owner is not preferred route owner; selected mac-book-release".to_string(),
        );

        merge_offline_claim_owner_reason(&mut decision, Some(&owner_node), 30);

        assert!(!decision.eligible);
        assert_eq!(
            decision.reasons,
            vec![
                "claim owner is not preferred route owner; selected mac-book-release".to_string(),
                "claim owner worker node is not online (status 'offline'; 30s heartbeat lease)"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn online_and_unregistered_claim_owners_keep_their_reasons() {
        let online_node = json!({
            "instance_id": "mac-mini-release",
            "status": "online",
            "labels": ["mac-mini"],
            "capabilities": {"providers": ["codex"]},
        });
        let mut online = capability_decision_for_claim_owner(
            Some(&online_node),
            "mac-mini-release",
            &json!({"required": {"labels": ["mac-book"]}}),
        );
        merge_offline_claim_owner_reason(&mut online, Some(&online_node), 30);
        assert!(!online.eligible);
        assert_eq!(online.reasons, vec!["missing label 'mac-book'".to_string()]);

        // The registration reason is already there; merging must not double it.
        let mut unregistered = capability_decision_for_claim_owner(
            None,
            "ghost-release",
            &json!({"required": {"providers": ["codex"]}}),
        );
        merge_offline_claim_owner_reason(&mut unregistered, None, 30);
        assert!(!unregistered.eligible);
        assert_eq!(
            unregistered.reasons,
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
    fn blackout_windows_enable_constraint_routing_for_unqualified_dispatches() {
        let mut config = crate::config::ClusterConfig::default();
        assert!(!routing_constraints_configured_for_unqualified_dispatch(
            &config
        ));

        config.blackout_windows.insert(
            "worker-a".to_string(),
            vec![crate::config::ClusterBlackoutWindowConfig {
                start: "23:00".to_string(),
                end: "23:30".to_string(),
                reason: Some("maintenance".to_string()),
            }],
        );

        assert!(routing_constraints_configured_for_unqualified_dispatch(
            &config
        ));
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

        let claimed_at = chrono::Utc::now();
        let row = candidate.into_outbox_row("worker-a".to_string(), claimed_at);
        assert_eq!(row.0, 7);
        assert_eq!(row.1, "dispatch-7");
        assert_eq!(row.2, "notify");
        assert_eq!(row.6, 2);
        assert_eq!(row.7, Some(json!({"providers": ["codex"]})));
        assert_eq!(row.8, "worker-a");
        assert_eq!(row.9, claimed_at);
    }

    #[test]
    fn increment_active_dispatch_count_updates_matching_node_only() {
        let mut worker_nodes = vec![
            json!({"instance_id": "node-a", "active_dispatch_count": 1}),
            json!({"instance_id": "node-b", "active_dispatch_count": 4}),
        ];

        increment_active_dispatch_count(&mut worker_nodes, "node-a");

        assert_eq!(worker_nodes[0]["active_dispatch_count"], 2);
        assert_eq!(worker_nodes[1]["active_dispatch_count"], 4);
    }

    #[test]
    fn hard_required_detection_ignores_preferred_only_routes() {
        assert!(!has_hard_required_capabilities(
            &json!({"preferred": {"labels": ["linux"]}})
        ));
        assert!(!has_hard_required_capabilities(&json!({
            "required": {},
            "preferred": {"labels": ["linux"]}
        })));
        assert!(has_hard_required_capabilities(
            &json!({"labels": ["mac-book"]})
        ));
        assert!(has_hard_required_capabilities(&json!({
            "required": {"labels": ["mac-book"]},
            "preferred": {"labels": ["mac-mini"]}
        })));
    }

    #[test]
    fn preferred_only_zero_score_reassignment_clears_owner() {
        let nodes = vec![json!({
            "instance_id": "mac-mini-release",
            "status": "online",
            "labels": ["mac-mini"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-08T00:00:00Z",
        })];
        let required = json!({"preferred": {"labels": ["linux"]}});
        let route_decision = RoutingEngine::new(vec![Box::new(NoOpConstraint)]).route(
            &nodes,
            &required,
            &RoutingDispatch::new("dispatch-a", None, Some(required.clone())),
        );

        assert_eq!(
            route_decision.selected_instance_id(),
            Some("mac-mini-release")
        );
        assert_eq!(route_decision.selected.as_ref().unwrap().score, 0);
        assert_eq!(
            eligible_reassignment_owner(&route_decision, &required),
            None
        );
    }

    // #4979 B0: PG 테스트를 `*_pg_tests` 하위 모듈로 이관해 `just test-postgres`의
    // `_pg` 선택자에 걸리게 한다. 아래 `#[cfg(test)]`는 컴파일상 중복이지만
    // 지우면 안 된다 — `check_test_lane_coverage.py`가 test fn을 가장 안쪽
    // `cfg(test)` 모듈에 귀속시키므로, 빼면 PG 2건이 부모 `…::tests`로 귀속되고
    // 그 부모는 `--skip _pg` 탓에 어떤 curated 호출로도 커버되지 않게 된다.
    // 이름 계약은 두 층으로 갈린다. `*_pg_tests`라는 **접미사 형태**는 어떤
    // 게이트도 강제하지 않는 사람 규약이고(bare `pg_tests` 모듈이 이미 4건
    // 있다), 실제로 강제되는 것은 정규화된 테스트 경로가 `_pg`·`pg_`·`postgres`
    // 중 하나를 부분문자열로 포함해야 한다는 것뿐이다 — `justfile`의
    // `cargo test --lib -- _pg pg_ postgres`(rule1)와 비PG 레인의 `--skip _pg
    // --skip pg_ --skip postgres`(rule2)에서 나오며, 둘 다 one-way ratchet이라
    // 위반하면 CI가 red다. 즉 `mod db_backed_tests` 같은 이름은 규약 위반이
    // 아니라 게이트 위반이다.
    #[cfg(test)]
    mod outbox_claiming_pg_tests {
        use super::*;
        use sqlx::PgPool;
        use uuid::Uuid;

        struct TestPostgresDb {
            _lock: crate::db::postgres::PostgresTestLifecycleGuard,
            admin_url: String,
            database_name: String,
            database_url: String,
        }

        impl TestPostgresDb {
            async fn create() -> Option<Self> {
                let lock = crate::db::postgres::lock_test_lifecycle();
                let Some(base) = crate::db::postgres::postgres_test_database_url_base() else {
                    drop(lock);
                    return None;
                };
                let admin_url = if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
                    let trimmed = url.trim();
                    if trimmed.is_empty() {
                        format!("{base}/postgres")
                    } else {
                        trimmed.to_string()
                    }
                } else {
                    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "postgres".to_string());
                    format!("{base}/{admin_db}")
                };
                let database_name =
                    format!("agentdesk_outbox_claiming_{}", Uuid::new_v4().simple());
                let database_url = format!("{base}/{database_name}");
                if let Err(error) = crate::db::postgres::create_test_database(
                    &admin_url,
                    &database_name,
                    "outbox claiming tests",
                )
                .await
                {
                    eprintln!("skipping outbox claiming postgres test: {error}");
                    return None;
                }

                Some(Self {
                    _lock: lock,
                    admin_url,
                    database_name,
                    database_url,
                })
            }

            async fn connect_and_migrate(&self) -> PgPool {
                crate::db::postgres::connect_test_pool_and_migrate(
                    &self.database_url,
                    "outbox claiming tests",
                )
                .await
                .expect("connect and migrate outbox claiming postgres test database")
            }

            async fn drop(self) {
                crate::db::postgres::drop_test_database(
                    &self.admin_url,
                    &self.database_name,
                    "outbox claiming tests",
                )
                .await
                .expect("drop outbox claiming postgres test database");
            }
        }

        async fn seed_worker_node(
            pool: &PgPool,
            instance_id: &str,
            labels: serde_json::Value,
            heartbeat_age_secs: i64,
        ) {
            sqlx::query(
                "INSERT INTO worker_nodes (
                instance_id, hostname, process_id, role, effective_role, status,
                labels, capabilities, last_heartbeat_at, started_at, updated_at
             ) VALUES (
                $1, $1, 100, 'auto', 'worker', 'online',
                $2, '{\"providers\":[\"codex\"]}'::jsonb,
                NOW() - ($3::BIGINT * INTERVAL '1 second'), NOW(), NOW()
             )",
            )
            .bind(instance_id)
            .bind(labels)
            .bind(heartbeat_age_secs)
            .execute(pool)
            .await
            .expect("seed worker node");
        }

        async fn seed_pending_outbox_row(
            pool: &PgPool,
            dispatch_id: &str,
            required_capabilities: serde_json::Value,
        ) {
            sqlx::query(
                "INSERT INTO dispatch_outbox (
                dispatch_id, action, status, claim_owner, required_capabilities
             ) VALUES ($1, 'notify', 'pending', NULL, $2)",
            )
            .bind(dispatch_id)
            .bind(required_capabilities)
            .execute(pool)
            .await
            .expect("seed pending outbox row");
        }

        /// The `dispatch_outbox` row state a claim round left behind, which is
        /// what "the verdict" means on this path: the claim function returns
        /// only the rows it took, and a skip is recorded on the row itself.
        #[derive(Debug, PartialEq, Eq)]
        struct ScenarioVerdict {
            claimed_dispatch_ids: Vec<String>,
            status: String,
            claim_owner: Option<String>,
            wait_reason: Option<String>,
        }

        /// Runs one claim round against a freshly seeded registry and returns
        /// the verdict only — never the reasons, which are the part #5407 is
        /// allowed to change.
        async fn run_claim_scenario(
            pool: &PgPool,
            dispatch_id: &str,
            required_capabilities: serde_json::Value,
            nodes: &[(&str, serde_json::Value, i64)],
            claim_owner: &str,
            cluster_config: &crate::config::ClusterConfig,
        ) -> ScenarioVerdict {
            sqlx::query("DELETE FROM dispatch_outbox")
                .execute(pool)
                .await
                .expect("reset dispatch_outbox");
            sqlx::query("DELETE FROM worker_nodes")
                .execute(pool)
                .await
                .expect("reset worker_nodes");
            for (instance_id, labels, heartbeat_age_secs) in nodes {
                seed_worker_node(pool, instance_id, labels.clone(), *heartbeat_age_secs).await;
            }
            seed_pending_outbox_row(pool, dispatch_id, required_capabilities).await;

            let claimed = claim_pending_dispatch_outbox_batch_with_cluster_config_pg(
                pool,
                claim_owner,
                cluster_config,
            )
            .await;

            let (status, claim_owner, wait_reason): (String, Option<String>, Option<String>) =
                sqlx::query_as(
                    "SELECT status, claim_owner, wait_reason
                       FROM dispatch_outbox
                      WHERE dispatch_id = $1",
                )
                .bind(dispatch_id)
                .fetch_one(pool)
                .await
                .expect("read back scenario outbox row");
            ScenarioVerdict {
                claimed_dispatch_ids: claimed.into_iter().map(|row| row.1).collect(),
                status,
                claim_owner,
                wait_reason,
            }
        }

        fn cluster_config_capping_node_at(
            instance_id: &str,
            cap: u32,
        ) -> crate::config::ClusterConfig {
            let mut config = crate::config::ClusterConfig::default();
            config.nodes.insert(
                instance_id.to_string(),
                crate::config::ClusterNodeConfig {
                    max_concurrent_dispatches: Some(cap),
                    ..Default::default()
                },
            );
            config
        }

        /// #5407 regression: an owner that is registered but has stopped
        /// heart-beating must say so in the skip reason. Before the fix the
        /// owner decision came from `explain_capability_match`, which never
        /// reads the node's computed `status`, so an offline owner reached the
        /// payload as `eligible: true, reasons: []` and the operator was handed
        /// a route-preference story for a node that was simply offline.
        #[tokio::test]
        async fn outbox_skip_reason_reports_offline_claim_owner_beside_route_preference() {
            let Some(pg_db) = TestPostgresDb::create().await else {
                return;
            };
            let pool = pg_db.connect_and_migrate().await;
            seed_worker_node(&pool, "mac-mini-release", json!(["mac-mini"]), 600).await;
            seed_worker_node(&pool, "mac-book-release", json!(["mac-mini"]), 0).await;
            seed_pending_outbox_row(
                &pool,
                "dispatch-offline-owner",
                json!({"required": {"providers": ["codex"]}}),
            )
            .await;

            let claimed = claim_pending_dispatch_outbox_batch_with_cluster_config_pg(
                &pool,
                "mac-mini-release",
                &crate::config::ClusterConfig::default(),
            )
            .await;
            assert!(claimed.is_empty());

            let diagnostics: serde_json::Value = sqlx::query_scalar(
                "SELECT routing_diagnostics
                   FROM dispatch_outbox
                  WHERE dispatch_id = 'dispatch-offline-owner'",
            )
            .fetch_one(&pool)
            .await
            .expect("offline-owner skip must persist routing diagnostics");
            let reasons = diagnostics["decision"]["reasons"]
                .as_array()
                .expect("skip diagnostics must carry reasons")
                .iter()
                .filter_map(|reason| reason.as_str())
                .collect::<Vec<_>>();
            assert!(
                reasons.iter().any(|reason| *reason
                    == "claim owner worker node is not online (status 'offline'; 30s heartbeat lease)"),
                "skip reason must name the offline owner node; got {reasons:?}"
            );
            assert!(
                reasons.iter().any(|reason| *reason
                    == "claim owner is not preferred route owner; selected mac-book-release"),
                "the route-preference reason must survive alongside it; got {reasons:?}"
            );
            assert_eq!(
                diagnostics["decision"]["eligible"],
                serde_json::Value::Bool(false),
                "an offline owner is not an eligible route target; got {diagnostics}"
            );

            pool.close().await;
            pg_db.drop().await;
        }

        /// #5407 guard: the skip-reason work is diagnostics only. Every verdict
        /// below was captured against the pre-fix binary and must stay
        /// byte-identical — reasons may gain detail, but who claims what, and
        /// the `wait_reason` that decides which rows the next round looks at,
        /// may not move.
        ///
        /// Pinning `wait_reason` only constrains the merge order in one shape.
        /// `wait_reason_from_routing_diagnostics` reads `constraint_results`
        /// first and only falls back to `decision.reasons[0]`, so the appended
        /// offline reason can reach the column solely in a scenario that leaves
        /// `constraint_results` without a wait/reject outcome *and* has the
        /// owner offline. That is `offline_owner_capability_mismatch_without_peer`
        /// below; without it every scenario here is green with the merge
        /// prepending, and this test pins the claim verdicts but not the column.
        #[tokio::test]
        async fn outbox_claim_verdicts_are_unchanged_by_skip_reason_diagnostics() {
            let Some(pg_db) = TestPostgresDb::create().await else {
                return;
            };
            let pool = pg_db.connect_and_migrate().await;
            let default_config = crate::config::ClusterConfig::default();

            let online_eligible_owner = run_claim_scenario(
                &pool,
                "dispatch-online-owner",
                json!({"required": {"providers": ["codex"]}}),
                &[("mac-mini-release", json!(["mac-mini"]), 0)],
                "mac-mini-release",
                &default_config,
            )
            .await;
            assert_eq!(
                online_eligible_owner,
                ScenarioVerdict {
                    claimed_dispatch_ids: vec!["dispatch-online-owner".to_string()],
                    status: "processing".to_string(),
                    claim_owner: Some("mac-mini-release".to_string()),
                    wait_reason: None,
                },
                "online eligible owner must still claim"
            );

            // The load-bearing one: a stale heartbeat does not stop the process
            // that is running this code from claiming its own outbox rows.
            let offline_owner_alone = run_claim_scenario(
                &pool,
                "dispatch-offline-owner-alone",
                json!({"required": {"providers": ["codex"]}}),
                &[("mac-mini-release", json!(["mac-mini"]), 600)],
                "mac-mini-release",
                &default_config,
            )
            .await;
            assert_eq!(
                offline_owner_alone,
                ScenarioVerdict {
                    claimed_dispatch_ids: vec!["dispatch-offline-owner-alone".to_string()],
                    status: "processing".to_string(),
                    claim_owner: Some("mac-mini-release".to_string()),
                    wait_reason: None,
                },
                "offline owner must still claim its own outbox rows"
            );

            let offline_owner_with_online_peer = run_claim_scenario(
                &pool,
                "dispatch-offline-owner-online-peer",
                json!({"required": {"providers": ["codex"]}}),
                &[
                    ("mac-mini-release", json!(["mac-mini"]), 600),
                    ("mac-book-release", json!(["mac-mini"]), 0),
                ],
                "mac-mini-release",
                &default_config,
            )
            .await;
            assert_eq!(
                offline_owner_with_online_peer,
                ScenarioVerdict {
                    claimed_dispatch_ids: Vec::new(),
                    status: "pending".to_string(),
                    claim_owner: Some("mac-book-release".to_string()),
                    wait_reason: None,
                },
                "offline owner must still skip while an online peer is the route owner"
            );

            let unregistered_owner = run_claim_scenario(
                &pool,
                "dispatch-unregistered-owner",
                json!({"required": {"providers": ["codex"]}}),
                &[("mac-book-release", json!(["mac-book"]), 0)],
                "ghost-release",
                &default_config,
            )
            .await;
            assert_eq!(
                unregistered_owner,
                ScenarioVerdict {
                    claimed_dispatch_ids: Vec::new(),
                    status: "pending".to_string(),
                    claim_owner: Some("mac-book-release".to_string()),
                    wait_reason: None,
                },
                "owner missing from worker_nodes must still skip"
            );

            let capability_mismatch = run_claim_scenario(
                &pool,
                "dispatch-capability-mismatch",
                json!({"required": {"labels": ["mac-mini"]}}),
                &[("mac-book-release", json!(["mac-book"]), 0)],
                "mac-book-release",
                &default_config,
            )
            .await;
            assert_eq!(
                capability_mismatch,
                ScenarioVerdict {
                    claimed_dispatch_ids: Vec::new(),
                    status: "pending".to_string(),
                    claim_owner: None,
                    wait_reason: Some("missing label 'mac-mini'".to_string()),
                },
                "capability mismatch must still skip"
            );

            // The skip path where the merge order is actually observable in the
            // column: the owner is offline *and* capability-mismatched, and no
            // peer survives capability selection, so `constraint_results` is
            // empty and `decision.reasons[0]` is the only source
            // `wait_reason_from_routing_diagnostics` has left. The offline
            // reason is appended behind the capability reason, so the column
            // keeps naming the mismatch; prepending it would rewrite
            // `wait_reason` and change which rows the next round looks at.
            let offline_owner_capability_mismatch_without_peer = run_claim_scenario(
                &pool,
                "dispatch-offline-owner-capability-mismatch",
                json!({"required": {"labels": ["mac-mini"]}}),
                &[("mac-book-release", json!(["mac-book"]), 600)],
                "mac-book-release",
                &default_config,
            )
            .await;
            assert_eq!(
                offline_owner_capability_mismatch_without_peer,
                ScenarioVerdict {
                    claimed_dispatch_ids: Vec::new(),
                    status: "pending".to_string(),
                    claim_owner: None,
                    wait_reason: Some("missing label 'mac-mini'".to_string()),
                },
                "an offline, capability-mismatched owner with no selectable peer must still wait on the mismatch"
            );

            // Sourced from `constraint_results`, which
            // `wait_reason_from_routing_diagnostics` consults ahead of
            // `decision.reasons` — so the owner's reasons, the appended offline
            // one included, never reach the column here. Pinned to show the
            // capacity wait survives the extra reason, not to guard its order.
            let offline_owner_with_capped_peer = run_claim_scenario(
                &pool,
                "dispatch-offline-owner-capped-peer",
                json!({"required": {"providers": ["codex"]}}),
                &[
                    ("mac-mini-release", json!(["mac-mini"]), 600),
                    ("mac-book-release", json!(["mac-mini"]), 0),
                ],
                "mac-mini-release",
                &cluster_config_capping_node_at("mac-book-release", 0),
            )
            .await;
            assert_eq!(
                offline_owner_with_capped_peer,
                ScenarioVerdict {
                    claimed_dispatch_ids: Vec::new(),
                    status: "pending".to_string(),
                    claim_owner: None,
                    wait_reason: Some(
                        "node_concurrency_cap: node mac-book-release active dispatches 0/0 at capacity"
                            .to_string()
                    ),
                },
                "an offline owner whose only peer is at capacity must still skip and wait"
            );

            pool.close().await;
            pg_db.drop().await;
        }

        #[tokio::test]
        async fn reassign_stale_claim_owners_moves_100_pending_rows_to_live_label_match() {
            let Some(pg_db) = TestPostgresDb::create().await else {
                return;
            };
            let pool = pg_db.connect_and_migrate().await;
            seed_worker_node(&pool, "stale-node", json!(["mac-book"]), 45).await;
            seed_worker_node(&pool, "mac-mini-release", json!(["mac-mini"]), 0).await;

            sqlx::query(
                "INSERT INTO dispatch_outbox (
                dispatch_id, action, status, claim_owner, required_capabilities
             )
             SELECT 'dispatch-stale-' || gs, 'notify', 'pending', 'stale-node', $1
               FROM generate_series(1, 100) gs",
            )
            .bind(json!({"preferred": {"labels": ["mac-book", "mac-mini"]}}))
            .execute(&pool)
            .await
            .expect("seed stale claim-owner outbox rows");

            let mut config = crate::config::ClusterConfig::default();
            config.heartbeat_interval_secs = 10;
            let reassigned =
                reassign_stale_dispatch_outbox_claim_owners_with_cluster_config_pg(&pool, &config)
                    .await
                    .expect("reassign stale claim owners");
            assert_eq!(reassigned, 100);

            let new_owner_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)::BIGINT
               FROM dispatch_outbox
              WHERE claim_owner = 'mac-mini-release'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(new_owner_count, 100);

            let diagnostics: serde_json::Value = sqlx::query_scalar(
                "SELECT routing_diagnostics
               FROM dispatch_outbox
              WHERE dispatch_id = 'dispatch-stale-1'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(diagnostics["event"], "stale_claim_owner_reassigned");
            assert_eq!(diagnostics["previous_claim_owner"], "stale-node");
            assert_eq!(diagnostics["new_claim_owner"], "mac-mini-release");
            assert_eq!(
                diagnostics["selected"]["decision"]["instance_id"],
                "mac-mini-release"
            );

            pool.close().await;
            pg_db.drop().await;
        }

        #[tokio::test]
        async fn reassign_stale_claim_owner_clears_owner_when_no_live_candidate_matches() {
            let Some(pg_db) = TestPostgresDb::create().await else {
                return;
            };
            let pool = pg_db.connect_and_migrate().await;
            seed_worker_node(&pool, "stale-node", json!(["mac-book"]), 45).await;
            seed_worker_node(&pool, "mac-mini-release", json!(["mac-mini"]), 0).await;

            sqlx::query(
                "INSERT INTO dispatch_outbox (
                dispatch_id, action, status, claim_owner, required_capabilities
             ) VALUES (
                'dispatch-no-candidate', 'notify', 'pending', 'stale-node', $1
             )",
            )
            .bind(json!({"required": {"labels": ["linux"]}}))
            .execute(&pool)
            .await
            .expect("seed no-candidate stale claim-owner outbox row");

            let config = crate::config::ClusterConfig::default();
            let reassigned =
                reassign_stale_dispatch_outbox_claim_owners_with_cluster_config_pg(&pool, &config)
                    .await
                    .expect("clear stale claim owner");
            assert_eq!(reassigned, 1);

            let (claim_owner, diagnostics): (Option<String>, serde_json::Value) = sqlx::query_as(
                "SELECT claim_owner, routing_diagnostics
               FROM dispatch_outbox
              WHERE dispatch_id = 'dispatch-no-candidate'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(claim_owner.is_none());
            assert_eq!(diagnostics["previous_claim_owner"], "stale-node");
            assert!(diagnostics["new_claim_owner"].is_null());
            assert!(diagnostics["selected"].is_null());

            pool.close().await;
            pg_db.drop().await;
        }
    }
}
