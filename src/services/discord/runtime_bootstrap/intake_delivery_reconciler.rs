//! Dormant reducer for journal-proven intake delivery settlement.
//!
//! B3 owns scheduling and boot wiring. This module only exposes a per-row
//! transaction boundary and cannot run until that later slice calls it.

#![allow(dead_code)]

use crate::config::DeliveryJournalMode;
use crate::db::intake_outbox_delivery_proof::{
    mark_done_from_delivery_proof, settle_dispatched_unknown, try_lock_dispatched_for_proof,
};
use crate::services::discord::session_relay_sink::journal::{
    judge_obligation_window, read_authority_obligation_window, select_reconcile_judgment,
};
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReconcileOutcome {
    Done,
    Unknown,
    Unchanged,
}

pub(super) async fn reconcile_row(
    pool: &PgPool,
    outbox_id: i64,
    cutoff: DateTime<Utc>,
    heartbeat_fresh: DateTime<Utc>,
) -> Result<ReconcileOutcome, sqlx::Error> {
    let mut transaction = super::intake_delivery_sweep::begin_settlement_transaction(pool).await?;
    let outcome = reconcile_in_tx(&mut transaction, outbox_id, cutoff, heartbeat_fresh).await?;
    transaction.commit().await?;
    Ok(outcome)
}

async fn reconcile_in_tx(
    connection: &mut PgConnection,
    outbox_id: i64,
    cutoff: DateTime<Utc>,
    heartbeat_fresh: DateTime<Utc>,
) -> Result<ReconcileOutcome, sqlx::Error> {
    // Capture the mode once so a YAML reload cannot mix readers in this transaction.
    let journal_mode = crate::config_live_reload::current()
        .map(|config| config.runtime.delivery_journal_mode)
        .unwrap_or(DeliveryJournalMode::Legacy);
    let obligations: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT obligation_id
           FROM public.delivery_journal_events
          WHERE event_kind = 'O'
            AND canonical_payload ->> 'intake_outbox_id' = $1
          ORDER BY obligation_id",
    )
    .bind(outbox_id.to_string())
    .fetch_all(&mut *connection)
    .await?;

    let mut delivered = false;
    for obligation_id in obligations {
        // Authority selects the journal read path; Legacy and Shadow retain the
        // reducer's pre-handoff facade until a later writer cutover.
        let authority_judgment =
            read_authority_obligation_window(&mut *connection, obligation_id, journal_mode).await?;
        let fallback_judgment = if authority_judgment.is_none() {
            Some(judge_obligation_window(&mut *connection, obligation_id).await?)
        } else {
            None
        };
        let judgment =
            select_reconcile_judgment(journal_mode, authority_judgment, fallback_judgment)
                .unwrap_or_else(|| {
                    // The caller's `authority.is_none()` guard makes the fallback present exactly when
                    // the authority result is absent. That invariant is non-local to this selector and
                    // must remain true while this code runs inside the open PG transaction; violating
                    // the caller contract is unreachable rather than a recoverable judgment.
                    unreachable!("one journal reader must produce a judgment")
                });
        if judgment.delivered_outbox_id() == Some(outbox_id) {
            delivered = true;
            break;
        }
    }

    if !delivered {
        return Ok(
            if settle_dispatched_unknown(connection, outbox_id, cutoff, heartbeat_fresh).await? {
                ReconcileOutcome::Unknown
            } else {
                ReconcileOutcome::Unchanged
            },
        );
    }
    if !try_lock_dispatched_for_proof(connection, outbox_id).await? {
        return Ok(ReconcileOutcome::Unchanged);
    }
    let stale: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM public.intake_outbox
              WHERE id = $1 AND status = $3 AND dispatched_at < $2)",
    )
    .bind(outbox_id)
    .bind(cutoff)
    .bind(crate::db::intake_outbox_status::IntakeOutboxStatus::Dispatched)
    .fetch_one(&mut *connection)
    .await?;
    if !stale {
        return Ok(ReconcileOutcome::Unchanged);
    }
    Ok(
        if mark_done_from_delivery_proof(connection, outbox_id).await? {
            ReconcileOutcome::Done
        } else {
            ReconcileOutcome::Unchanged
        },
    )
}

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::intake_outbox_delivery_proof::list_stale_dispatched;
    use chrono::Duration;
    use tokio::time::{Duration as TokioDuration, timeout};

    type Audit = (String, Option<DateTime<Utc>>, String, DateTime<Utc>);

    fn pg_time(value: DateTime<Utc>) -> DateTime<Utc> {
        DateTime::from_timestamp_micros(value.timestamp_micros()).expect("valid PG timestamp")
    }

    async fn seed_outbox(pool: &PgPool, key: &str, dispatched_at: DateTime<Utc>) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO public.intake_outbox(
               target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,
               request_owner_id,user_text,turn_kind,agent_id,status,claim_owner,dispatched_at)
             VALUES('worker','leader',$1,$1,'user','hello','standard','agent',
                    'dispatched','dispatch-worker',$2)
             RETURNING id",
        )
        .bind(key)
        .bind(dispatched_at)
        .fetch_one(pool)
        .await
        .expect("seed dispatched outbox")
    }

    async fn seed_gap(pool: &PgPool, outbox_id: i64, obligation_id: Uuid) {
        let event_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO public.delivery_journal_events(
               event_id,obligation_id,event_kind,event_seq,idempotency_key,canonical_payload)
             VALUES($1,$2,'O',0,uuid_send($1),jsonb_build_object('intake_outbox_id',$3))",
        )
        .bind(event_id)
        .bind(obligation_id)
        .bind(outbox_id)
        .execute(pool)
        .await
        .expect("seed incomplete obligation");
    }

    async fn seed_delivered(pool: &PgPool, outbox_id: i64, obligation_id: Uuid) {
        let attempt = Uuid::new_v4();
        let ids = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        sqlx::query(
            "INSERT INTO public.delivery_journal_events(
               event_id,obligation_id,attempt_id,event_kind,event_seq,idempotency_key,
               canonical_payload,requested_channel_id,returned_channel_id,message_id)
             VALUES
               ($1,$5,NULL,'O',0,uuid_send($1),jsonb_build_object('intake_outbox_id',$6),NULL,NULL,NULL),
               ($2,$5,$7,'A',1,uuid_send($2),'{\"frontier_start\":0,\"frontier_end\":1}',NULL,NULL,NULL),
               ($3,$5,$7,'T',2,uuid_send($3),'{\"requested_channel_id\":\"1\",\"returned_channel_id\":\"1\",\"message_id\":\"2\"}','1','1','2'),
               ($4,$5,$7,'C',3,uuid_send($4),'{\"frontier_start\":0,\"frontier_end\":1}',NULL,NULL,NULL)",
        )
        .bind(ids[0])
        .bind(ids[1])
        .bind(ids[2])
        .bind(ids[3])
        .bind(obligation_id)
        .bind(outbox_id)
        .bind(attempt)
        .execute(pool)
        .await
        .expect("seed delivered obligation");
    }

    async fn audit(pool: &PgPool, outbox_id: i64) -> Audit {
        sqlx::query_as(
            "SELECT status,completed_at,claim_owner,dispatched_at
               FROM public.intake_outbox WHERE id=$1",
        )
        .bind(outbox_id)
        .fetch_one(pool)
        .await
        .expect("read outbox audit")
    }

    #[tokio::test]
    async fn journal_judgment_precedes_proof_lock_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let cutoff = pg_time(Utc::now());
        let row = seed_outbox(&pool, "judgment-before-lock", cutoff - Duration::minutes(1)).await;
        let mut blocker = pool.begin().await.expect("begin journal blocker");
        sqlx::query("LOCK TABLE public.delivery_journal_events IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .expect("block journal judgment");
        let mut reducer = pool.begin().await.expect("begin reducer transaction");
        let reducing = tokio::spawn(async move {
            let result = reconcile_in_tx(&mut reducer, row, cutoff, cutoff).await;
            reducer
                .rollback()
                .await
                .expect("rollback reducer transaction");
            result
        });
        timeout(TokioDuration::from_secs(5), async {
            while !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM pg_locks
                 WHERE database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=current_database())
                   AND relation='public.delivery_journal_events'::regclass AND NOT granted)",
            )
            .fetch_one(&mut *blocker)
            .await
            .expect("observe blocked journal read")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reducer reaches blocked journal judgment");
        let mut contender = pool.begin().await.expect("begin proof-lock contender");
        let won = try_lock_dispatched_for_proof(&mut contender, row)
            .await
            .expect("probe proof lock");
        assert!(won, "proof row is not locked before judgment");
        contender.rollback().await.expect("release proof lock");
        blocker.rollback().await.expect("release journal blocker");
        reducing
            .await
            .expect("join reducer")
            .expect("finish reducer");
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test]
    async fn reducer_is_existential_strict_transactional_and_public_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let cutoff = pg_time(Utc::now());
        let old = cutoff - Duration::minutes(2);

        let done = seed_outbox(&pool, "proof-done", old).await;
        seed_gap(&pool, done, Uuid::from_u128(1)).await;
        seed_delivered(&pool, done, Uuid::from_u128(2)).await;
        let unknown = seed_outbox(&pool, "proof-unknown", old).await;
        let equal = seed_outbox(&pool, "proof-equal", cutoff).await;
        seed_delivered(&pool, equal, Uuid::from_u128(3)).await;
        let refreshed = seed_outbox(&pool, "proof-refreshed", old).await;
        seed_delivered(&pool, refreshed, Uuid::from_u128(4)).await;

        let listed: Vec<_> = list_stale_dispatched(&pool, cutoff, 500)
            .await
            .expect("list stale candidates")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(listed.contains(&done));
        assert!(listed.contains(&unknown));
        assert!(listed.contains(&refreshed));
        assert!(!listed.contains(&equal));
        sqlx::query("UPDATE public.intake_outbox SET dispatched_at=$2 WHERE id=$1")
            .bind(refreshed)
            .bind(cutoff + Duration::seconds(1))
            .execute(&pool)
            .await
            .expect("refresh listed row");

        assert_eq!(
            reconcile_row(&pool, done, cutoff, cutoff).await.unwrap(),
            ReconcileOutcome::Done
        );
        assert_eq!(
            reconcile_row(&pool, unknown, cutoff, cutoff).await.unwrap(),
            ReconcileOutcome::Unknown
        );
        assert_eq!(
            reconcile_row(&pool, equal, cutoff, cutoff).await.unwrap(),
            ReconcileOutcome::Unchanged
        );
        assert_eq!(
            reconcile_row(&pool, refreshed, cutoff, cutoff)
                .await
                .unwrap(),
            ReconcileOutcome::Unchanged
        );
        for (id, status, at) in [
            (done, "done", old),
            (unknown, "unknown", old),
            (equal, "dispatched", cutoff),
            (refreshed, "dispatched", cutoff + Duration::seconds(1)),
        ] {
            let row = audit(&pool, id).await;
            assert_eq!(
                (row.0.as_str(), row.2.as_str(), row.3),
                (status, "dispatch-worker", at)
            );
            assert_eq!(row.1.is_some(), matches!(status, "done" | "unknown"));
        }

        let rollback = seed_outbox(&pool, "proof-rollback", old).await;
        seed_delivered(&pool, rollback, Uuid::from_u128(5)).await;
        sqlx::raw_sql(
            "CREATE FUNCTION public.reject_proof_done() RETURNS trigger LANGUAGE plpgsql AS $$
               BEGIN IF NEW.status='done' AND OLD.status='dispatched' THEN
                 RAISE EXCEPTION 'reject proof done'; END IF; RETURN NEW; END $$;
             CREATE TRIGGER reject_proof_done AFTER UPDATE ON public.intake_outbox
               FOR EACH ROW EXECUTE FUNCTION public.reject_proof_done()",
        )
        .execute(&pool)
        .await
        .expect("install rollback trigger");
        assert!(
            reconcile_row(&pool, rollback, cutoff, cutoff)
                .await
                .is_err()
        );
        sqlx::raw_sql(
            "DROP TRIGGER reject_proof_done ON public.intake_outbox;
             DROP FUNCTION public.reject_proof_done()",
        )
        .execute(&pool)
        .await
        .expect("remove rollback trigger");
        assert_eq!(
            audit(&pool, rollback).await,
            ("dispatched".into(), None, "dispatch-worker".into(), old)
        );

        let hostile = seed_outbox(&pool, "proof-hostile", old).await;
        seed_delivered(&pool, hostile, Uuid::from_u128(6)).await;
        let hostile_schema = format!("proof_attacker_{}", Uuid::new_v4().simple());
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {hostile_schema};
             CREATE TABLE {hostile_schema}.intake_outbox
               (LIKE public.intake_outbox INCLUDING ALL);
             CREATE TABLE {hostile_schema}.delivery_journal_events
               (LIKE public.delivery_journal_events INCLUDING ALL)"
        ))
        .execute(&pool)
        .await
        .expect("create hostile decoys");
        sqlx::query(&format!(
            "INSERT INTO {hostile_schema}.intake_outbox(
               id,target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,
               request_owner_id,user_text,turn_kind,agent_id,status,claim_owner,dispatched_at)
             VALUES($1,'worker','leader','decoy','decoy','user','hello','standard','agent',
                    'done','dispatch-worker',$2)"
        ))
        .bind(hostile)
        .bind(old)
        .execute(&pool)
        .await
        .expect("seed hostile decoy");
        let mut transaction = pool.begin().await.expect("begin hostile transaction");
        sqlx::query(&format!("SET LOCAL search_path={hostile_schema},public"))
            .execute(&mut *transaction)
            .await
            .expect("set hostile search path");
        assert_eq!(
            reconcile_in_tx(&mut transaction, hostile, cutoff, cutoff)
                .await
                .unwrap(),
            ReconcileOutcome::Done
        );
        let statuses: (String, String) = sqlx::query_as(&format!(
            "SELECT (SELECT status FROM public.intake_outbox WHERE id=$1),
                    (SELECT status FROM {hostile_schema}.intake_outbox WHERE id=$1)"
        ))
        .bind(hostile)
        .fetch_one(&mut *transaction)
        .await
        .expect("compare public and decoy rows");
        assert_eq!(statuses, ("done".into(), "done".into()));
        transaction
            .rollback()
            .await
            .expect("rollback hostile transaction");
        sqlx::query(&format!("DROP SCHEMA {hostile_schema} CASCADE"))
            .execute(&pool)
            .await
            .expect("remove hostile decoys");

        pool.close().await;
        database.drop().await;
    }
}
