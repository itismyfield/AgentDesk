use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{FailedPreAcceptSweepOutcome, IntakeOutboxRow};
use crate::services::cluster::execution_requirements::tests::ready_node;
use crate::services::session_forwarding::probe::tests::ScopedReadinessProbe;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use std::path::Path;
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn isolated_home(root: &Path) -> TestResult<crate::config::TestEnvVarGuard> {
    let home = root.join("home");
    std::fs::create_dir_all(&home)?;
    Ok(
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "HOME",
            &std::fs::canonicalize(home)?,
        ),
    )
}

fn org_profile(root: &Path, profile: &str) -> TestResult {
    let home = std::fs::canonicalize(root.join("home"))?.join(".adk/profiles/claude/work");
    std::fs::create_dir_all(&home)?;
    let path = crate::runtime_layout::org_schema_path(root);
    std::fs::create_dir_all(path.parent().ok_or("org parent")?)?;
    let schema = serde_yaml::to_string(&json!({
        "version":1,
        "agents":{},
        "provider_auth_profiles":{"work":{"provider":"claude","home":home}},
        "provider_auth_primary_profiles":{"claude":profile}
    }))?;
    crate::services::discord::org_schema::parse_org_schema(&schema)?;
    std::fs::write(path, schema)?;
    Ok(())
}

fn modern_node(id: &str) -> Value {
    let mut node = ready_node(id, "linux");
    node["capabilities"]["execution_readiness_version"] = json!(1);
    node["capabilities"]["execution_readiness"]["providers"]["claude"]["credential_profiles"]["work"] =
        json!(false);
    node
}

async fn worker(pool: &PgPool, id: &str, capabilities: Value) -> TestResult {
    sqlx::query(
        "INSERT INTO worker_nodes(instance_id,status,labels,capabilities,last_heartbeat_at)
         VALUES ($1,'online','[]',$2,NOW())",
    )
    .bind(id)
    .bind(capabilities)
    .execute(pool)
    .await?;
    Ok(())
}

async fn failed_source(pool: &PgPool, id: &str, owned: bool) -> TestResult<i64> {
    sqlx::query("INSERT INTO agents(id,name,provider) VALUES ('sweep-boundary','Sweep','claude')")
        .execute(pool)
        .await?;
    let source = sqlx::query_scalar(
        "INSERT INTO intake_outbox(
            target_instance_id,forwarded_by_instance_id,required_labels,
            channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,
            provider,status,attempt_no,admission_kind,owner_instance_id,owner_generation,
            execution_requirements,attachment_refs,completed_at,updated_at)
         VALUES ($1,'leader','[]','sweep-channel','sweep-message','user','hello','standard',
            'sweep-boundary','claude','failed_pre_accept',1,$2,$3,$4,'{}','[]',NOW(),
            NOW()-INTERVAL '1 hour') RETURNING id",
    )
    .bind(id)
    .bind(if owned { "forwarded" } else { "local" })
    .bind(owned.then_some(id))
    .bind(owned.then_some(3_i64))
    .fetch_one(pool)
    .await?;
    if owned {
        sqlx::query(
            "INSERT INTO intake_session_owners(
                provider,raw_channel_id,owner_instance_id,generation,status)
             VALUES ('claude','sweep-channel',$1,3,'active')",
        )
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(source)
}

async fn source_row(pool: &PgPool, source: i64) -> TestResult<IntakeOutboxRow> {
    Ok(sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1")
        .bind(source)
        .fetch_one(pool)
        .await?)
}

async fn children(pool: &PgPool, source: i64) -> TestResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id=$1")
            .bind(source)
            .fetch_one(pool)
            .await?,
    )
}

async fn waiting_sweep(pool: &PgPool, blocker: i32) -> TestResult<(i32, DateTime<Utc>)> {
    let wait = async {
        loop {
            let found = sqlx::query_as::<_, (i32, DateTime<Utc>)>(
                "SELECT pid,xact_start FROM pg_stat_activity
                 WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid))",
            )
            .bind(blocker)
            .fetch_optional(pool)
            .await?;
            if let Some(found) = found {
                return Ok::<_, sqlx::Error>(found);
            }
            tokio::task::yield_now().await;
        }
    };
    Ok(tokio::time::timeout(Duration::from_secs(10), wait).await??)
}

async fn source_is_locked(pool: &PgPool, source: i64) -> TestResult {
    let mut probe = pool.begin().await?;
    let result = sqlx::query("SELECT id FROM intake_outbox WHERE id=$1 FOR UPDATE NOWAIT")
        .bind(source)
        .fetch_optional(&mut *probe)
        .await;
    assert!(
        matches!(&result, Err(sqlx::Error::Database(error)) if error.code().as_deref()==Some("55P03")),
        "the sweep must retain its source lock while waiting: {result:?}"
    );
    probe.rollback().await?;
    Ok(())
}

async fn hold_worker(pool: &PgPool, id: &str) -> TestResult<(Transaction<'static, Postgres>, i32)> {
    let mut blocker = pool.begin().await?;
    let backend = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await?;
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW() WHERE instance_id=$1")
        .bind(id)
        .execute(&mut *blocker)
        .await?;
    Ok((blocker, backend))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_accept_sweep_observes_profile_after_owner_lock_wait_pg() -> TestResult {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let _home = isolated_home(runtime.path())?;
    org_profile(runtime.path(), "default")?;
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let id = crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    worker(&pool, &id, modern_node(&id)["capabilities"].clone()).await?;
    let source = failed_source(&pool, &id, true).await?;
    let mut blocker = pool.begin().await?;
    let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(
            crate::domain::intake_identity::OwnerIdentity::new("claude", "sweep-channel")
                .advisory_key(),
        )
        .execute(&mut *blocker)
        .await?;
    let sweep_pool = pool.clone();
    let sweep = tokio::spawn(async move {
        sweep_failed_pre_accept_once(&sweep_pool, "leader", 5, 60, None).await
    });
    let observed = waiting_sweep(&pool, backend).await;
    if observed.is_err() {
        blocker.rollback().await?;
        let result = sweep.await?;
        panic!("sweep never waited on owner lock: {result:?}");
    }
    source_is_locked(&pool, source).await?;
    assert_eq!(
        super::super::readiness::expected_auth_profile("claude", "sweep-channel", "sweep-boundary"),
        "default"
    );
    org_profile(runtime.path(), "work")?;
    assert_eq!(
        super::super::readiness::expected_auth_profile("claude", "sweep-channel", "sweep-boundary"),
        "work"
    );
    blocker.commit().await?;
    assert_eq!(
        sweep.await??,
        FailedPreAcceptSweepOutcome::NoCapableTarget { source_id: source }
    );
    assert_eq!(children(&pool, source).await?, 0);
    pool.close().await;
    fixture.drop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_accept_sweep_observes_forwarding_expiry_after_worker_lock_wait_pg() -> TestResult {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let _home = isolated_home(runtime.path())?;
    org_profile(runtime.path(), "default")?;
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let id = format!("sweep-remote-{}", uuid::Uuid::new_v4());
    let origin = "http://127.0.0.1:1";
    let mut node = modern_node(&id);
    node["capabilities"]["agentdesk_api"]["base_url"] = json!(origin);
    node["api_base_url"] = json!(origin);
    let cache = ScopedReadinessProbe::verified(&id, origin, Utc::now().timestamp_millis() + 60_000);
    assert!(super::super::readiness::evaluate_declared(&node, "claude", "default").eligible);
    worker(&pool, &id, node["capabilities"].clone()).await?;
    let source = failed_source(&pool, &id, false).await?;
    let (blocker, backend) = hold_worker(&pool, &id).await?;
    let sweep_pool = pool.clone();
    let sweep = tokio::spawn(async move {
        sweep_failed_pre_accept_once(&sweep_pool, "leader", 5, 60, None).await
    });
    let observed = waiting_sweep(&pool, backend).await;
    if observed.is_err() {
        blocker.rollback().await?;
        let result = sweep.await?;
        panic!("sweep never waited on worker lock: {result:?}");
    }
    source_is_locked(&pool, source).await?;
    cache.expire();
    assert_eq!(
        crate::services::session_forwarding::probe::readiness_reason(&node).as_deref(),
        Some("forwarding_probe_expired")
    );
    blocker.commit().await?;
    assert_eq!(
        sweep.await??,
        FailedPreAcceptSweepOutcome::NoCapableTarget { source_id: source }
    );
    assert_eq!(children(&pool, source).await?, 0);
    pool.close().await;
    fixture.drop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_accept_sweep_retains_one_backend_and_transaction_through_insert_pg() -> TestResult {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let _home = isolated_home(runtime.path())?;
    org_profile(runtime.path(), "default")?;
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let id = "sweep-legacy-worker";
    worker(
        &pool,
        id,
        json!({"intake_worker":{"enabled":true,"providers":["claude"]}}),
    )
    .await?;
    let source = failed_source(&pool, id, false).await?;
    sqlx::raw_sql(
        "CREATE TABLE sweep_transaction_observation (backend INTEGER, started TIMESTAMPTZ);
         CREATE FUNCTION observe_sweep_transaction() RETURNS TRIGGER LANGUAGE plpgsql AS $body$
         BEGIN
           IF NEW.parent_outbox_id IS NOT NULL THEN
             INSERT INTO sweep_transaction_observation VALUES (pg_backend_pid(), transaction_timestamp());
           END IF;
           RETURN NEW;
         END $body$;
         CREATE TRIGGER observe_sweep_transaction BEFORE INSERT ON intake_outbox
           FOR EACH ROW EXECUTE FUNCTION observe_sweep_transaction();",
    )
    .execute(&pool)
    .await?;
    let (blocker, backend) = hold_worker(&pool, id).await?;
    let sweep_pool = pool.clone();
    let sweep = tokio::spawn(async move {
        sweep_failed_pre_accept_once(&sweep_pool, "leader", 5, 60, None).await
    });
    let observed = waiting_sweep(&pool, backend).await;
    if observed.is_err() {
        blocker.rollback().await?;
        let result = sweep.await?;
        panic!("sweep never waited on worker lock: {result:?}");
    }
    source_is_locked(&pool, source).await?;
    blocker.commit().await?;
    assert!(
        matches!(sweep.await??, FailedPreAcceptSweepOutcome::Retried { source_id, attempt_no:2, .. } if source_id==source)
    );
    let written: (i32, DateTime<Utc>) =
        sqlx::query_as("SELECT backend,started FROM sweep_transaction_observation")
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        written, observed?,
        "the insert must use the still-open lock-holding transaction"
    );
    assert_eq!(children(&pool, source).await?, 1);
    pool.close().await;
    fixture.drop().await;
    Ok(())
}

#[tokio::test]
async fn pre_accept_sweep_parse_error_rolls_back_without_child_or_rotation_pg() -> TestResult {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let _home = isolated_home(runtime.path())?;
    org_profile(runtime.path(), "default")?;
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let id = "sweep-legacy-worker";
    worker(
        &pool,
        id,
        json!({"intake_worker":{"enabled":true,"providers":["claude"]}}),
    )
    .await?;
    let source = failed_source(&pool, id, false).await?;
    sqlx::query("UPDATE intake_outbox SET execution_requirements=$2 WHERE id=$1")
        .bind(source)
        .bind(json!({"unknown":true}))
        .execute(&pool)
        .await?;
    let before: DateTime<Utc> =
        sqlx::query_scalar("SELECT updated_at FROM intake_outbox WHERE id=$1")
            .bind(source)
            .fetch_one(&pool)
            .await?;
    assert!(matches!(
        sweep_failed_pre_accept_once(&pool, "leader", 5, 60, None).await,
        Err(sqlx::Error::Protocol(_))
    ));
    assert_eq!(children(&pool, source).await?, 0);
    let after: DateTime<Utc> =
        sqlx::query_scalar("SELECT updated_at FROM intake_outbox WHERE id=$1")
            .bind(source)
            .fetch_one(&pool)
            .await?;
    assert_eq!(before, after);
    let mut unlocked = pool.begin().await?;
    sqlx::query("SELECT id FROM intake_outbox WHERE id=$1 FOR UPDATE NOWAIT")
        .bind(source)
        .fetch_one(&mut *unlocked)
        .await?;
    unlocked.rollback().await?;
    pool.close().await;
    fixture.drop().await;
    Ok(())
}

#[tokio::test]
async fn pre_accept_sweep_modern_unknown_and_offline_nodes_fail_closed_pg() -> TestResult {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let _home = isolated_home(runtime.path())?;
    org_profile(runtime.path(), "default")?;
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let id = "sweep-modern-unknown";
    worker(&pool, id, json!({"intake_worker":{"enabled":true,"providers":["claude"],"features":["execution_requirements_v1"]},"execution_readiness_version":1})).await?;
    let source = failed_source(&pool, id, false).await?;
    sqlx::query("UPDATE intake_outbox SET execution_requirements=$2 WHERE id=$1")
        .bind(source)
        .bind(json!({"os":["linux"]}))
        .execute(&pool)
        .await?;
    for status in ["online", "offline"] {
        sqlx::query("UPDATE worker_nodes SET status=$1 WHERE instance_id=$2")
            .bind(status)
            .bind(id)
            .execute(&pool)
            .await?;
        assert_eq!(
            sweep_failed_pre_accept_once(&pool, "leader", 5, 60, None).await?,
            FailedPreAcceptSweepOutcome::NoCapableTarget { source_id: source }
        );
        assert_eq!(children(&pool, source).await?, 0);
    }
    pool.close().await;
    fixture.drop().await;
    Ok(())
}

#[tokio::test]
async fn pre_accept_sweep_first_eligible_malformed_id_does_not_choose_later_node_pg() -> TestResult
{
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let _home = isolated_home(runtime.path())?;
    org_profile(runtime.path(), "default")?;
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let source = failed_source(&pool, "unused", false).await?;
    let row = source_row(&pool, source).await?;
    let valid = json!({"instance_id":"later","status":"online"});
    assert_eq!(
        select_target_index(&row, std::slice::from_ref(&valid))?,
        Some(0)
    );
    for malformed in [
        json!({"status":"online"}),
        json!({"instance_id":17,"status":"online"}),
    ] {
        assert_eq!(
            select_target_index(&row, &[malformed, valid.clone()])?,
            None
        );
    }
    assert_eq!(children(&pool, source).await?, 0);
    pool.close().await;
    fixture.drop().await;
    Ok(())
}

#[tokio::test]
async fn pre_accept_sweep_rejects_index_outside_its_locked_candidates_pg() -> TestResult {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let _home = isolated_home(runtime.path())?;
    org_profile(runtime.path(), "default")?;
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let id = "sweep-legacy-worker";
    worker(
        &pool,
        id,
        json!({"intake_worker":{"enabled":true,"providers":["claude"]}}),
    )
    .await?;
    let source = failed_source(&pool, id, false).await?;
    let before: DateTime<Utc> =
        sqlx::query_scalar("SELECT updated_at FROM intake_outbox WHERE id=$1")
            .bind(source)
            .fetch_one(&pool)
            .await?;
    let Preparation::Ready(prepared) = prepare_locked(pool.begin().await?, 5, 60, None).await?
    else {
        panic!("seeded source must prepare a locked candidate");
    };
    let invalid = prepared.targets().len();
    assert!(invalid > 0);
    assert!(matches!(
        (*prepared).finish_locked(Some(invalid)).await,
        Err(sqlx::Error::Protocol(_))
    ));
    assert_eq!(children(&pool, source).await?, 0);
    let after: DateTime<Utc> =
        sqlx::query_scalar("SELECT updated_at FROM intake_outbox WHERE id=$1")
            .bind(source)
            .fetch_one(&pool)
            .await?;
    assert_eq!(before, after);
    let mut unlocked = pool.begin().await?;
    sqlx::query("SELECT id FROM intake_outbox WHERE id=$1 FOR UPDATE NOWAIT")
        .bind(source)
        .fetch_one(&mut *unlocked)
        .await?;
    unlocked.rollback().await?;
    assert!(
        matches!(sweep_failed_pre_accept_once(&pool, "leader", 5, 60, None).await?, FailedPreAcceptSweepOutcome::Retried { source_id, .. } if source_id==source)
    );
    pool.close().await;
    fixture.drop().await;
    Ok(())
}
