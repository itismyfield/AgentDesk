use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use serde_json::json;

/// The outbox counts as four separate statements, the shape the aggregate replaced.
async fn outbox_oracle(pool: &sqlx::PgPool) -> [i64; 4] {
    let mut values = [0; 4];
    for (slot, sql) in [
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'pending'",
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'pending' AND retry_count > 0",
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'failed'",
        "SELECT COALESCE(CAST(EXTRACT(EPOCH FROM (NOW() - MIN(COALESCE(next_attempt_at, created_at))))
             AS BIGINT), 0)
           FROM dispatch_outbox
          WHERE status = 'pending' AND (next_attempt_at IS NULL OR next_attempt_at <= NOW())",
    ]
    .iter()
    .enumerate()
    {
        values[slot] = sqlx::query_scalar(*sql).fetch_one(pool).await.unwrap();
    }
    values
}

async fn cleanup_oracle(pool: &sqlx::PgPool) -> [i64; 2] {
    let count = |sql: &'static str| sqlx::query_scalar::<_, i64>(sql).fetch_one(pool);
    [
        count("SELECT COUNT(*)::BIGINT FROM auto_queue_run_cleanup_tasks WHERE dead_lettered_at IS NULL")
            .await
            .unwrap(),
        crate::services::auto_queue::cleanup_tasks::dead_lettered_run_cleanup_task_count_pg(pool)
            .await
            .unwrap(),
    ]
}

/// One key at a time, as each report loader read it.
async fn kv_oracle(pool: &sqlx::PgPool, key: &str) -> Option<serde_json::Value> {
    let raw = sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

async fn batched(pool: &sqlx::PgPool) -> (Option<[i64; 4]>, [i64; 2], HealthKvMeta) {
    let probed = || ProbedPool::wrap(Some(pool)).unwrap();
    let outbox = load_dispatch_outbox_stats_pg(probed()).await.map(|stats| {
        [
            stats.pending,
            stats.retrying,
            stats.permanent_failures,
            stats.oldest_pending_age,
        ]
    });
    let cleanup = load_auto_queue_cleanup_backlog_pg(probed()).await.unwrap();
    let kv = load_health_kv_meta(probed()).await;
    (outbox, [cleanup.pending, cleanup.dead_lettered], kv)
}

async fn assert_same(pool: &sqlx::PgPool, label: &str) -> [i64; 4] {
    let (outbox, cleanup, kv) = batched(pool).await;
    let expected = outbox_oracle(pool).await;
    assert_eq!(outbox, Some(expected), "{label}: outbox");
    assert_eq!(cleanup, cleanup_oracle(pool).await, "{label}: cleanup");
    assert_eq!(
        kv.config_audit,
        kv_oracle(pool, "config_audit_report").await,
        "{label}"
    );
    assert_eq!(
        kv.pipeline_overrides,
        kv_oracle(pool, "pipeline_override_health_report").await,
        "{label}"
    );
    assert_eq!(
        kv.runtime_config,
        kv_oracle(pool, "runtime-config").await,
        "{label}"
    );
    expected
}

async fn exec(pool: &sqlx::PgPool, sql: &'static str) {
    sqlx::query(sql).execute(pool).await.unwrap();
}

/// The aggregates, the kv list read and the bulk session read return what the statements they
/// replaced return on the same rows; the single-channel loader is the session oracle.
#[tokio::test]
async fn health_batch_queries_preserve_main_semantics() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;

    assert_eq!(assert_same(&pool, "empty").await, [0, 0, 0, 0]);

    exec(&pool, "INSERT INTO dispatch_outbox (dispatch_id, action, status, retry_count, created_at, next_attempt_at)
        VALUES ('p0', 'a', 'pending', 0, NOW() - INTERVAL '59 seconds', NULL),
               ('p1', 'a', 'pending', 2, NOW(), NULL),
               ('future', 'a', 'pending', 1, NOW() - INTERVAL '1 hour', NOW() + INTERVAL '1 hour'),
               ('f1', 'a', 'failed', 5, NOW() - INTERVAL '2 hours', NULL),
               ('done', 'a', 'done', 0, NOW() - INTERVAL '3 hours', NULL)").await;
    assert_eq!(assert_same(&pool, "age 59").await, [3, 2, 1, 59]);
    exec(&pool, "UPDATE dispatch_outbox SET created_at = NOW() - INTERVAL '60 seconds' WHERE dispatch_id = 'p0'").await;
    assert_eq!(assert_same(&pool, "age 60").await, [3, 2, 1, 60]);
    exec(
        &pool,
        "UPDATE dispatch_outbox SET next_attempt_at = NOW() - INTERVAL '10 minutes',
        created_at = NULL WHERE dispatch_id = 'p1'",
    )
    .await;
    assert_eq!(
        assert_same(&pool, "next attempt wins").await,
        [3, 2, 1, 600]
    );
    exec(&pool, "UPDATE dispatch_outbox SET status = 'done' WHERE status = 'pending' AND next_attempt_at IS NULL OR dispatch_id = 'p1'").await;
    assert_eq!(
        assert_same(&pool, "only future pending").await,
        [1, 1, 1, 0]
    );

    exec(
        &pool,
        "INSERT INTO auto_queue_run_cleanup_tasks (run_ids, dead_lettered_at)
        VALUES (ARRAY['a'], NULL), (ARRAY['b'], NULL), (ARRAY['c'], NOW())",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO kv_meta (key, value) VALUES
        ('config_audit_report', '{\"checked\":1}'),
        ('pipeline_override_health_report', 'not json'),
        ('runtime-config', NULL),
        ('unrelated', '{}')",
    )
    .await;
    assert_same(&pool, "kv and cleanup").await;
    let (_, cleanup, kv) = batched(&pool).await;
    assert_eq!(cleanup, [2, 1]);
    assert_eq!(kv.config_audit, Some(json!({"checked": 1})));
    assert_eq!((kv.pipeline_overrides, kv.runtime_config), (None, None));
    exec(&pool, "UPDATE kv_meta SET value = '{\"dispatchRateLimitGateDangerPct\":77}' WHERE key = 'runtime-config'").await;
    assert_same(&pool, "runtime config").await;
    assert_eq!(batched(&pool).await.2.dispatch_gate_overrides().1, Some(77));

    // Latest heartbeat wins, a NULL heartbeat sorts last, and a heartbeat tie goes to the higher id.
    exec(&pool, "INSERT INTO sessions (session_key, provider, status, active_dispatch_id, thread_channel_id, last_heartbeat) VALUES
        ('a-old', 'claude', 'idle', NULL, '101', NOW() - INTERVAL '1 hour'),
        ('a-new', 'claude', 'turn_active', 'd-1', '101', NOW()),
        ('a-null', 'codex', 'turn_active', NULL, '101', NULL),
        ('b-null', 'codex', 'idle', NULL, '102', NULL),
        ('c-tie-1', 'claude', 'idle', NULL, '103', '2026-10-01T00:00:00Z'),
        ('c-tie-2', 'codex', 'awaiting_user', ' ', '103', '2026-10-01T00:00:00Z')").await;
    let channels = [101, 102, 103, 104, 101];
    let bulk = load_channel_session_states(ProbedPool::wrap(Some(&pool)).unwrap(), &channels).await;
    assert_eq!(bulk.len(), 4);
    for channel in channels {
        let oracle = super::super::load_channel_session_state(Some(&pool), channel).await;
        assert_eq!(
            serde_json::to_value(&bulk[&channel]).unwrap(),
            serde_json::to_value(&oracle).unwrap(),
            "channel {channel}"
        );
    }
    let status = |channel| {
        bulk[&channel]
            .as_ref()
            .unwrap()
            .as_ref()
            .map(|s| s.status.clone())
    };
    assert_eq!(
        [status(101), status(102), status(103), status(104)],
        [
            Some(Some("turn_active".into())),
            Some(Some("idle".into())),
            Some(Some("awaiting_user".into())),
            None
        ]
    );

    pool.close().await;
    db.drop().await;
}

/// A query error fails every channel; a row that cannot decode fails only its own channel, and
/// an unreadable channel column fails every channel with no decoded row.
#[test]
fn bulk_session_rows_isolate_decode_failures() {
    let rows = vec![("201", false), ("202", true)];
    let read = |row: &(&str, bool), field: &str| match (field, row.1) {
        ("thread_channel_id", _) => Ok(Some(row.0.to_string())),
        ("status", true) => Err(sqlx::Error::ColumnDecode {
            index: field.into(),
            source: std::io::Error::other("injected decode failure").into(),
        }),
        ("status", false) => Ok(Some("idle".to_string())),
        _ => Ok(None),
    };
    let lookups = map_session_rows(&[201, 202, 203], Ok(rows.clone()), read);
    assert!(
        matches!(&lookups[&201], Ok(Some(session)) if session.status.as_deref() == Some("idle"))
    );
    assert!(
        lookups[&202]
            .as_ref()
            .unwrap_err()
            .contains("decode session status")
    );
    assert!(matches!(lookups[&203], Ok(None)));

    let failed = map_session_rows(
        &[201, 203],
        Err::<Vec<(&str, bool)>, _>("query channel sessions: boom".into()),
        read,
    );
    assert!(
        failed
            .values()
            .all(|lookup| lookup.as_ref().is_err_and(|e| e.contains("boom")))
    );

    let keyless = |row: &(&str, bool), field: &str| match field {
        "thread_channel_id" if row.1 => Err(sqlx::Error::ColumnDecode {
            index: field.into(),
            source: std::io::Error::other("injected key failure").into(),
        }),
        _ => read(row, field),
    };
    let unattributed = map_session_rows(&[201, 202, 203], Ok(rows), keyless);
    assert!(unattributed[&201].is_ok());
    for channel in [202, 203] {
        assert!(
            unattributed[&channel]
                .as_ref()
                .unwrap_err()
                .contains("thread_channel_id")
        );
    }
}
