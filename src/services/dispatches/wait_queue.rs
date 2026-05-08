//! Wait-queue wake-up for dispatch outbox routing constraints.
//!
//! Wait rows remain `pending`, but carry `wait_reason`. Claim selection skips
//! those rows until this module re-evaluates routing constraints and clears the
//! wait fields for the selected owner.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use crate::config::{ClusterConfig, ClusterDispatchRoutingConfig};
use crate::db::dispatches::outbox::wait_reason_from_routing_diagnostics;
use crate::services::dispatches::routing_constraint::{RoutingDispatch, RoutingEngine};

const DEFAULT_WAKE_LIMIT: i64 = 20;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct DispatchOutboxWakeSummary {
    pub(crate) trigger: String,
    pub(crate) reassigned: u64,
    pub(crate) timed_out: u64,
    pub(crate) still_waiting: u64,
}

impl DispatchOutboxWakeSummary {
    pub(crate) fn is_empty(&self) -> bool {
        self.reassigned == 0 && self.timed_out == 0 && self.still_waiting == 0
    }
}

pub(crate) async fn wake_waiting_dispatch_outbox_pg(
    pool: &PgPool,
    cluster_config: &ClusterConfig,
    trigger: &str,
) -> Result<DispatchOutboxWakeSummary, String> {
    let lease_ttl_secs = cluster_config.lease_ttl_secs.max(1);
    let mut worker_nodes = crate::server::cluster::list_worker_nodes(pool, lease_ttl_secs).await?;
    let routing_engine = RoutingEngine::from_cluster_config(cluster_config);
    let cluster_default = cluster_default_required_capabilities(&cluster_config.dispatch_routing);
    let now = Utc::now();

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin dispatch outbox wake tx: {error}"))?;
    let rows = sqlx::query(
        "SELECT
            o.id,
            o.dispatch_id,
            COALESCE(o.required_capabilities, td.required_capabilities) AS required_capabilities,
            o.wait_started_at
         FROM dispatch_outbox o
         LEFT JOIN task_dispatches td ON td.id = o.dispatch_id
         WHERE o.status = 'pending'
           AND o.wait_reason IS NOT NULL
         ORDER BY o.created_at ASC, o.id ASC
         FOR UPDATE OF o SKIP LOCKED
         LIMIT $1",
    )
    .bind(DEFAULT_WAKE_LIMIT)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("select dispatch outbox wait rows: {error}"))?;

    let mut summary = DispatchOutboxWakeSummary {
        trigger: trigger.to_string(),
        ..DispatchOutboxWakeSummary::default()
    };
    for row in rows {
        let outbox_id: i64 = row.get("id");
        let dispatch_id: String = row.get("dispatch_id");
        let dispatch_required = row
            .try_get::<Option<Value>, _>("required_capabilities")
            .ok()
            .flatten();
        let wait_started_at = row
            .try_get::<Option<DateTime<Utc>>, _>("wait_started_at")
            .ok()
            .flatten();
        let (effective_required, routing_origin) =
            effective_required_capabilities(dispatch_required.clone(), cluster_default.clone());
        let route_required = effective_required.clone().unwrap_or_else(|| json!({}));
        let dispatch = RoutingDispatch::new(dispatch_id.clone(), None, effective_required.clone());
        let routing_decision = routing_engine.route(&worker_nodes, &route_required, &dispatch);
        let diagnostics = wake_diagnostics(
            trigger,
            &dispatch_id,
            dispatch_required.as_ref(),
            effective_required.as_ref(),
            routing_origin,
            &routing_decision,
        );
        let wait_reason = wait_reason_from_routing_diagnostics(&diagnostics);
        let event = json!({
            "trigger": trigger,
            "checked_at": now,
            "selected": routing_decision.selected_instance_id(),
            "wait_reason": wait_reason,
        });
        let event_array = json!([event]);

        if let Some(selected) = routing_decision.selected_instance_id() {
            sqlx::query(
                "UPDATE dispatch_outbox
                    SET claim_owner = $2,
                        wait_reason = NULL,
                        wait_started_at = NULL,
                        next_attempt_at = NULL,
                        routing_diagnostics = $3,
                        constraint_results = $4,
                        wake_up_history = COALESCE(wake_up_history, '[]'::jsonb) || $5::jsonb
                  WHERE id = $1",
            )
            .bind(outbox_id)
            .bind(selected)
            .bind(&diagnostics)
            .bind(diagnostics.get("constraint_results"))
            .bind(&event_array)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("reassign dispatch outbox wait row {outbox_id}: {error}"))?;
            increment_active_dispatch_count(&mut worker_nodes, selected);
            summary.reassigned += 1;
            continue;
        }

        if wait_timed_out(
            wait_started_at,
            cluster_config.dispatch_routing.wait_timeout_secs,
            now,
        ) {
            let reason = wait_reason
                .clone()
                .unwrap_or_else(|| "dispatch outbox wait timed out".to_string());
            sqlx::query(
                "UPDATE dispatch_outbox
                    SET status = 'failed',
                        error = $2,
                        routing_diagnostics = $3,
                        constraint_results = $4,
                        wake_up_history = COALESCE(wake_up_history, '[]'::jsonb) || $5::jsonb
                  WHERE id = $1",
            )
            .bind(outbox_id)
            .bind(format!("wait timeout: {reason}"))
            .bind(&diagnostics)
            .bind(diagnostics.get("constraint_results"))
            .bind(&event_array)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("timeout dispatch outbox wait row {outbox_id}: {error}"))?;
            summary.timed_out += 1;
            continue;
        }

        sqlx::query(
            "UPDATE dispatch_outbox
                SET claim_owner = NULL,
                    wait_reason = $2,
                    wait_started_at = COALESCE(wait_started_at, NOW()),
                    next_attempt_at = NULL,
                    routing_diagnostics = $3,
                    constraint_results = $4,
                    wake_up_history = COALESCE(wake_up_history, '[]'::jsonb) || $5::jsonb
              WHERE id = $1",
        )
        .bind(outbox_id)
        .bind(wait_reason.as_deref())
        .bind(&diagnostics)
        .bind(diagnostics.get("constraint_results"))
        .bind(&event_array)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("refresh dispatch outbox wait row {outbox_id}: {error}"))?;
        summary.still_waiting += 1;
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit dispatch outbox wake tx: {error}"))?;
    Ok(summary)
}

fn cluster_default_required_capabilities(routing: &ClusterDispatchRoutingConfig) -> Option<Value> {
    if routing.default_preferred_labels.is_empty() {
        None
    } else {
        Some(json!({
            "preferred": { "labels": routing.default_preferred_labels.clone() }
        }))
    }
}

fn effective_required_capabilities(
    dispatch_required: Option<Value>,
    cluster_default: Option<Value>,
) -> (Option<Value>, &'static str) {
    if non_empty_required_capabilities(dispatch_required.as_ref()).is_some() {
        (dispatch_required, "dispatch")
    } else if cluster_default.is_some() {
        (cluster_default, "cluster_default")
    } else {
        (None, "none")
    }
}

fn non_empty_required_capabilities(required: Option<&Value>) -> Option<&Value> {
    match required {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) if map.is_empty() => None,
        Some(required) => Some(required),
    }
}

fn wake_diagnostics(
    trigger: &str,
    dispatch_id: &str,
    dispatch_required_capabilities: Option<&Value>,
    effective_required_capabilities: Option<&Value>,
    routing_origin: &str,
    routing_decision: &crate::services::dispatches::routing_constraint::RoutingEngineDecision,
) -> Value {
    json!({
        "dispatch_id": dispatch_id,
        "claim_owner": null,
        "decision": {
            "instance_id": null,
            "eligible": routing_decision.selected.is_some(),
            "reasons": Vec::<String>::new(),
        },
        "selected": &routing_decision.selected,
        "candidates": &routing_decision.candidates,
        "constraint_results": routing_decision.constraint_results_json(),
        "required_capabilities": dispatch_required_capabilities,
        "effective_required_capabilities": effective_required_capabilities,
        "routing_origin": routing_origin,
        "wake_up": {
            "trigger": trigger,
            "checked_at": Utc::now(),
        },
        "checked_at": Utc::now(),
    })
}

fn wait_timed_out(
    wait_started_at: Option<DateTime<Utc>>,
    wait_timeout_secs: Option<u64>,
    now: DateTime<Utc>,
) -> bool {
    let Some(timeout_secs) = wait_timeout_secs else {
        return false;
    };
    let Some(wait_started_at) = wait_started_at else {
        return false;
    };
    now.signed_duration_since(wait_started_at).num_seconds() >= timeout_secs as i64
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn wait_timeout_uses_wait_started_at() {
        let started = Utc.with_ymd_and_hms(2026, 5, 8, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 8, 0, 10, 0).unwrap();

        assert!(wait_timed_out(Some(started), Some(600), now));
        assert!(!wait_timed_out(Some(started), Some(601), now));
        assert!(!wait_timed_out(None, Some(600), now));
        assert!(!wait_timed_out(Some(started), None, now));
    }
}
