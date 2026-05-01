#[cfg(test)]
mod tests {
    use crate::db::postgres::AdvisoryLockLease;
    use crate::server::cluster::CLUSTER_LEADER_ADVISORY_LOCK_ID;
    use crate::server::resource_locks::{
        ResourceLockRequest, acquire_resource_lock, release_resource_lock, unreal_project_lock_key,
    };
    use crate::server::task_dispatch_claims::{TaskDispatchClaimRequest, claim_task_dispatches};
    use serde_json::Value;
    use uuid::Uuid;

    struct TestPostgresDb {
        database_url: String,
        database_name: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let base = postgres_base_database_url();
            let database_name = format!("agentdesk_multinode_{}", Uuid::new_v4().simple());
            let admin_url = format!("{base}/postgres");
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "multinode regression tests",
            )
            .await
            .expect("create multinode regression postgres test database");
            Self {
                database_url: format!("{base}/{database_name}"),
                database_name,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "multinode regression tests",
            )
            .await
            .expect("connect + migrate multinode regression postgres test db")
        }

        async fn connect_pool(&self) -> sqlx::PgPool {
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(5)
                .connect(&self.database_url)
                .await
                .expect("connect multinode regression postgres test db")
        }

        async fn drop(self) {
            let base = postgres_base_database_url();
            let admin_url = format!("{base}/postgres");
            crate::db::postgres::drop_test_database(
                &admin_url,
                &self.database_name,
                "multinode regression tests",
            )
            .await
            .expect("drop multinode regression postgres test database");
        }
    }

    #[tokio::test]
    async fn multinode_single_leader_lock_allows_one_holder() {
        let pg_db = TestPostgresDb::create().await;
        let leader_pool = pg_db.connect_and_migrate().await;
        let worker_pool = pg_db.connect_pool().await;

        let leader =
            AdvisoryLockLease::try_acquire(&leader_pool, CLUSTER_LEADER_ADVISORY_LOCK_ID, "leader")
                .await
                .unwrap()
                .expect("first node must acquire leader lease");
        let denied =
            AdvisoryLockLease::try_acquire(&worker_pool, CLUSTER_LEADER_ADVISORY_LOCK_ID, "worker")
                .await
                .unwrap();
        assert!(
            denied.is_none(),
            "second node must not acquire leader lease while first holder is alive"
        );

        leader.unlock().await.unwrap();
        let replacement =
            AdvisoryLockLease::try_acquire(&worker_pool, CLUSTER_LEADER_ADVISORY_LOCK_ID, "worker")
                .await
                .unwrap();
        assert!(
            replacement.is_some(),
            "standby node must acquire leader lease after release"
        );

        leader_pool.close().await;
        worker_pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn multinode_dispatch_claims_exactly_once_then_reclaims_expired_lease() {
        let pg_db = TestPostgresDb::create().await;
        let pool_a = pg_db.connect_and_migrate().await;
        let pool_b = pg_db.connect_pool().await;
        insert_claim_fixture(&pool_a).await;

        let request_a = TaskDispatchClaimRequest {
            claim_owner: "mac-mini-release".to_string(),
            ttl_secs: Some(60),
            limit: Some(10),
            to_agent_id: None,
            dispatch_type: None,
            lease_ttl_secs: Some(60),
        };
        let request_b = TaskDispatchClaimRequest {
            claim_owner: "mac-book-release".to_string(),
            ttl_secs: Some(60),
            limit: Some(10),
            to_agent_id: None,
            dispatch_type: None,
            lease_ttl_secs: Some(60),
        };

        let (claim_a, claim_b) = tokio::join!(
            claim_task_dispatches(&pool_a, &request_a),
            claim_task_dispatches(&pool_b, &request_b)
        );
        let claim_a = claim_a.unwrap();
        let claim_b = claim_b.unwrap();
        assert_eq!(
            claim_a.claimed.len() + claim_b.claimed.len(),
            1,
            "two workers sharing PG must claim one dispatch exactly once"
        );

        sqlx::query(
            "UPDATE task_dispatches
                SET claim_expires_at = NOW() - INTERVAL '1 second'
              WHERE id = 'dispatch-multinode-1'",
        )
        .execute(&pool_a)
        .await
        .unwrap();
        let reclaimed = claim_task_dispatches(&pool_b, &request_b).await.unwrap();
        assert_eq!(
            reclaimed.claimed.len(),
            1,
            "expired dispatch lease must be reclaimable by a different worker"
        );
        assert_eq!(reclaimed.claimed[0].id, "dispatch-multinode-1");

        pool_a.close().await;
        pool_b.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn multinode_unreal_resource_lock_is_exclusive() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let lock_key = unreal_project_lock_key("CookingHeart");

        let first = acquire_resource_lock(
            &pool,
            &ResourceLockRequest {
                lock_key: lock_key.clone(),
                holder_instance_id: "mac-mini-release".to_string(),
                holder_job_id: "compile-phase".to_string(),
                ttl_secs: Some(60),
                metadata: None,
            },
        )
        .await
        .unwrap();
        assert!(first.acquired);

        let second = acquire_resource_lock(
            &pool,
            &ResourceLockRequest {
                lock_key: lock_key.clone(),
                holder_instance_id: "mac-book-release".to_string(),
                holder_job_id: "compile-phase".to_string(),
                ttl_secs: Some(60),
                metadata: None,
            },
        )
        .await
        .unwrap();
        assert!(
            !second.acquired,
            "same Unreal project lock must not be held by two workers"
        );

        assert!(
            release_resource_lock(&pool, &lock_key, "mac-mini-release", "compile-phase")
                .await
                .unwrap()
        );

        pool.close().await;
        pg_db.drop().await;
    }

    async fn insert_claim_fixture(pool: &sqlx::PgPool) {
        sqlx::query("INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent 1')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kanban_cards (id, title) VALUES ('card-1', 'Card 1')")
            .execute(pool)
            .await
            .unwrap();
        for instance_id in ["mac-mini-release", "mac-book-release"] {
            sqlx::query(
                "INSERT INTO worker_nodes (
                    instance_id, hostname, role, effective_role, status, labels, capabilities,
                    last_heartbeat_at, started_at, updated_at
                 )
                 VALUES ($1, $1, 'worker', 'worker', 'online',
                         '[\"mac\"]'::jsonb, '{\"providers\":[\"codex\"]}'::jsonb,
                         NOW(), NOW(), NOW())",
            )
            .bind(instance_id)
            .execute(pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                required_capabilities, created_at, updated_at
             )
             VALUES (
                'dispatch-multinode-1', 'card-1', 'agent-1', 'implementation',
                'pending', 'Multinode dispatch', '{\"providers\":[\"codex\"]}'::jsonb,
                NOW(), NOW()
             )",
        )
        .execute(pool)
        .await
        .unwrap();

        let required: Option<Value> =
            sqlx::query_scalar("SELECT required_capabilities FROM task_dispatches WHERE id = $1")
                .bind("dispatch-multinode-1")
                .fetch_one(pool)
                .await
                .unwrap();
        assert!(required.is_some());
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
