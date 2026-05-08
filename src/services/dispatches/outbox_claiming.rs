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
use crate::server::cluster::CapabilityRouteDecision;
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
    let mut worker_nodes =
        match crate::server::cluster::list_worker_nodes(pool, lease_ttl_secs).await {
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
    let worker_nodes =
        crate::server::cluster::list_worker_nodes(pool, stale_threshold_secs as u64).await?;
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
    let hard_required = required
        .get("required")
        .filter(|value| value.is_object())
        .unwrap_or(required);
    match hard_required {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
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
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_outbox_claiming_{}", Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
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

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgres://{user}:{password}@{host}:{port}"),
            None => format!("postgres://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
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

        let row = candidate.into_outbox_row();
        assert_eq!(row.0, 7);
        assert_eq!(row.1, "dispatch-7");
        assert_eq!(row.2, "notify");
        assert_eq!(row.6, 2);
        assert_eq!(row.7, Some(json!({"providers": ["codex"]})));
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
