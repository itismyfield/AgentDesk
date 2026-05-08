use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

use crate::config::{ClusterConfig, ClusterSemaphoreConfig};

const DEFAULT_TASK_DISPATCH_CLAIM_TTL_SECS: i64 = 10 * 60;
const DEFAULT_TASK_DISPATCH_CLAIM_LIMIT: i64 = 10;

#[derive(Debug, Clone, Deserialize)]
pub struct TaskDispatchClaimRequest {
    pub claim_owner: String,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub to_agent_id: Option<String>,
    #[serde(default)]
    pub dispatch_type: Option<String>,
    #[serde(default)]
    pub lease_ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDispatchClaimOutcome {
    pub claimed: Vec<TaskDispatchClaim>,
    pub skipped: Vec<TaskDispatchClaimSkip>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDispatchClaim {
    pub id: String,
    pub kanban_card_id: Option<String>,
    pub to_agent_id: Option<String>,
    pub dispatch_type: Option<String>,
    pub title: Option<String>,
    pub claim_owner: String,
    pub claim_expires_at: DateTime<Utc>,
    pub required_capabilities: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDispatchClaimSkip {
    pub id: String,
    pub reasons: Vec<String>,
    pub required_capabilities: Option<Value>,
}

pub async fn claim_task_dispatches(
    pool: &PgPool,
    request: &TaskDispatchClaimRequest,
) -> Result<TaskDispatchClaimOutcome, String> {
    let cluster_config = crate::config::load_graceful().cluster;
    claim_task_dispatches_with_cluster_config(pool, request, &cluster_config).await
}

async fn claim_task_dispatches_with_cluster_config(
    pool: &PgPool,
    request: &TaskDispatchClaimRequest,
    cluster_config: &ClusterConfig,
) -> Result<TaskDispatchClaimOutcome, String> {
    let claim_owner = normalize_required("claim_owner", &request.claim_owner)?;
    let ttl_secs = request
        .ttl_secs
        .unwrap_or(DEFAULT_TASK_DISPATCH_CLAIM_TTL_SECS)
        .clamp(1, 24 * 60 * 60);
    let limit = request
        .limit
        .unwrap_or(DEFAULT_TASK_DISPATCH_CLAIM_LIMIT)
        .clamp(1, 50);
    let to_agent_id = clean_optional(request.to_agent_id.as_deref());
    let dispatch_type = clean_optional(request.dispatch_type.as_deref());
    let lease_ttl_secs = request.lease_ttl_secs.unwrap_or(60);
    let worker_nodes = crate::server::cluster::list_worker_nodes(pool, lease_ttl_secs).await?;
    let semaphore_configs = &cluster_config.semaphores;

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("claim task dispatches begin tx: {error}"))?;
    crate::db::dispatch_semaphores::reclaim_expired_dispatch_semaphores_on_pg_tx(&mut tx)
        .await
        .map_err(|error| format!("reclaim expired task dispatch semaphores: {error}"))?;
    let rows = sqlx::query(
        r#"
        SELECT id, kanban_card_id, to_agent_id, dispatch_type, title, required_capabilities
          FROM task_dispatches
         WHERE (
                status = 'pending'
             OR (
                    status = 'dispatched'
                AND claim_expires_at <= NOW()
                )
             OR (
                    status = 'dispatched'
                AND claim_expires_at IS NULL
                AND NOT EXISTS (
                    SELECT 1
                      FROM dispatch_semaphore_holdings active_holdings
                     WHERE active_holdings.dispatch_id = task_dispatches.id
                       AND active_holdings.expires_at > NOW()
                )
                )
           )
           AND ($1::TEXT IS NULL OR to_agent_id = $1)
           AND ($2::TEXT IS NULL OR dispatch_type = $2)
         ORDER BY created_at ASC, id ASC
         FOR UPDATE SKIP LOCKED
         LIMIT $3
        "#,
    )
    .bind(to_agent_id)
    .bind(dispatch_type)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("select task dispatch claims: {error}"))?;

    let mut claimed = Vec::new();
    let mut skipped = Vec::new();

    for row in rows {
        let id: String = row.get("id");
        let required_capabilities = row
            .try_get::<Option<Value>, _>("required_capabilities")
            .ok()
            .flatten();

        if !required_capabilities_empty(required_capabilities.as_ref()) {
            let required = required_capabilities.as_ref().expect("checked above");
            let candidates = semaphore_aware_route_candidates_on_pg_tx(
                &mut tx,
                &worker_nodes,
                required,
                semaphore_configs,
            )
            .await?;
            let selected_candidate = candidates
                .iter()
                .find(|candidate| candidate.decision.eligible);
            let selected =
                selected_candidate.and_then(|candidate| candidate.decision.instance_id.as_deref());
            let mut owner_decision = candidates
                .iter()
                .find(|candidate| candidate.decision.instance_id.as_deref() == Some(&claim_owner))
                .map(|candidate| candidate.decision.clone())
                .or_else(|| {
                    worker_nodes
                        .iter()
                        .find(|node| {
                            node.get("instance_id").and_then(|value| value.as_str())
                                == Some(claim_owner.as_str())
                        })
                        .map(|node| {
                            crate::server::cluster::explain_capability_match(node, required)
                        })
                })
                .unwrap_or_else(|| crate::server::cluster::CapabilityRouteDecision {
                    instance_id: Some(claim_owner.clone()),
                    eligible: false,
                    reasons: if candidates.is_empty() {
                        vec![
                            "no online worker node satisfies required capabilities or semaphore constraints"
                                .to_string(),
                        ]
                    } else {
                        vec![format!(
                            "claim owner is not preferred route owner; selected {}",
                            selected.unwrap_or("unknown")
                        )]
                    },
                });
            if selected != Some(claim_owner.as_str()) {
                if owner_decision.eligible && owner_decision.reasons.is_empty() {
                    owner_decision.reasons.push(format!(
                        "claim owner is not preferred route owner; selected {}",
                        selected.unwrap_or("unknown")
                    ));
                }
                let diagnostics = json!({
                    "claim_owner": claim_owner,
                    "decision": owner_decision,
                    "selected": selected_candidate,
                    "candidates": candidates,
                    "required_capabilities": required_capabilities,
                    "checked_at": Utc::now(),
                });
                sqlx::query(
                    "UPDATE task_dispatches
                        SET routing_diagnostics = $2,
                            updated_at = NOW()
                      WHERE id = $1",
                )
                .bind(&id)
                .bind(&diagnostics)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("record task dispatch routing diagnostics: {error}"))?;
                skipped.push(TaskDispatchClaimSkip {
                    id,
                    reasons: owner_decision.reasons,
                    required_capabilities,
                });
                continue;
            }

            let acquire = crate::db::dispatch_semaphores::try_acquire_dispatch_semaphores_on_pg_tx(
                &mut tx,
                &id,
                &claim_owner,
                ttl_secs,
                required_capabilities.as_ref(),
                semaphore_configs,
            )
            .await
            .map_err(|error| format!("acquire task dispatch semaphores for {id}: {error}"))?;
            if !acquire.acquired {
                owner_decision.eligible = false;
                owner_decision.reasons.extend(acquire.reasons.clone());
                let diagnostics = json!({
                    "claim_owner": claim_owner,
                    "decision": owner_decision,
                    "selected": selected_candidate,
                    "candidates": candidates,
                    "required_capabilities": required_capabilities,
                    "semaphore_acquire_reasons": acquire.reasons,
                    "checked_at": Utc::now(),
                });
                sqlx::query(
                    "UPDATE task_dispatches
                        SET routing_diagnostics = $2,
                            updated_at = NOW()
                      WHERE id = $1",
                )
                .bind(&id)
                .bind(&diagnostics)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("record task dispatch semaphore diagnostics: {error}"))?;
                skipped.push(TaskDispatchClaimSkip {
                    id,
                    reasons: owner_decision.reasons,
                    required_capabilities,
                });
                continue;
            }
        }

        let updated = sqlx::query(
            r#"
            UPDATE task_dispatches
               SET status = 'dispatched',
                   claimed_at = NOW(),
                   claim_owner = $2,
                   claim_expires_at = NOW() + ($3::BIGINT * INTERVAL '1 second'),
                   updated_at = NOW()
             WHERE id = $1
             RETURNING id, kanban_card_id, to_agent_id, dispatch_type, title,
                       claim_owner, claim_expires_at, required_capabilities
            "#,
        )
        .bind(&id)
        .bind(&claim_owner)
        .bind(ttl_secs)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| format!("claim task dispatch {id}: {error}"))?;
        claimed.push(claim_from_row(updated));
    }

    tx.commit()
        .await
        .map_err(|error| format!("claim task dispatches commit: {error}"))?;
    Ok(TaskDispatchClaimOutcome { claimed, skipped })
}

async fn semaphore_aware_route_candidates_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    worker_nodes: &[Value],
    required_capabilities: &Value,
    semaphore_configs: &BTreeMap<String, ClusterSemaphoreConfig>,
) -> Result<Vec<crate::server::cluster::CapabilityRouteCandidate>, String> {
    let mut candidates =
        crate::server::cluster::select_capability_route(worker_nodes, required_capabilities);
    if crate::db::dispatch_semaphores::required_semaphore_names(Some(required_capabilities))
        .is_empty()
    {
        return Ok(candidates);
    }

    for candidate in &mut candidates {
        let Some(instance_id) = candidate.decision.instance_id.as_deref() else {
            candidate.decision.eligible = false;
            candidate
                .decision
                .reasons
                .push("candidate is missing instance_id".to_string());
            continue;
        };
        let reasons = crate::db::dispatch_semaphores::semaphore_unavailable_reasons_on_pg_tx(
            tx,
            Some(required_capabilities),
            semaphore_configs,
            instance_id,
        )
        .await
        .map_err(|error| format!("check dispatch semaphore availability: {error}"))?;
        if !reasons.is_empty() {
            candidate.decision.eligible = false;
            candidate.decision.reasons.extend(reasons);
        }
    }

    Ok(candidates)
}

fn required_capabilities_empty(required: Option<&Value>) -> bool {
    match required {
        None | Some(Value::Null) => true,
        Some(Value::Object(map)) => map.is_empty(),
        _ => false,
    }
}

fn claim_from_row(row: sqlx::postgres::PgRow) -> TaskDispatchClaim {
    TaskDispatchClaim {
        id: row.get("id"),
        kanban_card_id: row.get("kanban_card_id"),
        to_agent_id: row.get("to_agent_id"),
        dispatch_type: row.get("dispatch_type"),
        title: row.get("title"),
        claim_owner: row.get("claim_owner"),
        claim_expires_at: row.get("claim_expires_at"),
        required_capabilities: row.get("required_capabilities"),
    }
}

fn normalize_required(field: &str, value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{field} is required"));
    }
    if trimmed.len() > 256 {
        return Err(format!("{field} is too long"));
    }
    Ok(trimmed.to_string())
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    struct TestPostgresDb {
        database_url: String,
        database_name: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let base = postgres_base_database_url();
            let database_name =
                format!("agentdesk_task_dispatch_claims_{}", Uuid::new_v4().simple());
            let admin_url = format!("{base}/postgres");
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "task_dispatch_claims tests",
            )
            .await
            .expect("create task_dispatch_claims postgres test database");
            Self {
                database_url: format!("{base}/{database_name}"),
                database_name,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "task_dispatch_claims tests",
            )
            .await
            .expect("connect + migrate task_dispatch_claims postgres test db")
        }

        async fn drop(self) {
            let base = postgres_base_database_url();
            let admin_url = format!("{base}/postgres");
            crate::db::postgres::drop_test_database(
                &admin_url,
                &self.database_name,
                "task_dispatch_claims tests",
            )
            .await
            .expect("drop task_dispatch_claims postgres test database");
        }
    }

    #[tokio::test]
    async fn claim_rejects_capability_mismatch_and_claims_eligible_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO worker_nodes (
                instance_id, hostname, role, effective_role, status, labels, capabilities,
                last_heartbeat_at, started_at, updated_at
             )
             VALUES (
                'mac-book-release', 'mac-book', 'worker', 'worker', 'online',
                '[\"mac-book\"]'::jsonb,
                '{\"providers\":[\"codex\"],\"mcp\":{\"filesystem\":{\"healthy\":true}}}'::jsonb,
                NOW(), NOW(), NOW()
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent 1')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kanban_cards (id, title) VALUES ('card-1', 'Card 1')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                required_capabilities, created_at, updated_at
             )
             VALUES
                ('disp-ok', 'card-1', 'agent-1', 'implementation', 'pending', 'OK',
                 '{\"labels\":[\"mac-book\"],\"providers\":[\"codex\"]}'::jsonb, NOW(), NOW()),
                ('disp-skip', 'card-1', 'agent-1', 'implementation', 'pending', 'Skip',
                 '{\"labels\":[\"mac-mini\"]}'::jsonb, NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let outcome = claim_task_dispatches(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-book-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.claimed.len(), 1);
        assert_eq!(outcome.claimed[0].id, "disp-ok");
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].id, "disp-skip");
        assert!(
            outcome.skipped[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("missing label"))
        );

        let status: String = sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("disp-ok")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "dispatched");
        let diagnostics: Option<Value> =
            sqlx::query_scalar("SELECT routing_diagnostics FROM task_dispatches WHERE id = $1")
                .bind("disp-skip")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(diagnostics.is_some());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn claim_falls_back_when_preferred_node_semaphore_is_exhausted() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_two_worker_nodes(&pool).await;
        seed_agent_and_card(&pool).await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-holder",
            "Holder",
            json!({"required": {"semaphores": ["ue_editor"]}}),
            "dispatched",
        )
        .await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-work",
            "Work",
            json!({
                "required": {"semaphores": ["ue_editor"]},
                "preferred": {"labels": ["mac-mini"]}
            }),
            "pending",
        )
        .await;
        seed_dispatch_semaphore_holding(
            &pool,
            "per-node",
            "mac-mini-release",
            "mac-mini-release",
            "disp-holder",
            600,
        )
        .await;

        let outcome = claim_task_dispatches_with_cluster_config(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-book-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
            &cluster_config_with_semaphore(crate::config::ClusterSemaphoreScope::PerNode),
        )
        .await
        .unwrap();

        assert_eq!(outcome.claimed.len(), 1);
        assert_eq!(outcome.claimed[0].id, "disp-work");
        assert!(outcome.skipped.is_empty());
        let holder: String = sqlx::query_scalar(
            "SELECT holder_instance_id
             FROM dispatch_semaphore_holdings
             WHERE dispatch_id = 'disp-work'
               AND semaphore_name = 'ue_editor'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(holder, "mac-book-release");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn claim_acquires_first_per_node_semaphore_on_preferred_node() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_two_worker_nodes(&pool).await;
        seed_agent_and_card(&pool).await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-first",
            "First",
            json!({
                "required": {"semaphores": ["ue_editor"]},
                "preferred": {"labels": ["mac-mini"]}
            }),
            "pending",
        )
        .await;

        let outcome = claim_task_dispatches_with_cluster_config(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-mini-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
            &cluster_config_with_semaphore(crate::config::ClusterSemaphoreScope::PerNode),
        )
        .await
        .unwrap();

        assert_eq!(outcome.claimed.len(), 1);
        assert_eq!(outcome.claimed[0].id, "disp-first");
        assert!(outcome.skipped.is_empty());
        let holder: (String, String) = sqlx::query_as(
            "SELECT holder_instance_id, scope_key
             FROM dispatch_semaphore_holdings
             WHERE dispatch_id = 'disp-first'
               AND semaphore_name = 'ue_editor'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(holder.0, "mac-mini-release");
        assert_eq!(holder.1, "mac-mini-release");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn claim_waits_when_all_per_node_semaphore_slots_are_exhausted() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_two_worker_nodes(&pool).await;
        seed_agent_and_card(&pool).await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-mini-holder",
            "Mini Holder",
            json!({"required": {"semaphores": ["ue_editor"]}}),
            "dispatched",
        )
        .await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-book-holder",
            "Book Holder",
            json!({"required": {"semaphores": ["ue_editor"]}}),
            "dispatched",
        )
        .await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-work",
            "Work",
            json!({"required": {"semaphores": ["ue_editor"]}}),
            "pending",
        )
        .await;
        seed_dispatch_semaphore_holding(
            &pool,
            "per-node",
            "mac-mini-release",
            "mac-mini-release",
            "disp-mini-holder",
            600,
        )
        .await;
        seed_dispatch_semaphore_holding(
            &pool,
            "per-node",
            "mac-book-release",
            "mac-book-release",
            "disp-book-holder",
            600,
        )
        .await;

        let outcome = claim_task_dispatches_with_cluster_config(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-book-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
            &cluster_config_with_semaphore(crate::config::ClusterSemaphoreScope::PerNode),
        )
        .await
        .unwrap();

        assert!(outcome.claimed.is_empty());
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].id, "disp-work");
        assert!(
            outcome.skipped[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("exhausted"))
        );
        let status: String = sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("disp-work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "pending");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn claim_waits_when_per_cluster_semaphore_is_exhausted() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_two_worker_nodes(&pool).await;
        seed_agent_and_card(&pool).await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-holder",
            "Holder",
            json!({"required": {"semaphores": ["ue_editor"]}}),
            "dispatched",
        )
        .await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-work",
            "Work",
            json!({"required": {"semaphores": ["ue_editor"]}}),
            "pending",
        )
        .await;
        seed_dispatch_semaphore_holding(
            &pool,
            "per-cluster",
            "cluster",
            "mac-mini-release",
            "disp-holder",
            600,
        )
        .await;

        let outcome = claim_task_dispatches_with_cluster_config(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-book-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
            &cluster_config_with_semaphore(crate::config::ClusterSemaphoreScope::PerCluster),
        )
        .await
        .unwrap();

        assert!(outcome.claimed.is_empty());
        assert_eq!(outcome.skipped.len(), 1);
        assert!(
            outcome.skipped[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("exhausted"))
        );
        let status: String = sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("disp-work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "pending");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn claim_reclaims_expired_semaphore_holding_before_acquire() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_two_worker_nodes(&pool).await;
        seed_agent_and_card(&pool).await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-expired",
            "Expired",
            json!({"required": {"semaphores": ["ue_editor"]}}),
            "failed",
        )
        .await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-work",
            "Work",
            json!({
                "required": {"semaphores": ["ue_editor"]},
                "preferred": {"labels": ["mac-mini"]}
            }),
            "pending",
        )
        .await;
        seed_dispatch_semaphore_holding(
            &pool,
            "per-node",
            "mac-mini-release",
            "mac-mini-release",
            "disp-expired",
            -60,
        )
        .await;

        let outcome = claim_task_dispatches_with_cluster_config(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-mini-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
            &cluster_config_with_semaphore(crate::config::ClusterSemaphoreScope::PerNode),
        )
        .await
        .unwrap();

        assert_eq!(outcome.claimed.len(), 1);
        assert_eq!(outcome.claimed[0].id, "disp-work");
        let expired_holdings: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM dispatch_semaphore_holdings
             WHERE dispatch_id = 'disp-expired'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(expired_holdings, 0);
        let work_holder: String = sqlx::query_scalar(
            "SELECT holder_instance_id
             FROM dispatch_semaphore_holdings
             WHERE dispatch_id = 'disp-work'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(work_holder, "mac-mini-release");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn claim_does_not_reclaim_active_null_expiry_semaphore_holder() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_two_worker_nodes(&pool).await;
        seed_agent_and_card(&pool).await;
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-holder",
            "Holder",
            json!({"required": {"semaphores": ["ue_editor"]}}),
            "dispatched",
        )
        .await;
        sqlx::query("UPDATE task_dispatches SET claim_expires_at = NULL WHERE id = $1")
            .bind("disp-holder")
            .execute(&pool)
            .await
            .unwrap();
        seed_dispatch_with_required_capabilities(
            &pool,
            "disp-work",
            "Work",
            json!({
                "required": {"semaphores": ["ue_editor"]},
                "preferred": {"labels": ["mac-mini"]}
            }),
            "pending",
        )
        .await;
        seed_dispatch_semaphore_holding(
            &pool,
            "per-node",
            "mac-mini-release",
            "mac-mini-release",
            "disp-holder",
            600,
        )
        .await;

        let outcome = claim_task_dispatches_with_cluster_config(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-book-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
            &cluster_config_with_semaphore(crate::config::ClusterSemaphoreScope::PerNode),
        )
        .await
        .unwrap();

        assert_eq!(outcome.claimed.len(), 1);
        assert_eq!(outcome.claimed[0].id, "disp-work");
        let holder_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM dispatch_semaphore_holdings
             WHERE dispatch_id = 'disp-holder'
               AND holder_instance_id = 'mac-mini-release'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(holder_count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    async fn seed_two_worker_nodes(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO worker_nodes (
                instance_id, hostname, role, effective_role, status, labels, capabilities,
                last_heartbeat_at, started_at, updated_at
             )
             VALUES
                (
                    'mac-mini-release', 'mac-mini', 'worker', 'worker', 'online',
                    '[\"mac-mini\"]'::jsonb, '{\"providers\":[\"codex\"]}'::jsonb,
                    NOW(), NOW(), NOW()
                ),
                (
                    'mac-book-release', 'mac-book', 'worker', 'worker', 'online',
                    '[\"mac-book\"]'::jsonb, '{\"providers\":[\"codex\"]}'::jsonb,
                    NOW() - INTERVAL '1 second', NOW(), NOW()
                )",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_dispatch_semaphore_holding(
        pool: &PgPool,
        scope: &str,
        scope_key: &str,
        holder_instance_id: &str,
        dispatch_id: &str,
        ttl_secs: i64,
    ) {
        sqlx::query(
            "INSERT INTO dispatch_semaphore_holdings (
                semaphore_name, scope, scope_key, slot_index, holder_instance_id,
                dispatch_id, expires_at
             )
             VALUES (
                'ue_editor', $1, $2, 0, $3, $4,
                NOW() + ($5::BIGINT * INTERVAL '1 second')
             )",
        )
        .bind(scope)
        .bind(scope_key)
        .bind(holder_instance_id)
        .bind(dispatch_id)
        .bind(ttl_secs)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_agent_and_card(pool: &PgPool) {
        sqlx::query("INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent 1')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kanban_cards (id, title) VALUES ('card-1', 'Card 1')")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn seed_dispatch_with_required_capabilities(
        pool: &PgPool,
        dispatch_id: &str,
        title: &str,
        required_capabilities: Value,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                required_capabilities, created_at, updated_at
             )
             VALUES ($1, 'card-1', 'agent-1', 'implementation', $2, $3, $4, NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(status)
        .bind(title)
        .bind(required_capabilities)
        .execute(pool)
        .await
        .unwrap();
        if status == "dispatched" {
            sqlx::query(
                "UPDATE task_dispatches
                    SET claim_owner = 'mac-mini-release',
                        claimed_at = NOW(),
                        claim_expires_at = NOW() + INTERVAL '10 minutes'
                  WHERE id = $1",
            )
            .bind(dispatch_id)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    fn cluster_config_with_semaphore(
        scope: crate::config::ClusterSemaphoreScope,
    ) -> crate::config::ClusterConfig {
        let mut config = crate::config::ClusterConfig::default();
        config.semaphores.insert(
            "ue_editor".to_string(),
            crate::config::ClusterSemaphoreConfig { capacity: 1, scope },
        );
        config
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
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "postgres".to_string());
        format!("postgres://{user}@127.0.0.1:5432")
    }
}
