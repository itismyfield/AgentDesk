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
use futures::FutureExt;
use sqlx::{PgConnection, PgPool};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const UNREGISTERED: u8 = 0;
const STARTING: u8 = 1;
const RUNNING: u8 = 2;
static LIFECYCLE: AtomicU8 = AtomicU8::new(UNREGISTERED);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
static ACTIVE_GENERATION: AtomicU64 = AtomicU64::new(0);
static FINGERPRINT: Mutex<Option<(u64, BootFingerprint)>> = Mutex::new(None);
// Retaining the handle makes this a process-owned task, not a detached future.
struct TaskOwner {
    handle: Option<JoinHandle<()>>,
    shutdown: CancellationToken,
}
impl Drop for TaskOwner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
    }
}
static TASK_OWNER: Mutex<Option<TaskOwner>> = Mutex::new(None);
#[cfg(test)]
static PANIC_AFTER_PROMOTE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
            .and_then(|age| Utc::now().checked_sub_signed(age))
            .is_none()
            || std::time::Instant::now()
                .checked_add(Duration::from_secs(period_secs))
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
        let mut fingerprint = FINGERPRINT.lock().unwrap_or_else(|e| e.into_inner());
        if ACTIVE_GENERATION
            .compare_exchange(self.generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if fingerprint.is_some_and(|(generation, _)| generation == self.generation) {
                *fingerprint = None;
            }
            LIFECYCLE.store(UNREGISTERED, Ordering::Release);
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

pub(crate) fn ensure_spawned(pool: Option<&PgPool>, runtime: &RuntimeSettingsConfig) {
    let Some(pool) = pool else { return };
    let ReconcilerBootConfig::Enabled(boot) = ReconcilerBootConfig::from_runtime(runtime) else {
        return;
    };
    let Some(lease) = claim(boot) else { return };
    let pool = pool.clone();
    // The lease is created before spawn and captured by the future, so aborting
    // an unpolled task still clears Starting and its boot fingerprint.
    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let handle = tokio::spawn(async move {
        let lease = lease;
        if !lease.promote() {
            return;
        }
        #[cfg(test)]
        if PANIC_AFTER_PROMOTE.swap(false, Ordering::AcqRel) {
            panic!("injected reconciler lifecycle panic");
        }
        while !task_shutdown.is_cancelled() {
            if std::panic::AssertUnwindSafe(run_loop(&pool, boot, task_shutdown.clone()))
                .catch_unwind()
                .await
                .is_err()
            {
                tracing::error!("[intake_delivery_reconciler] loop panicked; restarting");
                tokio::task::yield_now().await;
            }
        }
    });
    *TASK_OWNER.lock().unwrap_or_else(|e| e.into_inner()) = Some(TaskOwner {
        handle: Some(handle),
        shutdown,
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchemaReason {
    Ready,
    Query,
    MigrationFloor,
    Relation,
    Privilege,
    IntakeColumns,
    IntakeConstraint,
    JournalColumns,
    JournalConstraint,
    Index,
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

impl AuthorityDecision {
    fn new(effective: bool, reason: AuthorityReason) -> Self {
        let decision = Self { effective, reason };
        debug_assert_eq!(
            decision.effective,
            decision.reason == AuthorityReason::Effective
        );
        decision
    }
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
        return AuthorityDecision::new(false, AuthorityReason::NotRequested);
    };
    if let Err(reason) = authority_without_probe(&runtime) {
        return AuthorityDecision::new(false, reason);
    }
    let schema = probe_schema_readiness(pool).await;
    AuthorityDecision::new(
        schema.ready,
        if schema.ready {
            AuthorityReason::Effective
        } else {
            AuthorityReason::Schema(schema.reason)
        },
    )
}

fn sql_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        if ch == '\'' || ch == '"' {
            let quote = ch;
            let mut token = ch.to_string();
            while let Some(inner) = chars.next() {
                token.push(inner);
                if inner == quote {
                    if chars.peek() == Some(&quote) {
                        token.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
            }
            tokens.push(token);
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            let mut token = ch.to_string();
            while chars
                .peek()
                .is_some_and(|next| next.is_ascii_alphanumeric() || *next == '_')
            {
                token.push(chars.next().unwrap());
            }
            tokens.push(token);
        } else {
            let mut token = ch.to_string();
            if matches!(
                (ch, chars.peek()),
                (':' | '<' | '>' | '!', Some(':' | '=' | '>'))
            ) {
                token.push(chars.next().unwrap());
            }
            tokens.push(token);
        }
    }
    if tokens.len() >= 2
        && tokens[tokens.len() - 2].eq_ignore_ascii_case("NOT")
        && tokens[tokens.len() - 1].eq_ignore_ascii_case("VALID")
    {
        tokens.truncate(tokens.len() - 2);
    }
    tokens
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
                    (sql_tokens(&definition) == sql_tokens(item.1)).then_some((name, validated))
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
        return Ok(SchemaReason::Relation);
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
        return Ok(SchemaReason::Privilege);
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
    if intake_columns != 4 {
        return Ok(SchemaReason::IntakeColumns);
    }
    if intake_checks.len() != 2 {
        return Ok(SchemaReason::IntakeConstraint);
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
                return Ok(SchemaReason::IntakeConstraint);
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
    if journal_columns != 11 {
        return Ok(SchemaReason::JournalColumns);
    }
    if journal_checks.len() != 5 || journal_checks.iter().any(|(_, validated)| !validated) {
        return Ok(SchemaReason::JournalConstraint);
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
        SchemaReason::Index
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
        if is_delivery_witness(judgment.delivered_outbox_id(), row_id) {
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

fn is_delivery_witness(delivered_outbox_id: Option<i64>, row_id: i64) -> bool {
    delivered_outbox_id == Some(row_id)
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
    let Some(cutoff) = Utc::now().checked_sub_signed(stale_age) else {
        return;
    };
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

async fn run_loop(pool: &PgPool, boot: BootFingerprint, shutdown: CancellationToken) {
    let mut ticks = tokio::time::interval(Duration::from_secs(boot.period_secs));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = ticks.tick() => tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                _ = run_tick(pool, boot) => {},
            },
        }
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

    fn runtime() -> RuntimeSettingsConfig {
        RuntimeSettingsConfig {
            delivery_journal_intake_authority: true,
            delivery_journal_mode: DeliveryJournalMode::Shadow,
            delivery_journal_intake_reconcile_period_secs: Some(1),
            delivery_journal_intake_stale_age_secs: Some(2),
            delivery_journal_intake_reconcile_batch_size: Some(3),
            ..Default::default()
        }
    }

    fn reset() {
        ACTIVE_GENERATION.store(0, Ordering::Release);
        LIFECYCLE.store(UNREGISTERED, Ordering::Release);
        *FINGERPRINT.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(owner) = TASK_OWNER.lock().unwrap_or_else(|e| e.into_inner()).take() {
            owner.handle.as_ref().unwrap().abort();
        }
    }

    #[tokio::test]
    async fn authority_requires_every_live_term() {
        let _guard = LOCK.lock().await;
        reset();
        let mut runtime = runtime();
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

    #[tokio::test]
    async fn tick_gate_ignores_authority_and_mode_for_rollback_drain() {
        let _guard = LOCK.lock().await;
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
            assert!(matches!(
                ReconcilerBootConfig::from_runtime(&runtime),
                ReconcilerBootConfig::Disabled
            ));
            assert_eq!(runtime.delivery_journal_intake_authority, requested);
        }
        let body = include_str!("intake_delivery_reconciler.rs")
            .split("async fn run_tick")
            .nth(1)
            .unwrap()
            .split("async fn run_loop")
            .next()
            .unwrap();
        assert!(!body.contains("delivery_journal_intake_authority"));
        assert!(!body.contains("delivery_journal_mode"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn singleton_registration_clears_on_abort_and_rejects_duplicate_spawn() {
        let _guard = LOCK.lock().await;
        reset();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        ensure_spawned(Some(&pool), &runtime());
        assert_eq!(LIFECYCLE.load(Ordering::Acquire), STARTING);
        ensure_spawned(Some(&pool), &runtime());
        let mut owner = TASK_OWNER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap();
        owner.handle.as_ref().unwrap().abort();
        assert!(
            owner
                .handle
                .take()
                .unwrap()
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert_eq!(LIFECYCLE.load(Ordering::Acquire), UNREGISTERED);
        PANIC_AFTER_PROMOTE.store(true, Ordering::Release);
        ensure_spawned(Some(&pool), &runtime());
        let mut owner = TASK_OWNER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap();
        assert!(owner.handle.take().unwrap().await.unwrap_err().is_panic());
        assert_eq!(LIFECYCLE.load(Ordering::Acquire), UNREGISTERED);
        ensure_spawned(Some(&pool), &runtime());
        tokio::task::yield_now().await;
        let mut owner = TASK_OWNER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap();
        assert_eq!(LIFECYCLE.load(Ordering::Acquire), RUNNING);
        assert!(!owner.shutdown.is_cancelled());
        owner.shutdown.cancel();
        owner.handle.take().unwrap().await.unwrap();
        assert_eq!(LIFECYCLE.load(Ordering::Acquire), UNREGISTERED);
        reset();
    }

    #[test]
    fn existential_reduction_continues_after_first_gap() {
        let row_id = 41;
        assert!(
            [None, Some(99), Some(row_id)]
                .into_iter()
                .any(|binding| is_delivery_witness(binding, row_id))
        );
        assert!(
            ![None, Some(99)]
                .into_iter()
                .any(|binding| is_delivery_witness(binding, row_id))
        );
    }

    #[test]
    fn sql_tokenizer_preserves_literal_whitespace_and_quoted_escapes() {
        assert_ne!(
            sql_tokens("CHECK (status='dis patched')"),
            sql_tokens("CHECK(status = 'dispatched')")
        );
        assert_eq!(
            sql_tokens("CHECK (name = 'it''s ok') NOT VALID"),
            sql_tokens("CHECK(name='it''s ok')")
        );
        assert_ne!(
            sql_tokens("CHECK (\"a b\" = 'x')"),
            sql_tokens("CHECK (\"ab\" = 'x')")
        );
    }

    #[test]
    fn oversized_tunables_disable_without_panicking() {
        let mut runtime = runtime();
        runtime.delivery_journal_intake_reconcile_period_secs = Some(u64::MAX);
        assert!(matches!(
            ReconcilerBootConfig::from_runtime(&runtime),
            ReconcilerBootConfig::Disabled
        ));
        runtime.delivery_journal_intake_reconcile_period_secs = Some(1);
        runtime.delivery_journal_intake_stale_age_secs = Some(u64::MAX);
        assert!(matches!(
            ReconcilerBootConfig::from_runtime(&runtime),
            ReconcilerBootConfig::Disabled
        ));
    }
}

#[cfg(test)]
#[rustfmt::skip]
mod postgres_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    async fn database() -> (TestPostgresDb, PgPool) {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        (db, pool)
    }
    async fn outbox(pool:&PgPool,key:&str,at:chrono::DateTime<Utc>)->i64 { sqlx::query_scalar("INSERT INTO public.intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,status,claim_owner,dispatched_at) VALUES('w','l',$1,$1,'u','x','standard','a','dispatched','c',$2) RETURNING id").bind(key).bind(at).fetch_one(pool).await.unwrap() }
    async fn delivered(pool:&PgPool,row:i64,obligation:Uuid) { let attempt=Uuid::new_v4(); let ids=[Uuid::new_v4(),Uuid::new_v4(),Uuid::new_v4(),Uuid::new_v4()]; sqlx::query("INSERT INTO public.delivery_journal_events(event_id,obligation_id,attempt_id,event_kind,event_seq,idempotency_key,canonical_payload,requested_channel_id,returned_channel_id,message_id) VALUES($1,$5,NULL,'O',0,uuid_send($1),jsonb_build_object('intake_outbox_id',$6),NULL,NULL,NULL),($2,$5,$7,'A',1,uuid_send($2),'{\"frontier_start\":0,\"frontier_end\":1}',NULL,NULL,NULL),($3,$5,$7,'T',2,uuid_send($3),'{\"requested_channel_id\":\"1\",\"returned_channel_id\":\"1\",\"message_id\":\"2\"}','1','1','2'),($4,$5,$7,'C',3,uuid_send($4),'{\"frontier_start\":0,\"frontier_end\":1}',NULL,NULL,NULL)").bind(ids[0]).bind(ids[1]).bind(ids[2]).bind(ids[3]).bind(obligation).bind(row).bind(attempt).execute(pool).await.unwrap(); }
    async fn status(pool:&PgPool,row:i64)->(String,Option<chrono::DateTime<Utc>>,String,chrono::DateTime<Utc>) { sqlx::query_as("SELECT status,completed_at,claim_owner,dispatched_at FROM public.intake_outbox WHERE id=$1").bind(row).fetch_one(pool).await.unwrap() }

    #[tokio::test]
    async fn capability_accepts_0109_without_completion_uuid_pg() {
        let (db, pool) = database().await;
        assert_eq!(
            probe_schema_readiness(&pool).await.reason,
            SchemaReason::Ready
        );
        let completion: i64=sqlx::query_scalar("SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND table_name='intake_outbox' AND udt_name='uuid'").fetch_one(&pool).await.unwrap();
        assert_eq!(completion, 0);
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn capability_rejects_bad_rows_constraints_and_indexes_pg() {
        let (db, pool) = database().await;
        sqlx::raw_sql("CREATE SCHEMA attacker; CREATE TABLE attacker.intake_outbox (LIKE public.intake_outbox INCLUDING ALL); CREATE TABLE attacker.delivery_journal_events (LIKE public.delivery_journal_events INCLUDING ALL)").execute(&pool).await.unwrap();
        sqlx::query("SET search_path=attacker,public").execute(&pool).await.unwrap();
        assert!(probe_schema_readiness(&pool).await.ready);
        let cutoff=Utc::now(); let public_row=outbox(&pool,"public",cutoff-ChronoDuration::seconds(2)).await; delivered(&pool,public_row,Uuid::new_v4()).await;
        sqlx::query("INSERT INTO attacker.intake_outbox(id,target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,status,claim_owner,dispatched_at) VALUES($1,'w','l','attacker','attacker','u','x','standard','a','dispatched','c',$2)").bind(public_row).bind(cutoff-ChronoDuration::seconds(2)).execute(&pool).await.unwrap();
        reconcile_row(&pool,public_row,cutoff).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT status FROM public.intake_outbox WHERE id=$1").bind(public_row).fetch_one(&pool).await.unwrap(),"done");
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT status FROM attacker.intake_outbox WHERE id=$1").bind(public_row).fetch_one(&pool).await.unwrap(),"dispatched");
        for ddl in [
            "ALTER TABLE public.intake_outbox DROP CONSTRAINT intake_outbox_dispatched_requires_clock; ALTER TABLE public.intake_outbox ADD CONSTRAINT intake_outbox_dispatched_requires_clock CHECK (status <> 'dis patched'::text OR dispatched_at IS NOT NULL)",
            "ALTER TABLE public.intake_outbox DROP CONSTRAINT intake_outbox_dispatched_requires_clock; ALTER TABLE public.intake_outbox ADD CONSTRAINT intake_outbox_dispatched_requires_clock CHECK (true)",
        ] { sqlx::raw_sql(ddl).execute(&pool).await.unwrap(); assert!(!probe_schema_readiness(&pool).await.ready); }
        sqlx::raw_sql("ALTER TABLE public.intake_outbox DROP CONSTRAINT intake_outbox_dispatched_requires_clock; ALTER TABLE public.intake_outbox ADD CONSTRAINT intake_outbox_dispatched_requires_clock CHECK (status <> 'dispatched'::text OR dispatched_at IS NOT NULL)").execute(&pool).await.unwrap();
        let bad=outbox(&pool,"bad-clock",cutoff-ChronoDuration::seconds(2)).await; sqlx::raw_sql("ALTER TABLE public.intake_outbox DROP CONSTRAINT intake_outbox_dispatched_requires_clock").execute(&pool).await.unwrap(); sqlx::query("UPDATE public.intake_outbox SET dispatched_at=NULL WHERE id=$1").bind(bad).execute(&pool).await.unwrap(); sqlx::raw_sql("ALTER TABLE public.intake_outbox ADD CONSTRAINT intake_outbox_dispatched_requires_clock CHECK (status <> 'dispatched'::text OR dispatched_at IS NOT NULL) NOT VALID").execute(&pool).await.unwrap(); assert!(!probe_schema_readiness(&pool).await.ready); sqlx::query("UPDATE public.intake_outbox SET dispatched_at=$2 WHERE id=$1").bind(bad).bind(cutoff).execute(&pool).await.unwrap(); sqlx::raw_sql("ALTER TABLE public.intake_outbox VALIDATE CONSTRAINT intake_outbox_dispatched_requires_clock").execute(&pool).await.unwrap();
        sqlx::raw_sql("ALTER TABLE public.intake_outbox DROP CONSTRAINT intake_outbox_status_check").execute(&pool).await.unwrap(); let future=outbox(&pool,"future",cutoff).await; sqlx::query("UPDATE public.intake_outbox SET status='future_status' WHERE id=$1").bind(future).execute(&pool).await.unwrap(); sqlx::raw_sql("ALTER TABLE public.intake_outbox ADD CONSTRAINT intake_outbox_status_check CHECK (status = ANY (ARRAY['pending'::text,'claimed'::text,'accepted'::text,'spawned'::text,'dispatched'::text,'unknown'::text,'done'::text,'failed_pre_accept'::text,'failed_post_accept'::text])) NOT VALID").execute(&pool).await.unwrap(); assert!(!probe_schema_readiness(&pool).await.ready); sqlx::query("DELETE FROM public.intake_outbox WHERE id=$1").bind(future).execute(&pool).await.unwrap(); sqlx::raw_sql("ALTER TABLE public.intake_outbox VALIDATE CONSTRAINT intake_outbox_status_check").execute(&pool).await.unwrap(); let official=outbox(&pool,"official-unknown",cutoff).await; sqlx::query("UPDATE public.intake_outbox SET status='unknown',completed_at=NOW() WHERE id=$1").bind(official).execute(&pool).await.unwrap(); assert!(probe_schema_readiness(&pool).await.ready);
        sqlx::raw_sql("ALTER TABLE public.delivery_journal_events DROP CONSTRAINT delivery_journal_kind_check; ALTER TABLE public.delivery_journal_events ADD CONSTRAINT delivery_journal_kind_check CHECK (event_kind = ANY (ARRAY['O'::text,'A'::text,'T'::text,'C'::text,'S'::text,'U'::text])) NOT VALID").execute(&pool).await.unwrap(); assert!(!probe_schema_readiness(&pool).await.ready); sqlx::raw_sql("ALTER TABLE public.delivery_journal_events VALIDATE CONSTRAINT delivery_journal_kind_check").execute(&pool).await.unwrap();
        sqlx::query("UPDATE public._sqlx_migrations SET success=false WHERE version=109").execute(&pool).await.unwrap(); assert_eq!(probe_schema_readiness(&pool).await.reason,SchemaReason::MigrationFloor); sqlx::query("UPDATE public._sqlx_migrations SET success=true WHERE version=109").execute(&pool).await.unwrap();
        for column in ["indisvalid","indisready","indislive"] { let sql=format!("UPDATE pg_catalog.pg_index SET {column}=false WHERE indexrelid='public.idx_delivery_journal_intake_binding'::regclass"); sqlx::query(&sql).execute(&pool).await.unwrap(); assert!(!probe_schema_readiness(&pool).await.ready); let sql=format!("UPDATE pg_catalog.pg_index SET {column}=true WHERE indexrelid='public.idx_delivery_journal_intake_binding'::regclass"); sqlx::query(&sql).execute(&pool).await.unwrap(); }
        for replacement in [
            "CREATE INDEX idx_delivery_journal_intake_binding ON public.intake_outbox(id) WHERE status='dispatched'",
            "CREATE INDEX idx_delivery_journal_intake_binding ON public.delivery_journal_events ((canonical_payload->>'wrong')) WHERE event_kind='O'",
            "CREATE INDEX idx_delivery_journal_intake_binding ON public.delivery_journal_events ((canonical_payload->>'intake_outbox_id')) WHERE event_kind='A'",
        ] { sqlx::query("DROP INDEX public.idx_delivery_journal_intake_binding").execute(&pool).await.unwrap(); sqlx::query(replacement).execute(&pool).await.unwrap(); assert!(!probe_schema_readiness(&pool).await.ready); }
        sqlx::raw_sql("DROP INDEX public.idx_delivery_journal_intake_binding; CREATE INDEX idx_delivery_journal_intake_binding ON public.delivery_journal_events ((canonical_payload ->> 'intake_outbox_id')) WHERE event_kind='O'").execute(&pool).await.unwrap(); assert!(probe_schema_readiness(&pool).await.ready);
        sqlx::raw_sql("DROP ROLE IF EXISTS intake_probe_limited; CREATE ROLE intake_probe_limited NOLOGIN; GRANT USAGE ON SCHEMA public TO intake_probe_limited; GRANT SELECT ON public._sqlx_migrations, public.intake_outbox, public.delivery_journal_events TO intake_probe_limited").execute(&pool).await.unwrap(); let mut limited=pool.begin().await.unwrap(); sqlx::query("SET LOCAL ROLE intake_probe_limited").execute(&mut *limited).await.unwrap(); assert_ne!(probe_inner(&mut limited).await.unwrap(),SchemaReason::Ready); limited.rollback().await.unwrap(); sqlx::raw_sql("DROP OWNED BY intake_probe_limited; DROP ROLE intake_probe_limited").execute(&pool).await.unwrap();
        sqlx::query("ALTER TABLE public.intake_outbox RENAME TO intake_outbox_hidden").execute(&pool).await.unwrap(); assert!(!probe_schema_readiness(&pool).await.ready); sqlx::query("ALTER TABLE public.intake_outbox_hidden RENAME TO intake_outbox").execute(&pool).await.unwrap();
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn reconciler_existential_done_and_unknown_pg() {
        let (db, pool) = database().await;
        let cutoff=Utc::now(); let done=outbox(&pool,"done",cutoff-ChronoDuration::seconds(10)).await; let unknown=outbox(&pool,"unknown",cutoff-ChronoDuration::seconds(10)).await;
        let gap=Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(); let witness=Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        sqlx::query("INSERT INTO public.delivery_journal_events(event_id,obligation_id,event_kind,event_seq,idempotency_key,canonical_payload) VALUES($1,$2,'O',0,uuid_send($1),jsonb_build_object('intake_outbox_id',$3))").bind(Uuid::new_v4()).bind(gap).bind(done).execute(&pool).await.unwrap(); delivered(&pool,done,witness).await;
        run_tick(&pool,BootFingerprint{period_secs:1,stale_age_secs:1,batch_size:3}).await;
        let statuses:Vec<String>=sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id=ANY($1) ORDER BY id").bind(vec![done,unknown]).fetch_all(&pool).await.unwrap(); assert_eq!(statuses,vec!["done","unknown"]);
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn reconciler_cutoff_status_and_rollback_are_transactional_pg() {
        let (db, pool) = database().await;
        let cutoff=Utc::now();
        let equal=outbox(&pool,"equal",cutoff).await; delivered(&pool,equal,Uuid::new_v4()).await; reconcile_row(&pool,equal,cutoff).await.unwrap(); assert_eq!(status(&pool,equal).await.0,"dispatched");
        let refreshed=outbox(&pool,"refresh",cutoff-ChronoDuration::seconds(2)).await; delivered(&pool,refreshed,Uuid::new_v4()).await; sqlx::query("UPDATE public.intake_outbox SET dispatched_at=$2 WHERE id=$1").bind(refreshed).bind(cutoff+ChronoDuration::seconds(1)).execute(&pool).await.unwrap(); reconcile_row(&pool,refreshed,cutoff).await.unwrap(); let fresh=status(&pool,refreshed).await; assert_eq!(fresh.0,"dispatched"); assert_eq!(fresh.3,cutoff+ChronoDuration::seconds(1));
        let competed=outbox(&pool,"competed",cutoff-ChronoDuration::seconds(2)).await; delivered(&pool,competed,Uuid::new_v4()).await; sqlx::query("UPDATE public.intake_outbox SET status='done',completed_at=NOW() WHERE id=$1").bind(competed).execute(&pool).await.unwrap(); reconcile_row(&pool,competed,cutoff).await.unwrap(); assert_eq!(status(&pool,competed).await.0,"done");
        let rollback=outbox(&pool,"rollback",cutoff-ChronoDuration::seconds(2)).await; delivered(&pool,rollback,Uuid::new_v4()).await; sqlx::raw_sql("CREATE FUNCTION public.reject_proof_done() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.status='done' AND OLD.status='dispatched' THEN RAISE EXCEPTION 'forced rollback after write'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_proof_done AFTER UPDATE ON public.intake_outbox FOR EACH ROW EXECUTE FUNCTION public.reject_proof_done()").execute(&pool).await.unwrap(); assert!(reconcile_row(&pool,rollback,cutoff).await.is_err()); sqlx::raw_sql("DROP TRIGGER reject_proof_done ON public.intake_outbox; DROP FUNCTION public.reject_proof_done()").execute(&pool).await.unwrap(); let rolled=status(&pool,rollback).await; assert_eq!(rolled.0,"dispatched"); assert!(rolled.1.is_none()); assert_eq!(rolled.2,"c");
        pool.close().await;
        db.drop().await;
    }
}
