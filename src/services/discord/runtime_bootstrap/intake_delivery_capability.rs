//! Boot/reload PostgreSQL capability probe for intake-delivery reconciliation.
//!
//! A bridge turn samples [`SettlementCapabilities`] before its dispatched-stamp await, and that
//! snapshot governs the stamp through completion. A stage downgrade clears the cache bits
//! immediately so later turns cannot stamp. A later stage elevation cannot publish its newly
//! requested capability until its schema probe completes. Bridge-exit settlement consumes the
//! same per-turn snapshot that the stamp consumed, so a downgrade cannot strand that turn while
//! fresh Off/Observe turns remain database-neutral.

#![allow(dead_code)]
use crate::config::IntakeDeliverySettlementStage;
use sqlx::{Connection, PgConnection, PgPool};

mod cache;
pub(in crate::services::discord) use cache::{SettlementCapabilityCache, bootstrap};

/// One turn's immutable intake-delivery authority snapshot.
///
/// Package visibility is the minimum production visibility shared by the sibling
/// `runtime_bootstrap`, `router`, and `turn_bridge` modules that publish, stamp with, and settle
/// from this value; it is not widened for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct SettlementCapabilities {
    /// Whether this snapshot authorizes a dispatched handoff stamp.
    pub(in crate::services::discord) stamp_dispatched: bool,
    /// Authorizes bridge-exit settlement: set only for Settle/Enforce after a Ready schema
    /// probe, and `settle_intake_row_at_bridge_exit` returns early without it. The recovery
    /// sweep is NOT stage-gated the same way: `intake_delivery_sweep::sweep_once` takes this
    /// as one disjunct of two and proceeds whenever open stamp debt exists -- its own gate
    /// decides, not this field alone.
    pub(in crate::services::discord) settle_and_sweep: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SchemaReason {
    /// Required reconciliation terms are readable and have the expected shape.
    ///
    /// This is not an INSERT-success guarantee: additional NOT NULL or
    /// generated columns, triggers, RLS, and additional CHECK constraints are
    /// outside this probe and can still reject a future writer.
    Ready,
    Query,
    Migration,
    Relation,
    Privilege,
    Columns,
    Constraint,
    Index,
}
fn sql_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        let mut token = ch.to_string();
        if ch == '\'' || ch == '"' {
            while let Some(next) = chars.next() {
                token.push(next);
                if next == ch {
                    if chars.peek() == Some(&ch) {
                        token.push(chars.next().expect("peeked quote"));
                    } else {
                        break;
                    }
                }
            }
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            while chars
                .peek()
                .is_some_and(|next| next.is_ascii_alphanumeric() || *next == '_')
            {
                token.push(chars.next().expect("peeked identifier character"));
            }
        } else if matches!(
            (ch, chars.peek()),
            (':' | '<' | '>' | '!', Some(':' | '=' | '>'))
        ) {
            token.push(chars.next().expect("peeked operator character"));
        }
        tokens.push(token);
    }
    if tokens.len() >= 2
        && tokens[tokens.len() - 2].eq_ignore_ascii_case("NOT")
        && tokens[tokens.len() - 1].eq_ignore_ascii_case("VALID")
    {
        tokens.truncate(tokens.len() - 2);
    }
    tokens
}
const INTAKE_CHECKS: [(&str, &str); 2] = [
    (
        "intake_outbox_dispatched_requires_clock",
        "CHECK (status <> 'dispatched'::text OR dispatched_at IS NOT NULL)",
    ),
    (
        "intake_outbox_status_check",
        "CHECK (status = ANY (ARRAY['pending'::text, 'claimed'::text, 'accepted'::text, 'spawned'::text, 'dispatched'::text, 'unknown'::text, 'done'::text, 'failed_pre_accept'::text, 'failed_post_accept'::text]))",
    ),
];
const JOURNAL_CHECKS: [(&str, &str); 5] = [
    (
        "delivery_journal_attempt_check",
        "CHECK ((event_kind = ANY (ARRAY['T'::text, 'A'::text, 'C'::text, 'U'::text])) AND attempt_id IS NOT NULL OR (event_kind = ANY (ARRAY['O'::text, 'S'::text])) AND attempt_id IS NULL)",
    ),
    (
        "delivery_journal_kind_check",
        "CHECK (event_kind = ANY (ARRAY['O'::text, 'A'::text, 'T'::text, 'C'::text, 'S'::text, 'U'::text]))",
    ),
    (
        "delivery_journal_obligation_slot_unique",
        "UNIQUE (obligation_id, event_seq)",
    ),
    (
        "delivery_journal_slot_check",
        "CHECK (event_kind = 'O'::text AND event_seq = 0 OR (event_kind = ANY (ARRAY['A'::text, 'S'::text])) AND event_seq = 1 OR (event_kind = ANY (ARRAY['T'::text, 'U'::text])) AND event_seq = 2 OR event_kind = 'C'::text AND event_seq = 3)",
    ),
    (
        "delivery_journal_transport_receipt_check",
        "CHECK (event_kind <> 'T'::text OR requested_channel_id IS NOT NULL AND returned_channel_id IS NOT NULL AND message_id IS NOT NULL)",
    ),
];
async fn relation_oids(conn: &mut PgConnection) -> Result<Option<(i64, i64)>, sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT c.relname,c.oid::bigint
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
          WHERE n.nspname='public' AND c.relkind IN ('r','p')
            AND c.relname=ANY($1::text[])
          ORDER BY c.relname",
    )
    .bind(["delivery_journal_events", "intake_outbox"])
    .fetch_all(conn)
    .await?;
    Ok(match rows.as_slice() {
        [(journal_name, journal), (intake_name, intake)]
            if journal_name == "delivery_journal_events" && intake_name == "intake_outbox" =>
        {
            Some((*journal, *intake))
        }
        _ => None,
    })
}
async fn exact_constraints(
    conn: &mut PgConnection,
    oid: i64,
    expected: &[(&str, &str)],
) -> Result<Vec<(String, bool)>, sqlx::Error> {
    let rows: Vec<(String, bool, String)> = sqlx::query_as(
        "SELECT conname,convalidated,pg_get_constraintdef(oid,true)
           FROM pg_catalog.pg_constraint
          WHERE conrelid=$1::oid AND conname=ANY($2::text[])
          ORDER BY conname",
    )
    .bind(oid)
    .bind(expected.iter().map(|item| item.0).collect::<Vec<_>>())
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(name, validated, definition)| {
            expected
                .iter()
                .find(|item| item.0 == name)
                .filter(|item| sql_tokens(item.1) == sql_tokens(&definition))
                .map(|_| (name, validated))
        })
        .collect())
}
async fn catalog_shape(
    conn: &mut PgConnection,
    journal: i64,
    intake: i64,
) -> Result<SchemaReason, sqlx::Error> {
    let columns: (i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM pg_catalog.pg_attribute a
             WHERE a.attrelid=$1::oid AND a.attnum>0 AND NOT a.attisdropped AND
               ((a.attname='id' AND a.atttypid='pg_catalog.int8'::pg_catalog.regtype AND a.attnotnull) OR
                (a.attname='status' AND a.atttypid='pg_catalog.text'::pg_catalog.regtype AND a.attnotnull) OR
                (a.attname='dispatched_at' AND a.atttypid='pg_catalog.timestamptz'::pg_catalog.regtype AND NOT a.attnotnull AND NOT a.atthasdef) OR
                (a.attname='completed_at' AND a.atttypid='pg_catalog.timestamptz'::pg_catalog.regtype AND NOT a.attnotnull))),
           (SELECT count(*) FROM pg_catalog.pg_attribute a
             WHERE a.attrelid=$2::oid AND a.attnum>0 AND NOT a.attisdropped AND
               ((a.attname IN ('event_id','obligation_id') AND a.atttypid='pg_catalog.uuid'::pg_catalog.regtype AND a.attnotnull) OR
                (a.attname='attempt_id' AND a.atttypid='pg_catalog.uuid'::pg_catalog.regtype AND NOT a.attnotnull) OR
                (a.attname='event_kind' AND a.atttypid='pg_catalog.text'::pg_catalog.regtype AND a.attnotnull) OR
                (a.attname='event_seq' AND a.atttypid='pg_catalog.int2'::pg_catalog.regtype AND a.attnotnull) OR
                (a.attname='idempotency_key' AND a.atttypid='pg_catalog.bytea'::pg_catalog.regtype AND a.attnotnull) OR
                (a.attname='canonical_payload' AND a.atttypid='pg_catalog.jsonb'::pg_catalog.regtype AND a.attnotnull) OR
                (a.attname IN ('requested_channel_id','returned_channel_id','message_id') AND a.atttypid='pg_catalog.text'::pg_catalog.regtype AND NOT a.attnotnull) OR
                (a.attname='observed_at' AND a.atttypid='pg_catalog.timestamptz'::pg_catalog.regtype AND a.attnotnull)))",
    )
    .bind(intake)
    .bind(journal)
    .fetch_one(&mut *conn)
    .await?;
    if columns != (4, 11) {
        return Ok(SchemaReason::Columns);
    }
    let intake_checks = exact_constraints(conn, intake, &INTAKE_CHECKS).await?;
    let journal_checks = exact_constraints(conn, journal, &JOURNAL_CHECKS).await?;
    if intake_checks.len() != 2 || journal_checks.len() != 5 || journal_checks.iter().any(|v| !v.1)
    {
        return Ok(SchemaReason::Constraint);
    }
    for (name, validated) in intake_checks {
        if validated {
            continue;
        }
        let clean: bool = if name == "intake_outbox_status_check" {
            sqlx::query_scalar(
                "SELECT NOT EXISTS(SELECT 1 FROM public.intake_outbox
                  WHERE status IS NULL OR NOT(status=ANY($1::text[])))",
            )
            .bind(
                crate::db::intake_outbox_status::IntakeOutboxStatus::ALL
                    .map(|status| status.as_str()),
            )
            .fetch_one(&mut *conn)
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT NOT EXISTS(SELECT 1 FROM public.intake_outbox
                  WHERE status='dispatched' AND dispatched_at IS NULL)",
            )
            .fetch_one(&mut *conn)
            .await?
        };
        if !clean {
            return Ok(SchemaReason::Constraint);
        }
    }
    // `pg_get_indexdef` returns empty text when the requested key number is
    // beyond `indnkeyatts`. Exact key counts below keep that no-key sentinel
    // distinguishable from an occupied key slot.
    let indexes: bool = sqlx::query_scalar(
        "WITH expected(name,table_oid,is_unique,key_count,key1,key2,predicate) AS (VALUES
          ('idx_intake_outbox_stale_dispatched',$1::oid,false,1::smallint,'dispatched_at',''::text,'(status = ''dispatched''::text)'),
          ('idx_delivery_journal_intake_binding',$2::oid,false,1::smallint,'(canonical_payload ->> ''intake_outbox_id''::text)',''::text,'(event_kind = ''O''::text)'),
          ('delivery_journal_single_o_a_t',$2::oid,true,2::smallint,'obligation_id','event_kind','(event_kind = ANY (ARRAY[''O''::text, ''A''::text, ''T''::text]))'),
          ('delivery_journal_single_terminal',$2::oid,true,1::smallint,'obligation_id',''::text,'(event_kind = ANY (ARRAY[''C''::text, ''S''::text, ''U''::text]))'),
          ('delivery_journal_obligation_order',$2::oid,false,2::smallint,'obligation_id','event_seq',NULL::text))
        SELECT count(*)=5 AND bool_and(
          am.amname='btree' AND i.indisvalid AND i.indisready AND i.indislive
          AND i.indisunique=expected.is_unique
          AND i.indnkeyatts=expected.key_count AND i.indnatts=expected.key_count
          AND pg_get_indexdef(i.indexrelid,1,true)=expected.key1
          AND pg_get_indexdef(i.indexrelid,2,true) IS NOT DISTINCT FROM expected.key2
          AND pg_get_expr(i.indpred,i.indrelid) IS NOT DISTINCT FROM expected.predicate)
        FROM expected
        JOIN pg_catalog.pg_class c ON c.relname=expected.name
        JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace AND n.nspname='public'
        JOIN pg_catalog.pg_index i ON i.indexrelid=c.oid AND i.indrelid=expected.table_oid
        JOIN pg_catalog.pg_am am ON am.oid=c.relam",
    )
    .bind(intake)
    .bind(journal)
    .fetch_one(conn)
    .await?;
    Ok(if indexes {
        SchemaReason::Ready
    } else {
        SchemaReason::Index
    })
}
async fn probe_inner(conn: &mut PgConnection) -> Result<SchemaReason, sqlx::Error> {
    let required_migrations =
        crate::db::intake_delivery_required_migrations::INTAKE_DELIVERY_REQUIRED_MIGRATIONS;
    let migration_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._sqlx_migrations
          WHERE version=ANY($1::bigint[]) AND success",
    )
    .bind(required_migrations)
    .fetch_one(&mut *conn)
    .await?;
    if migration_count != required_migrations.len() as i64 {
        return Ok(SchemaReason::Migration);
    }
    let Some((journal, intake)) = relation_oids(conn).await? else {
        return Ok(SchemaReason::Relation);
    };
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
    sqlx::query(
        "LOCK TABLE public.intake_outbox,public.delivery_journal_events IN ACCESS SHARE MODE",
    )
    .execute(&mut *conn)
    .await?;
    catalog_shape(conn, journal, intake).await
}

async fn probe_transaction(conn: &mut PgConnection) -> Result<SchemaReason, sqlx::Error> {
    let mut tx = conn.begin().await?;
    let result = async {
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let reason = probe_inner(&mut tx).await?;
        Ok::<_, sqlx::Error>(reason)
    }
    .await;
    tx.rollback().await?;
    result
}

pub(super) async fn probe_schema(pool: &PgPool) -> SchemaReason {
    let result = async {
        let mut conn = pool.acquire().await?;
        probe_transaction(&mut conn).await
    }
    .await;
    result.unwrap_or_else(|error| {
        tracing::warn!("[intake_delivery_capability] probe failed: {error}");
        SchemaReason::Query
    })
}

fn capabilities_for(
    stage: IntakeDeliverySettlementStage,
    schema: SchemaReason,
) -> SettlementCapabilities {
    // S-W3's stale spawned/dispatched sweep landed in #5385, so S-W4 removes
    // the temporary dispatched-stamping clamp and activates the design formula.
    SettlementCapabilities {
        stamp_dispatched: stage >= IntakeDeliverySettlementStage::Enforce
            && schema == SchemaReason::Ready,
        settle_and_sweep: schema == SchemaReason::Ready
            && stage >= IntakeDeliverySettlementStage::Settle,
    }
}

#[cfg(test)]
pub(in crate::services::discord) use cache::bootstrap_from_receiver_for_test;

#[cfg(test)]
#[path = "intake_delivery_capability/tests.rs"]
mod tests;

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use uuid::Uuid;

    async fn database() -> (TestPostgresDb, PgPool) {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        (database, pool)
    }

    async fn finish(database: TestPostgresDb, pool: PgPool) {
        pool.close().await;
        database.drop().await;
    }

    async fn execute(pool: &PgPool, statement: &str) {
        sqlx::query(statement).execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn capability_accepts_public_0109_under_hostile_search_path_pg() {
        let (database, pool) = database().await;
        assert_eq!(probe_schema(&pool).await, SchemaReason::Ready);
        let forbidden: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_schema='public' AND table_name='intake_outbox'
                AND data_type='uuid'
                AND column_name IN ('completion_uuid','completion_obligation_id','completing_obligation_id')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(forbidden, 0);

        let schema = format!("attacker_{}", Uuid::new_v4().simple());
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.intake_outbox
               (LIKE public.intake_outbox INCLUDING ALL);
             CREATE TABLE {schema}.delivery_journal_events
               (LIKE public.delivery_journal_events INCLUDING ALL)"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(&format!("SET LOCAL search_path={schema},public"))
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_eq!(probe_inner(&mut tx).await.unwrap(), SchemaReason::Ready);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {schema}.intake_outbox"))
                .fetch_one(&mut *tx)
                .await
                .unwrap(),
            0
        );
        tx.rollback().await.unwrap();
        execute(&pool, &format!("DROP SCHEMA {schema} CASCADE")).await;

        // A repeatable-read catalog re-read after a name-based lock remains on
        // the original snapshot even when the lock resolved a replacement.
        // Repeating `relation_oids` after the lock would therefore be inert.
        let snapshot_schema = format!("snapshot_{}", Uuid::new_v4().simple());
        execute(&pool, &format!("CREATE SCHEMA {snapshot_schema}")).await;
        execute(
            &pool,
            &format!("CREATE TABLE {snapshot_schema}.target(id bigint)"),
        )
        .await;
        let mut snapshot = pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *snapshot)
            .await
            .unwrap();
        let original_oid: i64 = sqlx::query_scalar(&format!(
            "SELECT oid::bigint FROM pg_catalog.pg_class
              WHERE oid='{snapshot_schema}.target'::pg_catalog.regclass"
        ))
        .fetch_one(&mut *snapshot)
        .await
        .unwrap();
        execute(
            &pool,
            &format!("ALTER TABLE {snapshot_schema}.target RENAME TO target_old"),
        )
        .await;
        execute(
            &pool,
            &format!("CREATE TABLE {snapshot_schema}.target(id bigint)"),
        )
        .await;
        sqlx::query(&format!(
            "LOCK TABLE {snapshot_schema}.target IN ACCESS SHARE MODE"
        ))
        .execute(&mut *snapshot)
        .await
        .unwrap();
        let snapshot_oid: i64 = sqlx::query_scalar(&format!(
            "SELECT oid::bigint FROM pg_catalog.pg_class
              WHERE relnamespace='{snapshot_schema}'::pg_catalog.regnamespace
                AND relname='target'"
        ))
        .fetch_one(&mut *snapshot)
        .await
        .unwrap();
        let live_oid: i64 = sqlx::query_scalar(&format!(
            "SELECT '{snapshot_schema}.target'::pg_catalog.regclass::oid::bigint"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(snapshot_oid, original_oid);
        assert_ne!(snapshot_oid, live_oid);
        snapshot.rollback().await.unwrap();
        execute(&pool, &format!("DROP SCHEMA {snapshot_schema} CASCADE")).await;
        finish(database, pool).await;
    }

    #[tokio::test]
    async fn capability_rejects_catalog_and_data_mutations_pg() {
        let (database, pool) = database().await;
        assert_eq!(probe_schema(&pool).await, SchemaReason::Ready);

        for (hide, reason, restore) in [
            (
                "ALTER TABLE public.delivery_journal_events RENAME TO journal_hidden",
                SchemaReason::Relation,
                "ALTER TABLE public.journal_hidden RENAME TO delivery_journal_events",
            ),
            (
                "ALTER TABLE public._sqlx_migrations RENAME TO migrations_hidden",
                SchemaReason::Query,
                "ALTER TABLE public.migrations_hidden RENAME TO _sqlx_migrations",
            ),
        ] {
            execute(&pool, hide).await;
            assert_eq!(probe_schema(&pool).await, reason);
            execute(&pool, restore).await;
        }
        assert_eq!(probe_schema(&pool).await, SchemaReason::Ready);

        for version in
            crate::db::intake_delivery_required_migrations::INTAKE_DELIVERY_REQUIRED_MIGRATIONS
        {
            sqlx::query("UPDATE public._sqlx_migrations SET success=false WHERE version=$1")
                .bind(version)
                .execute(&pool)
                .await
                .unwrap();
            assert_eq!(probe_schema(&pool).await, SchemaReason::Migration);
            sqlx::query("UPDATE public._sqlx_migrations SET success=true WHERE version=$1")
                .bind(version)
                .execute(&pool)
                .await
                .unwrap();
        }

        let domain_schema = format!("domain_{}", Uuid::new_v4().simple());
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {domain_schema};
             CREATE DOMAIN {domain_schema}.timestamptz AS pg_catalog.timestamptz;
             ALTER TABLE public.intake_outbox ALTER COLUMN completed_at
               TYPE {domain_schema}.timestamptz USING completed_at::{domain_schema}.timestamptz"
        ))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(probe_schema(&pool).await, SchemaReason::Columns);
        execute(
            &pool,
            "ALTER TABLE public.intake_outbox ALTER COLUMN completed_at
             TYPE pg_catalog.timestamptz USING completed_at::pg_catalog.timestamptz",
        )
        .await;
        execute(&pool, &format!("DROP SCHEMA {domain_schema} CASCADE")).await;
        execute(
            &pool,
            "ALTER TABLE public.intake_outbox ALTER COLUMN dispatched_at SET DEFAULT now()",
        )
        .await;
        assert_eq!(probe_schema(&pool).await, SchemaReason::Columns);
        execute(
            &pool,
            "ALTER TABLE public.intake_outbox ALTER COLUMN dispatched_at DROP DEFAULT",
        )
        .await;

        sqlx::raw_sql(
            "ALTER TABLE public.intake_outbox
               DROP CONSTRAINT intake_outbox_dispatched_requires_clock;
             ALTER TABLE public.intake_outbox
               ADD CONSTRAINT intake_outbox_dispatched_requires_clock CHECK(true)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(probe_schema(&pool).await, SchemaReason::Constraint);
        sqlx::raw_sql(&format!(
            "ALTER TABLE public.intake_outbox
               DROP CONSTRAINT intake_outbox_dispatched_requires_clock;
             ALTER TABLE public.intake_outbox
               ADD CONSTRAINT intake_outbox_dispatched_requires_clock {} NOT VALID",
            INTAKE_CHECKS[0].1
        ))
        .execute(&pool)
        .await
        .unwrap();

        execute(
            &pool,
            "ALTER TABLE public.intake_outbox
             DROP CONSTRAINT intake_outbox_dispatched_requires_clock",
        )
        .await;
        let bad_clock: i64 = sqlx::query_scalar(
            "INSERT INTO public.intake_outbox(
               target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,
               request_owner_id,user_text,turn_kind,agent_id,status)
             VALUES('w','l','bad-clock','bad-clock','u','x','standard','a','dispatched')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(&format!(
            "ALTER TABLE public.intake_outbox
             ADD CONSTRAINT intake_outbox_dispatched_requires_clock {} NOT VALID",
            INTAKE_CHECKS[0].1
        ))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(probe_schema(&pool).await, SchemaReason::Constraint);
        sqlx::query("DELETE FROM public.intake_outbox WHERE id=$1")
            .bind(bad_clock)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(probe_schema(&pool).await, SchemaReason::Ready);

        let definitions: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.relname,pg_get_indexdef(c.oid)
               FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
              WHERE n.nspname='public' AND c.relname=ANY($1::text[])
              ORDER BY c.relname",
        )
        .bind([
            "idx_intake_outbox_stale_dispatched",
            "idx_delivery_journal_intake_binding",
            "delivery_journal_single_o_a_t",
            "delivery_journal_single_terminal",
            "delivery_journal_obligation_order",
        ])
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(definitions.len(), 5);
        let obligation_order = definitions
            .iter()
            .find(|(name, _)| name == "delivery_journal_obligation_order")
            .map(|(_, definition)| definition.clone())
            .unwrap();
        execute(&pool, "DROP INDEX public.delivery_journal_obligation_order").await;
        execute(
            &pool,
            "CREATE INDEX delivery_journal_obligation_order
               ON public.delivery_journal_events(obligation_id,event_seq)
             WHERE event_seq >= 0",
        )
        .await;
        assert_eq!(probe_schema(&pool).await, SchemaReason::Index);
        execute(&pool, "DROP INDEX public.delivery_journal_obligation_order").await;
        sqlx::raw_sql(&obligation_order)
            .execute(&pool)
            .await
            .unwrap();
        for (name, definition) in definitions {
            for bit in ["indisvalid", "indisready", "indislive"] {
                sqlx::query(&format!(
                    "UPDATE pg_catalog.pg_index SET {bit}=false
                      WHERE indexrelid='public.{name}'::regclass"
                ))
                .execute(&pool)
                .await
                .unwrap();
                assert_eq!(probe_schema(&pool).await, SchemaReason::Index);
                sqlx::query(&format!(
                    "UPDATE pg_catalog.pg_index SET {bit}=true
                      WHERE indexrelid='public.{name}'::regclass"
                ))
                .execute(&pool)
                .await
                .unwrap();
            }
            execute(&pool, &format!("DROP INDEX public.{name}")).await;
            sqlx::query(&format!(
                "CREATE INDEX {name} ON public.intake_outbox(id) WHERE status='pending'"
            ))
            .execute(&pool)
            .await
            .unwrap();
            assert_eq!(probe_schema(&pool).await, SchemaReason::Index);
            execute(&pool, &format!("DROP INDEX public.{name}")).await;
            sqlx::raw_sql(&definition).execute(&pool).await.unwrap();
        }
        assert_eq!(probe_schema(&pool).await, SchemaReason::Ready);

        let role = format!("intake_probe_{}", Uuid::new_v4().simple());
        sqlx::raw_sql(&format!(
            "CREATE ROLE {role} NOLOGIN;
             GRANT USAGE ON SCHEMA public TO {role};
             GRANT SELECT ON public._sqlx_migrations TO {role}"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let mut limited = pool.acquire().await.unwrap();
        sqlx::query(&format!("SET ROLE {role}"))
            .execute(&mut *limited)
            .await
            .unwrap();
        assert_eq!(
            probe_transaction(&mut limited).await.unwrap(),
            SchemaReason::Privilege
        );
        sqlx::query("RESET ROLE")
            .execute(&mut *limited)
            .await
            .unwrap();
        drop(limited);
        sqlx::raw_sql(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(probe_schema(&pool).await, SchemaReason::Ready);
        finish(database, pool).await;
    }
}

#[cfg(test)]
pub(in crate::services::discord) use cache::bootstrap_for_test;
