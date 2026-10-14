use super::super::tests::{owner, record, wire};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::session_host::legacy_return::tests::{ended, provider_alive};
use serde_json::json;

async fn setup() -> (TestPostgresDb, PgPool, HostedObservation, HostedExecution) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let mut r = record(&owner("legacy-return"), "n-old", HostedState::Retired);
    r.expected.as_mut().unwrap().provider_process.start = "linux-ticks:1".into();
    let id = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, identity_kind, discord_token_hash, channel_id,
                               hosted_execution, status, thread_channel_id, last_heartbeat,
                               claude_session_id, raw_provider_session_id)
         VALUES ('legacy-return-key', 'claude', 'discord_channel', $1, $2, $3,
                 'disconnected', 'legacy-return-thread', NOW() - INTERVAL '2 hours', 'native-id', 'native-id')
         RETURNING id",
    ).bind(&r.owner.discord_token_hash).bind(&r.owner.channel_id).bind(wire(&r))
        .fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO session_key_aliases (session_id, session_key) VALUES ($1, 'legacy-return-alias')")
        .bind(id).execute(&pool).await.unwrap();
    let observed = HostedObservation {
        session_id: id,
        raw: Some(wire(&r)),
        record: HostedRecord::Known(r.clone()),
    };
    (db, pool, observed, r)
}

async fn snapshot(pool: &PgPool, id: i64) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn legacy_return_exact_retired_pg_preserves_identity_and_survives_gc() {
    let (db, pool, observed, r) = setup().await;
    let before = snapshot(&pool, observed.session_id).await;
    assert_eq!(
        return_legacy_pg(&pool, &observed, &r.owner, &r.execution_nonce, &ended(&r))
            .await
            .unwrap(),
        HostedCasOutcome::Written
    );
    // These are the real GC and disconnected cleanup implementations, with only tmux presence injected.
    assert!(
        crate::db::dispatched_sessions::gc_stale_thread_sessions_with_probe_pg(&pool, |_| async {
            crate::services::platform::tmux::SessionPresence::Missing
        })
        .await
        .is_empty()
    );
    assert_eq!(delete_disconnected_sessions_pg(&pool).await.unwrap(), 0);
    let after = snapshot(&pool, observed.session_id).await;
    for field in [
        "id",
        "session_key",
        "provider",
        "identity_kind",
        "discord_token_hash",
        "channel_id",
        "claude_session_id",
        "raw_provider_session_id",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert!(after["hosted_execution"].is_null());
    assert_eq!(after["returned_hosted_execution"], wire(&r));
    assert_eq!(after["status"], "idle");
    assert_ne!(after["last_heartbeat"], before["last_heartbeat"]);
    let fresh: bool = sqlx::query_scalar(
        "SELECT last_heartbeat > NOW() - INTERVAL '1 minute' FROM sessions WHERE id = $1",
    )
    .bind(observed.session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(fresh);
    let alias: i64 = sqlx::query_scalar(
        "SELECT session_id FROM session_key_aliases WHERE session_key = 'legacy-return-alias'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(alias, observed.session_id);
    assert_eq!(
        return_legacy_pg(&pool, &observed, &r.owner, &r.execution_nonce, &ended(&r))
            .await
            .unwrap(),
        HostedCasOutcome::Stale
    );
    assert_eq!(snapshot(&pool, observed.session_id).await, after);
    let legacy = HostedObservation {
        raw: None,
        record: HostedRecord::Legacy,
        ..observed
    };
    let next = super::super::tests::pending(&r.owner, "n-new");
    assert_eq!(
        install_pending_pg(&pool, &legacy, next).await.unwrap(),
        HostedCasOutcome::Written
    );
    assert_eq!(
        snapshot(&pool, legacy.session_id).await["returned_hosted_execution"],
        wire(&r)
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn legacy_return_rejects_changed_inputs_and_live_unknown_records_pg() {
    let (db, pool, observed, r) = setup().await;
    let proof = ended(&r);
    let before = snapshot(&pool, observed.session_id).await;
    assert!(matches!(
        provider_alive(&r),
        Err(crate::services::session_host::legacy_return::ReturnEndRefusal::ProviderUnverified(_))
    ));
    assert_eq!(snapshot(&pool, observed.session_id).await, before);
    let mut other = r.owner.clone();
    other.logical_key = "other".into();
    assert_eq!(
        return_legacy_pg(&pool, &observed, &other, &r.execution_nonce, &proof)
            .await
            .unwrap_err(),
        HostedTransitionError::OwnerMismatch
    );
    assert_eq!(
        return_legacy_pg(&pool, &observed, &r.owner, "new-nonce", &proof)
            .await
            .unwrap_err(),
        HostedTransitionError::NonceMismatch
    );
    let mut other_record = r.clone();
    other_record.source_ref.baseline_event_seq = Some(7);
    assert_eq!(
        return_legacy_pg(
            &pool,
            &observed,
            &r.owner,
            &r.execution_nonce,
            &ended(&other_record)
        )
        .await
        .unwrap_err(),
        HostedTransitionError::ExpectedOverwrite
    );
    for raw in [
        wire(&record(&r.owner, "n-old", HostedState::Pending)),
        wire(&record(&r.owner, "n-old", HostedState::Bound)),
        json!({"schema": 99}),
    ] {
        let obs = HostedObservation {
            raw: Some(raw.clone()),
            record: HostedRecord::decode(Some(&raw)),
            ..observed.clone()
        };
        assert!(
            return_legacy_pg(&pool, &obs, &r.owner, &r.execution_nonce, &proof)
                .await
                .is_err()
        );
    }
    assert_eq!(snapshot(&pool, observed.session_id).await, before);
    // Every stale DB mutation must leave all columns, including the archive and heartbeat, unchanged.
    for assignment in [
        "hosted_execution = jsonb_set(hosted_execution, '{source_ref,baseline_event_seq}', '42')",
        "hosted_execution = jsonb_set(hosted_execution, '{execution_nonce}', '\"new\"')",
        "hosted_execution = jsonb_set(hosted_execution, '{owner,logical_key}', '\"new\"')",
        "discord_token_hash = 'other-owner'",
        "status = 'idle', active_dispatch_id = 'unsettled'",
        "returned_hosted_execution = '{\"occupied\":true}'",
    ] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("UPDATE sessions SET {assignment} WHERE id = $1"))
            .bind(observed.session_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let changed = snapshot(&pool, observed.session_id).await;
        assert_eq!(
            return_legacy_pg(&pool, &observed, &r.owner, &r.execution_nonce, &proof)
                .await
                .unwrap(),
            HostedCasOutcome::Stale,
            "{assignment}"
        );
        assert_eq!(snapshot(&pool, observed.session_id).await, changed);
        sqlx::query("UPDATE sessions SET hosted_execution = $2, discord_token_hash = $3, active_dispatch_id = NULL, returned_hosted_execution = NULL WHERE id = $1")
            .bind(observed.session_id).bind(wire(&r)).bind(&r.owner.discord_token_hash).execute(&pool).await.unwrap();
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn legacy_return_cas_waits_for_concurrent_incarnation_pg() {
    let (db, pool, observed, r) = setup().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE id = $1")
        .bind(observed.session_id)
        .bind(wire(&super::super::tests::pending(&r.owner, "concurrent")))
        .execute(&mut *tx)
        .await
        .unwrap();
    let worker_pool = pool.clone();
    let worker_obs = observed.clone();
    let worker_r = r.clone();
    let worker = tokio::spawn(async move {
        return_legacy_pg(
            &worker_pool,
            &worker_obs,
            &worker_r.owner,
            &worker_r.execution_nonce,
            &ended(&worker_r),
        )
        .await
    });
    // Wait for PostgreSQL to report the blocked UPDATE, rather than relying on sleep timing.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE 'UPDATE sessions%')")
                .fetch_one(&pool).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    tx.commit().await.unwrap();
    let changed = snapshot(&pool, observed.session_id).await;
    assert_eq!(worker.await.unwrap().unwrap(), HostedCasOutcome::Stale);
    assert_eq!(snapshot(&pool, observed.session_id).await, changed);
    pool.close().await;
    db.drop().await;
}
