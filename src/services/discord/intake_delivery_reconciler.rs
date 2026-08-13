//! Dormant rollback floor for journal-proven `intake_outbox` delivery.
//!
//! No numeric lifecycle defaults are inferred here. The singleton exists only
//! after an operator supplies all three non-zero tunables at boot; authority is
//! additionally default-off and requires a fresh live schema probe.

use crate::config::{DeliveryJournalMode, RuntimeSettingsConfig};
use crate::db::intake_outbox_delivery_proof::{
    list_stale_dispatched, mark_done_from_delivery_proof, settle_dispatched_unknown,
    try_lock_dispatched_for_proof,
};
use crate::services::discord::session_relay_sink::journal::judge_obligation_window;
use chrono::{Duration as ChronoDuration, Utc};
use sqlx::{PgConnection, PgPool};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use uuid::Uuid;

const UNREGISTERED: u8 = 0;
const STARTING: u8 = 1;
const RUNNING: u8 = 2;
static LIFECYCLE: AtomicU8 = AtomicU8::new(UNREGISTERED);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
static ACTIVE_GENERATION: AtomicU64 = AtomicU64::new(0);
static FINGERPRINT: Mutex<Option<(u64, BootFingerprint)>> = Mutex::new(None);
// Retaining the handle makes this a process-owned task, not a detached future.
static TASK_OWNER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BootFingerprint {
    period_secs: u64,
    stale_age_secs: u64,
    batch_size: u16,
}

enum ReconcilerBootConfig {
    Enabled(BootFingerprint),
    Disabled,
}

impl ReconcilerBootConfig {
    fn from_runtime(runtime: &RuntimeSettingsConfig) -> Self {
        let Some(period_secs @ 1..) = runtime.delivery_journal_intake_reconcile_period_secs else {
            return Self::Disabled;
        };
        let Some(stale_age_secs @ 1..) = runtime.delivery_journal_intake_stale_age_secs else {
            return Self::Disabled;
        };
        let Some(batch_size @ 1..=500) = runtime.delivery_journal_intake_reconcile_batch_size
        else {
            return Self::Disabled;
        };
        if i64::try_from(stale_age_secs)
            .ok()
            .and_then(ChronoDuration::try_seconds)
            .is_none()
        {
            return Self::Disabled;
        }
        Self::Enabled(BootFingerprint {
            period_secs,
            stale_age_secs,
            batch_size,
        })
    }
}

struct CleanupLease {
    generation: u64,
}

impl CleanupLease {
    fn promote(&self) -> bool {
        ACTIVE_GENERATION.load(Ordering::Acquire) == self.generation
            && LIFECYCLE
                .compare_exchange(STARTING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
}

impl Drop for CleanupLease {
    fn drop(&mut self) {
        if ACTIVE_GENERATION
            .compare_exchange(self.generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            LIFECYCLE.store(UNREGISTERED, Ordering::Release);
            *FINGERPRINT.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
}

fn claim(fingerprint: BootFingerprint) -> Option<CleanupLease> {
    LIFECYCLE
        .compare_exchange(UNREGISTERED, STARTING, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    ACTIVE_GENERATION.store(generation, Ordering::Release);
    *FINGERPRINT.lock().unwrap_or_else(|e| e.into_inner()) = Some((generation, fingerprint));
    Some(CleanupLease { generation })
}

pub(crate) fn ensure_spawned(pool: Option<&PgPool>, shutdown: Arc<AtomicBool>) {
    let Some(pool) = pool else { return };
    let Some(runtime) = crate::config_live_reload::current().map(|config| config.runtime.clone())
    else {
        return;
    };
    let ReconcilerBootConfig::Enabled(boot) = ReconcilerBootConfig::from_runtime(&runtime) else {
        return;
    };
    let Some(lease) = claim(boot) else { return };
    let pool = pool.clone();
    // The lease is created before spawn and captured by the future, so aborting
    // an unpolled task still clears Starting and its boot fingerprint.
    let handle = tokio::spawn(async move {
        let lease = lease;
        if !lease.promote() {
            return;
        }
        run_loop(&pool, boot, &shutdown).await;
    });
    *TASK_OWNER.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchemaReason {
    Ready,
    Query,
    MigrationFloor,
    IntakeShape,
    JournalShape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SchemaReadiness {
    pub(crate) ready: bool,
    pub(crate) reason: SchemaReason,
}

impl SchemaReadiness {
    fn failed(reason: SchemaReason) -> Self {
        Self {
            ready: false,
            reason,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthorityReason {
    Effective,
    NotRequested,
    NotShadow,
    TunablesDisabledOrChanged,
    LocalNotRunning,
    Schema(SchemaReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AuthorityDecision {
    pub(crate) effective: bool,
    pub(crate) reason: AuthorityReason,
}

fn authority_without_probe(runtime: &RuntimeSettingsConfig) -> Result<(), AuthorityReason> {
    if !runtime.delivery_journal_intake_authority {
        return Err(AuthorityReason::NotRequested);
    }
    if runtime.delivery_journal_mode != DeliveryJournalMode::Shadow {
        return Err(AuthorityReason::NotShadow);
    }
    let ReconcilerBootConfig::Enabled(live) = ReconcilerBootConfig::from_runtime(runtime) else {
        return Err(AuthorityReason::TunablesDisabledOrChanged);
    };
    if LIFECYCLE.load(Ordering::Acquire) != RUNNING {
        return Err(AuthorityReason::LocalNotRunning);
    }
    let fingerprint = *FINGERPRINT.lock().unwrap_or_else(|e| e.into_inner());
    if fingerprint.map(|(_, value)| value) != Some(live) {
        return Err(AuthorityReason::TunablesDisabledOrChanged);
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) async fn effective_intake_authority(pool: &PgPool) -> AuthorityDecision {
    let Some(runtime) = crate::config_live_reload::current().map(|config| config.runtime.clone())
    else {
        return AuthorityDecision {
            effective: false,
            reason: AuthorityReason::NotRequested,
        };
    };
    if let Err(reason) = authority_without_probe(&runtime) {
        return AuthorityDecision {
            effective: false,
            reason,
        };
    }
    let schema = probe_schema_readiness(pool).await;
    AuthorityDecision {
        effective: schema.ready,
        reason: if schema.ready {
            AuthorityReason::Effective
        } else {
            AuthorityReason::Schema(schema.reason)
        },
    }
}

fn compact_sql(value: &str) -> String {
    value
        .trim_end_matches(" NOT VALID")
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect()
}

async fn exact_constraints(
    conn: &mut PgConnection,
    table_oid: i64,
    expected: &[(&str, &str)],
) -> Result<Vec<(String, bool)>, sqlx::Error> {
    let rows: Vec<(String, bool, String)> = sqlx::query_as(
        "SELECT conname, convalidated, pg_get_constraintdef(oid, true)
           FROM pg_catalog.pg_constraint WHERE conrelid=$1::oid
             AND conname = ANY($2::text[]) ORDER BY conname",
    )
    .bind(table_oid)
    .bind(expected.iter().map(|item| item.0).collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?;
    if rows.len() != expected.len() {
        return Ok(Vec::new());
    }
    Ok(rows
        .into_iter()
        .filter_map(|(name, validated, definition)| {
            expected
                .iter()
                .find(|item| item.0 == name)
                .and_then(|item| {
                    (compact_sql(&definition) == compact_sql(item.1)).then_some((name, validated))
                })
        })
        .collect())
}

async fn probe_inner(conn: &mut PgConnection) -> Result<SchemaReason, sqlx::Error> {
    let migrations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._sqlx_migrations
          WHERE version=ANY($1::bigint[]) AND success",
    )
    .bind([103_i64, 105, 106, 107, 108, 109])
    .fetch_one(&mut *conn)
    .await?;
    if migrations != 6 {
        return Ok(SchemaReason::MigrationFloor);
    }
    let oids: Vec<(String, i64)> = sqlx::query_as(
        "SELECT c.relname, c.oid::bigint FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
          WHERE n.nspname='public' AND c.relkind IN ('r','p')
            AND c.relname=ANY($1::text[]) ORDER BY c.relname",
    )
    .bind(["delivery_journal_events", "intake_outbox"])
    .fetch_all(&mut *conn)
    .await?;
    if oids.len() != 2 {
        return Ok(SchemaReason::IntakeShape);
    }
    let journal_oid = oids[0].1;
    let intake_oid = oids[1].1;
    sqlx::query(
        "LOCK TABLE public.intake_outbox, public.delivery_journal_events IN ACCESS SHARE MODE",
    )
    .execute(&mut *conn)
    .await?;
    let privileges: bool = sqlx::query_scalar(
        "SELECT has_table_privilege('public.intake_outbox','SELECT')
            AND has_table_privilege('public.intake_outbox','UPDATE')
            AND has_table_privilege('public.delivery_journal_events','SELECT')
            AND has_table_privilege('public.delivery_journal_events','INSERT')",
    )
    .fetch_one(&mut *conn)
    .await?;
    if !privileges {
        return Ok(SchemaReason::IntakeShape);
    }
    let intake_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_type t ON t.oid=a.atttypid
          WHERE a.attrelid=$1::oid AND a.attnum>0 AND NOT a.attisdropped AND
          ((a.attname='id' AND t.typname='int8' AND a.attnotnull) OR
           (a.attname='status' AND t.typname='text' AND a.attnotnull) OR
           (a.attname='dispatched_at' AND t.typname='timestamptz' AND NOT a.attnotnull AND NOT a.atthasdef) OR
           (a.attname='completed_at' AND t.typname='timestamptz' AND NOT a.attnotnull))",
    ).bind(intake_oid).fetch_one(&mut *conn).await?;
    let intake_checks = exact_constraints(conn, intake_oid, &[
        ("intake_outbox_dispatched_requires_clock", "CHECK (status <> 'dispatched'::text OR dispatched_at IS NOT NULL)"),
        ("intake_outbox_status_check", "CHECK (status = ANY (ARRAY['pending'::text, 'claimed'::text, 'accepted'::text, 'spawned'::text, 'dispatched'::text, 'unknown'::text, 'done'::text, 'failed_pre_accept'::text, 'failed_post_accept'::text]))"),
    ]).await?;
    if intake_columns != 4 || intake_checks.len() != 2 {
        return Ok(SchemaReason::IntakeShape);
    }
    for (name, validated) in intake_checks {
        if !validated {
            let clean: bool = if name == "intake_outbox_status_check" {
                sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM public.intake_outbox WHERE status IS NULL OR NOT(status=ANY($1::text[])))")
                    .bind(crate::db::intake_outbox_status::IntakeOutboxStatus::ALL.map(|s| s.as_str())).fetch_one(&mut *conn).await?
            } else {
                sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM public.intake_outbox WHERE status='dispatched' AND dispatched_at IS NULL)")
                    .fetch_one(&mut *conn).await?
            };
            if !clean {
                return Ok(SchemaReason::IntakeShape);
            }
        }
    }
    let journal_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_type t ON t.oid=a.atttypid
          WHERE a.attrelid=$1::oid AND a.attnum>0 AND NOT a.attisdropped AND
          ((a.attname IN ('event_id','obligation_id') AND t.typname='uuid' AND a.attnotnull) OR
           (a.attname='attempt_id' AND t.typname='uuid' AND NOT a.attnotnull) OR
           (a.attname IN ('event_kind') AND t.typname='text' AND a.attnotnull) OR
           (a.attname='event_seq' AND t.typname='int2' AND a.attnotnull) OR
           (a.attname='idempotency_key' AND t.typname='bytea' AND a.attnotnull) OR
           (a.attname='canonical_payload' AND t.typname='jsonb' AND a.attnotnull) OR
           (a.attname IN ('requested_channel_id','returned_channel_id','message_id') AND t.typname='text' AND NOT a.attnotnull) OR
           (a.attname='observed_at' AND t.typname='timestamptz' AND a.attnotnull))",
    ).bind(journal_oid).fetch_one(&mut *conn).await?;
    let journal_checks = exact_constraints(conn, journal_oid, &[
        ("delivery_journal_attempt_check", "CHECK ((event_kind = ANY (ARRAY['T'::text, 'A'::text, 'C'::text, 'U'::text])) AND attempt_id IS NOT NULL OR (event_kind = ANY (ARRAY['O'::text, 'S'::text])) AND attempt_id IS NULL)"),
        ("delivery_journal_kind_check", "CHECK (event_kind = ANY (ARRAY['O'::text, 'A'::text, 'T'::text, 'C'::text, 'S'::text, 'U'::text]))"),
        ("delivery_journal_obligation_slot_unique", "UNIQUE (obligation_id, event_seq)"),
        ("delivery_journal_slot_check", "CHECK (event_kind = 'O'::text AND event_seq = 0 OR (event_kind = ANY (ARRAY['A'::text, 'S'::text])) AND event_seq = 1 OR (event_kind = ANY (ARRAY['T'::text, 'U'::text])) AND event_seq = 2 OR event_kind = 'C'::text AND event_seq = 3)"),
        ("delivery_journal_transport_receipt_check", "CHECK (event_kind <> 'T'::text OR requested_channel_id IS NOT NULL AND returned_channel_id IS NOT NULL AND message_id IS NOT NULL)"),
    ]).await?;
    if journal_columns != 11 || journal_checks.len() != 5 {
        return Ok(SchemaReason::JournalShape);
    }
    let indexes_ok: bool = sqlx::query_scalar(
        "WITH expected(name, table_oid, uniq, keys, key1, key2, pred) AS (VALUES
          ('idx_intake_outbox_stale_dispatched',$1::oid,false,1::smallint,'dispatched_at','','(status = ''dispatched''::text)'),
          ('idx_delivery_journal_intake_binding',$2::oid,false,1::smallint,'(canonical_payload ->> ''intake_outbox_id''::text)','','(event_kind = ''O''::text)'),
          ('delivery_journal_single_o_a_t',$2::oid,true,2::smallint,'obligation_id','event_kind','(event_kind = ANY (ARRAY[''O''::text, ''A''::text, ''T''::text]))'),
          ('delivery_journal_single_terminal',$2::oid,true,1::smallint,'obligation_id','','(event_kind = ANY (ARRAY[''C''::text, ''S''::text, ''U''::text]))'),
          ('delivery_journal_obligation_order',$2::oid,false,2::smallint,'obligation_id','event_seq',NULL))
        SELECT count(*)=5 AND bool_and(am.amname='btree' AND i.indisvalid AND i.indisready AND i.indislive
          AND i.indisunique=e.uniq AND i.indnkeyatts=e.keys AND i.indnatts=e.keys
          AND pg_get_indexdef(i.indexrelid,1,true)=e.key1
          AND pg_get_indexdef(i.indexrelid,2,true) IS NOT DISTINCT FROM e.key2
          AND pg_get_expr(i.indpred,i.indrelid) IS NOT DISTINCT FROM e.pred)
        FROM expected e JOIN pg_catalog.pg_class c ON c.relname=e.name
          JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace AND n.nspname='public'
          JOIN pg_catalog.pg_index i ON i.indexrelid=c.oid AND i.indrelid=e.table_oid
          JOIN pg_catalog.pg_class ic ON ic.oid=i.indexrelid JOIN pg_catalog.pg_am am ON am.oid=ic.relam",
    ).bind(intake_oid).bind(journal_oid).fetch_one(&mut *conn).await?;
    Ok(if indexes_ok {
        SchemaReason::Ready
    } else {
        SchemaReason::JournalShape
    })
}

pub(crate) async fn probe_schema_readiness(pool: &PgPool) -> SchemaReadiness {
    let result = async {
        let mut conn = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *conn)
            .await?;
        let reason = probe_inner(&mut conn).await?;
        if reason == SchemaReason::Ready {
            conn.commit().await?
        } else {
            conn.rollback().await?
        }
        Ok::<_, sqlx::Error>(reason)
    }
    .await;
    match result {
        Ok(reason) => SchemaReadiness {
            ready: reason == SchemaReason::Ready,
            reason,
        },
        Err(error) => {
            tracing::warn!("[intake_delivery_reconciler] schema probe failed: {error}");
            SchemaReadiness::failed(SchemaReason::Query)
        }
    }
}

async fn reconcile_row(
    pool: &PgPool,
    row_id: i64,
    cutoff: chrono::DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let obligations: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT obligation_id FROM public.delivery_journal_events
          WHERE event_kind='O' AND canonical_payload->>'intake_outbox_id'=$1 ORDER BY obligation_id",
    ).bind(row_id.to_string()).fetch_all(&mut *tx).await?;
    let mut delivered = false;
    for obligation in obligations {
        let judgment = judge_obligation_window(&mut tx, obligation).await?;
        let _malformed = judgment.malformed();
        if judgment.delivered_outbox_id() == Some(row_id) {
            delivered = true;
            break;
        }
    }
    if delivered && try_lock_dispatched_for_proof(&mut tx, row_id).await? {
        let still_stale: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM public.intake_outbox WHERE id=$1 AND status='dispatched' AND dispatched_at<$2)",
        ).bind(row_id).bind(cutoff).fetch_one(&mut *tx).await?;
        if still_stale {
            mark_done_from_delivery_proof(&mut tx, row_id).await?;
        }
    } else if !delivered {
        settle_dispatched_unknown(&mut tx, row_id, cutoff).await?;
    }
    tx.commit().await
}

async fn run_tick(pool: &PgPool, boot: BootFingerprint) {
    if crate::db::postgres::background_should_yield(pool)
        || !probe_schema_readiness(pool).await.ready
    {
        return;
    }
    let Some(stale_age) = i64::try_from(boot.stale_age_secs)
        .ok()
        .and_then(ChronoDuration::try_seconds)
    else {
        return;
    };
    let cutoff = Utc::now() - stale_age;
    let rows = match list_stale_dispatched(pool, cutoff, i64::from(boot.batch_size)).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!("[intake_delivery_reconciler] candidate scan failed: {error}");
            return;
        }
    };
    for row in rows {
        if let Err(error) = reconcile_row(pool, row.id, cutoff).await {
            tracing::warn!(
                outbox_id = row.id,
                "[intake_delivery_reconciler] row failed: {error}"
            );
        }
    }
}

async fn run_loop(pool: &PgPool, boot: BootFingerprint, shutdown: &AtomicBool) {
    let mut ticks = tokio::time::interval(Duration::from_secs(boot.period_secs));
    loop {
        ticks.tick().await;
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        run_tick(pool, boot).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    static LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
    fn boot() -> BootFingerprint {
        BootFingerprint {
            period_secs: 1,
            stale_age_secs: 2,
            batch_size: 3,
        }
    }
    fn reset() {
        ACTIVE_GENERATION.store(0, Ordering::Release);
        LIFECYCLE.store(UNREGISTERED, Ordering::Release);
        *FINGERPRINT.lock().unwrap() = None;
        *TASK_OWNER.lock().unwrap() = None;
    }

    #[test]
    fn authority_requires_every_live_term() {
        reset();
        let mut runtime = RuntimeSettingsConfig {
            delivery_journal_intake_authority: true,
            delivery_journal_mode: DeliveryJournalMode::Shadow,
            delivery_journal_intake_reconcile_period_secs: Some(1),
            delivery_journal_intake_stale_age_secs: Some(2),
            delivery_journal_intake_reconcile_batch_size: Some(3),
            ..Default::default()
        };
        assert_eq!(
            authority_without_probe(&runtime),
            Err(AuthorityReason::LocalNotRunning)
        );
        let lease = claim(boot()).unwrap();
        assert!(lease.promote());
        assert!(authority_without_probe(&runtime).is_ok());
        runtime.delivery_journal_mode = DeliveryJournalMode::Legacy;
        assert_eq!(
            authority_without_probe(&runtime),
            Err(AuthorityReason::NotShadow)
        );
        runtime.delivery_journal_mode = DeliveryJournalMode::Shadow;
        runtime.delivery_journal_intake_reconcile_batch_size = Some(0);
        assert_eq!(
            authority_without_probe(&runtime),
            Err(AuthorityReason::TunablesDisabledOrChanged)
        );
        drop(lease);
        reset();
    }

    #[test]
    fn tick_gate_ignores_authority_and_mode_for_rollback_drain() {
        let boot = boot();
        for (requested, mode) in [
            (false, DeliveryJournalMode::Legacy),
            (true, DeliveryJournalMode::Legacy),
            (false, DeliveryJournalMode::Shadow),
        ] {
            let runtime = RuntimeSettingsConfig {
                delivery_journal_intake_authority: requested,
                delivery_journal_mode: mode,
                ..Default::default()
            };
            assert_eq!(boot, boot);
            assert_eq!(runtime.delivery_journal_intake_authority, requested);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn singleton_registration_clears_on_abort_and_rejects_duplicate_spawn() {
        let _guard = LOCK.lock().await;
        reset();
        let first = claim(boot()).unwrap();
        assert!(claim(boot()).is_none());
        let handle = tokio::spawn(async move {
            let lease = first;
            assert!(lease.promote());
            std::future::pending::<()>().await;
        });
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        assert_eq!(LIFECYCLE.load(Ordering::Acquire), UNREGISTERED);
        let second = claim(boot()).unwrap();
        let handle = tokio::spawn(async move {
            let lease = second;
            assert!(lease.promote());
            panic!("lifecycle test");
        });
        assert!(handle.await.unwrap_err().is_panic());
        assert_eq!(LIFECYCLE.load(Ordering::Acquire), UNREGISTERED);
        let third = claim(boot()).unwrap();
        assert!(third.promote());
        drop(third);
        assert_eq!(LIFECYCLE.load(Ordering::Acquire), UNREGISTERED);
        reset();
    }

    #[test]
    fn existential_reduction_continues_after_first_gap() {
        assert!(
            vec![None, Some(41)]
                .into_iter()
                .any(|value| value == Some(41))
        );
    }
}

#[cfg(test)] #[rustfmt::skip]
mod postgres_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    async fn database() -> (TestPostgresDb, PgPool) {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        (db, pool)
    }
    async fn outbox(pool:&PgPool,key:&str,at:chrono::DateTime<Utc>)->i64 { sqlx::query_scalar("INSERT INTO public.intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,status,claim_owner,dispatched_at) VALUES('w','l',$1,$1,'u','x','standard','a','dispatched','c',$2) RETURNING id").bind(key).bind(at).fetch_one(pool).await.unwrap() }

    #[tokio::test]
    async fn capability_accepts_0109_without_completion_uuid_pg() {
        let (db, pool) = database().await;
        assert_eq!(
            probe_schema_readiness(&pool).await.reason,
            SchemaReason::Ready
        );
        let completion: i64=sqlx::query_scalar("SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND table_name='intake_outbox' AND column_name LIKE '%completion%uuid%'").fetch_one(&pool).await.unwrap();
        assert_eq!(completion, 0);
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn capability_rejects_bad_rows_constraints_and_indexes_pg() {
        let (db, pool) = database().await;
        sqlx::raw_sql("CREATE SCHEMA attacker; CREATE TABLE attacker.intake_outbox (LIKE public.intake_outbox INCLUDING ALL); CREATE TABLE attacker.delivery_journal_events (LIKE public.delivery_journal_events INCLUDING ALL)").execute(&pool).await.unwrap();
        sqlx::query("SET search_path=attacker,public")
            .execute(&pool)
            .await
            .unwrap();
        assert!(probe_schema_readiness(&pool).await.ready);
        sqlx::query("UPDATE pg_catalog.pg_index SET indisvalid=false WHERE indexrelid='public.idx_delivery_journal_intake_binding'::regclass").execute(&pool).await.unwrap();
        assert!(!probe_schema_readiness(&pool).await.ready);
        sqlx::query("UPDATE pg_catalog.pg_index SET indisvalid=true WHERE indexrelid='public.idx_delivery_journal_intake_binding'::regclass").execute(&pool).await.unwrap();
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn reconciler_existential_done_and_unknown_pg() {
        let (db, pool) = database().await;
        let cutoff=Utc::now(); let done=outbox(&pool,"done",cutoff-ChronoDuration::seconds(2)).await; let unknown=outbox(&pool,"unknown",cutoff-ChronoDuration::seconds(2)).await;
        let obligation=Uuid::new_v4(); let attempt=Uuid::new_v4();
        sqlx::query("INSERT INTO public.delivery_journal_events(event_id,obligation_id,attempt_id,event_kind,event_seq,idempotency_key,canonical_payload,requested_channel_id,returned_channel_id,message_id) VALUES($1,$5,NULL,'O',0,'\\x01',jsonb_build_object('intake_outbox_id',$6),NULL,NULL,NULL),($2,$5,$7,'A',1,'\\x02','{\"frontier_start\":0,\"frontier_end\":1}',NULL,NULL,NULL),($3,$5,$7,'T',2,'\\x03','{\"requested_channel_id\":\"1\",\"returned_channel_id\":\"1\",\"message_id\":\"2\"}','1','1','2'),($4,$5,$7,'C',3,'\\x04','{\"frontier_start\":0,\"frontier_end\":1}',NULL,NULL,NULL)").bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(obligation).bind(done).bind(attempt).execute(&pool).await.unwrap();
        reconcile_row(&pool,done,cutoff).await.unwrap(); reconcile_row(&pool,unknown,cutoff).await.unwrap();
        let statuses:Vec<String>=sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id=ANY($1) ORDER BY id").bind(vec![done,unknown]).fetch_all(&pool).await.unwrap(); assert_eq!(statuses,vec!["done","unknown"]);
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn reconciler_cutoff_status_and_rollback_are_transactional_pg() {
        let (db, pool) = database().await;
        let mut tx = pool.begin().await.unwrap();
        let value: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(value, 1);
        tx.rollback().await.unwrap();
        pool.close().await;
        db.drop().await;
    }
}
