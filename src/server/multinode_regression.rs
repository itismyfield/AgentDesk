#[cfg(test)]
/// The module name carries the `_pg_` lane token deliberately. Two different
/// skip filters guard the PG-less lanes and they are not the same string set:
/// the PR sweep skips `_pg`/`pg_`/`postgres`, while the nightly `full_macos`
/// and `full_windows` jobs skip `_pg_`/`postgres_` — underscores on both sides.
/// A trailing `_pg` suffix satisfies the first and slips through the second, so
/// the token has to sit *inside* the path. As `tests` this module carried no
/// token at all and ran in every PG-less lane (#5218).
///
/// Renaming only fixes scheduling. Safety comes from the fixture below, which
/// has no host fallback: even if a filter regresses, these tests cannot reach a
/// Postgres server that the lane did not hand them.
mod multinode_regression_pg_tests {
    use crate::db::postgres::AdvisoryLockLease;
    use crate::server::cluster::CLUSTER_LEADER_ADVISORY_LOCK_ID;
    use crate::server::resource_locks::{
        ResourceLockRequest, acquire_resource_lock, release_resource_lock, unreal_project_lock_key,
    };
    use crate::server::task_dispatch_claims::{TaskDispatchClaimRequest, claim_task_dispatches};
    use serde_json::Value;
    use uuid::Uuid;

    struct TestPostgresDb {
        admin_url: String,
        database_url: String,
        database_name: String,
    }

    impl TestPostgresDb {
        /// `None` means one thing only: the shared fixture base is unconfigured,
        /// so there is no server this fixture is entitled to talk to. It never
        /// means "Postgres answered and failed" — every call below still panics
        /// on error, so a reachable-but-broken server cannot be laundered into a
        /// green run. `postgres_test_database_url_base()` additionally panics
        /// when `AGENTDESK_REQUIRE_PG=1`, so the PG lanes treat a missing base
        /// as fatal rather than skippable (#4979 S2 contract).
        ///
        /// There is deliberately no host fallback. Inventing an address made
        /// this fixture connect to whatever Postgres happened to listen on the
        /// developer's loopback and create/drop databases there (#5218).
        async fn create() -> Option<Self> {
            let base = crate::db::postgres::postgres_test_database_url_base()?;
            let database_name = format!("agentdesk_multinode_{}", Uuid::new_v4().simple());
            let admin_url = format!("{base}/postgres");
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "multinode regression tests",
            )
            .await
            .expect("create multinode regression postgres test database");
            Some(Self {
                admin_url,
                database_url: format!("{base}/{database_name}"),
                database_name,
            })
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
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "multinode regression tests",
            )
            .await
            .expect("drop multinode regression postgres test database");
        }
    }

    #[tokio::test]
    async fn multinode_single_leader_lock_allows_one_holder() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
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
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
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
        let contended_claims = claim_a.claimed.len() + claim_b.claimed.len();
        let mut trace = vec![
            describe_claim_outcome("contended mac-mini-release", &claim_a),
            describe_claim_outcome("contended mac-book-release", &claim_b),
        ];

        // Safety half of exactly-once: under real contention the same dispatch
        // must never be handed to both workers. Unconditional — no interleaving
        // may relax it.
        assert!(
            contended_claims <= 1,
            "two workers sharing PG must never claim one dispatch twice; {}",
            trace.join(" | ")
        );

        // Liveness half. Both fixture nodes satisfy the dispatch's required
        // capabilities, so `select_capability_route` elects a single route owner
        // (mac-book-release — it is inserted second, so it carries the later
        // heartbeat, and the instance_id tie-break favours it as well) and the
        // other worker skips as "not preferred route owner". A contended round
        // therefore claims nothing whenever the *non-elected* worker wins the
        // `FOR UPDATE SKIP LOCKED` race in `claim_task_dispatches`: it holds the
        // row for the length of its transaction while the elected worker's
        // select comes back empty. That is a lock-race artifact, not a claim
        // leak — the dispatch stays pending and the next poll takes it.
        //
        // The old single-round `== 1` assertion made the ubuntu PG lane depend
        // on winning that race. Stalling the claim transaction by 5ms right
        // after its select turned this test from 20/20 green into 5/5 red
        // locally, which reproduces the CI signature (claims=0) exactly (#5387).
        // So settle the round without contention and require the total to still
        // be exactly one.
        let settled_claims = if contended_claims == 0 {
            let settle = claim_task_dispatches(&pool_b, &request_b).await.unwrap();
            trace.push(describe_claim_outcome("settling mac-book-release", &settle));
            settle.claimed.len()
        } else {
            0
        };
        assert_eq!(
            contended_claims + settled_claims,
            1,
            "two workers sharing PG must claim one dispatch exactly once; {}",
            trace.join(" | ")
        );

        // Exactly-once at the row itself: one owner holds the lease, not two
        // rounds' worth of overlapping claims.
        let claim_owner: Option<String> = sqlx::query_scalar(
            "SELECT claim_owner FROM task_dispatches WHERE id = 'dispatch-multinode-1'",
        )
        .fetch_one(&pool_a)
        .await
        .unwrap();
        assert_eq!(
            claim_owner.as_deref(),
            Some("mac-book-release"),
            "claimed dispatch must be held by the elected route owner alone; {}",
            trace.join(" | ")
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
            "expired dispatch lease must be reclaimable by a different worker; {}",
            describe_claim_outcome("reclaim mac-book-release", &reclaimed)
        );
        assert_eq!(reclaimed.claimed[0].id, "dispatch-multinode-1");

        pool_a.close().await;
        pool_b.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn multinode_dispatch_claims_prefer_label_but_fallback_when_preferred_node_offline() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query("INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent 1')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kanban_cards (id, title) VALUES ('card-1', 'Card 1')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO worker_nodes (
                instance_id, hostname, role, effective_role, status, labels, capabilities,
                last_heartbeat_at, started_at, updated_at
             )
             VALUES
                ('mac-book-release', 'mac-book', 'worker', 'worker', 'online',
                 '[\"mac-book\"]'::jsonb, '{\"providers\":[\"codex\"]}'::jsonb,
                 NOW() - INTERVAL '2 minutes', NOW(), NOW()),
                ('mac-mini-release', 'mac-mini', 'worker', 'worker', 'online',
                 '[\"mac-mini\"]'::jsonb, '{\"providers\":[\"codex\"]}'::jsonb,
                 NOW(), NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                required_capabilities, created_at, updated_at
             )
             VALUES (
                'dispatch-preferred-fallback', 'card-1', 'agent-1', 'implementation',
                'pending', 'Preferred fallback',
                '{\"preferred\":{\"labels\":[\"mac-book\"]}}'::jsonb,
                NOW(), NOW()
             )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let fallback = claim_task_dispatches(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-mini-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
        )
        .await
        .unwrap();
        assert_eq!(fallback.claimed.len(), 1);
        assert_eq!(fallback.claimed[0].id, "dispatch-preferred-fallback");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn multinode_dispatch_claims_route_mixed_required_and_preferred_to_best_online_node() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query("INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent 1')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kanban_cards (id, title) VALUES ('card-1', 'Card 1')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO worker_nodes (
                instance_id, hostname, role, effective_role, status, labels, capabilities,
                last_heartbeat_at, started_at, updated_at
             )
             VALUES
                ('mac-mini-release', 'mac-mini', 'worker', 'worker', 'online',
                 '[\"mac-mini\"]'::jsonb, '{\"providers\":[\"codex\"]}'::jsonb,
                 NOW(), NOW(), NOW()),
                ('mac-book-release', 'mac-book', 'worker', 'worker', 'online',
                 '[\"mac-book\"]'::jsonb, '{\"providers\":[\"codex\"]}'::jsonb,
                 NOW() - INTERVAL '1 second', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                required_capabilities, created_at, updated_at
             )
             VALUES (
                'dispatch-mixed-routing', 'card-1', 'agent-1', 'implementation',
                'pending', 'Mixed routing',
                '{\"required\":{\"providers\":[\"codex\"]},\"preferred\":{\"labels\":[\"mac-book\"]}}'::jsonb,
                NOW(), NOW()
             )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let non_preferred = claim_task_dispatches(
            &pool,
            &TaskDispatchClaimRequest {
                claim_owner: "mac-mini-release".to_string(),
                ttl_secs: Some(60),
                limit: Some(10),
                to_agent_id: None,
                dispatch_type: None,
                lease_ttl_secs: Some(60),
            },
        )
        .await
        .unwrap();
        assert_eq!(non_preferred.claimed.len(), 0);
        assert_eq!(non_preferred.skipped.len(), 1);
        assert!(
            non_preferred.skipped[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("not preferred route owner"))
        );

        let preferred = claim_task_dispatches(
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
        assert_eq!(preferred.claimed.len(), 1);
        assert_eq!(preferred.claimed[0].id, "dispatch-mixed-routing");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn multinode_unreal_resource_lock_is_exclusive() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
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

    /// Renders a claim outcome for assert messages. The skip reasons are the
    /// part that matters: they say *why* a worker claimed nothing, which is what
    /// #5387 could not tell from `claims=0` alone. The reasons discriminate the
    /// candidates — "not preferred route owner; selected <instance>" means
    /// another node was elected (and an empty `skipped` alongside it means this
    /// worker's `FOR UPDATE SKIP LOCKED` select found the row already locked),
    /// "selected unknown" means no node was eligible at all (offline heartbeat
    /// or capability mismatch), and semaphore text points at cluster config.
    fn describe_claim_outcome(
        label: &str,
        outcome: &crate::server::task_dispatch_claims::TaskDispatchClaimOutcome,
    ) -> String {
        let claimed = outcome
            .claimed
            .iter()
            .map(|claim| format!("{} -> {}", claim.id, claim.claim_owner))
            .collect::<Vec<_>>()
            .join(", ");
        let skipped = outcome
            .skipped
            .iter()
            .map(|skip| format!("{}: {}", skip.id, skip.reasons.join("; ")))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{label} claimed=[{claimed}] skipped=[{skipped}]")
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
}
