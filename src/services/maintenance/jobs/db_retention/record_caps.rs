//! Caps the postmortem record tables (transcript archive, supervisor audit log, SLO
//! trend rows) so they keep a bounded window instead of growing forever.

use super::*;

// Runs after `retain_session_transcripts`, so a transcript archived in this pass is
// judged by the archive window, not the hot one.
pub(super) async fn retain(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    for (table, days) in [
        (
            "session_transcripts_archive",
            TRANSCRIPT_ARCHIVE_RETENTION_DAYS,
        ),
        ("runtime_decisions", RUNTIME_DECISIONS_RETENTION_DAYS),
        ("slo_aggregates", SLO_AGGREGATES_RETENTION_DAYS),
    ] {
        delete_aged_by_created_at(pool, dry_run, report, table, days).await?;
    }
    Ok(())
}

// `table` comes only from the fixed list above. Rows with NULL `created_at` are kept.
async fn delete_aged_by_created_at(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
    table: &'static str,
    days: i32,
) -> Result<()> {
    const AGED: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL";
    if dry_run {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*)::BIGINT FROM {table} WHERE {AGED}"
        )))
        .bind(days)
        .fetch_one(pool)
        .await?;
        report.push(TableReport {
            table_name: table,
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let del = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} WHERE {AGED}"
    )))
    .bind(days)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: table,
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ids(pool: &PgPool, sql: &'static str) -> Vec<String> {
        sqlx::query_scalar(sql)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|err| panic!("read survivors `{sql}`: {err}"))
    }

    async fn seed(pool: &PgPool, sql: &'static str, key: &str, age_days: i32) {
        sqlx::query(sql)
            .bind(key)
            .bind(age_days)
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("seed {key}: {err}"));
    }

    /// Each capped table keeps rows inside its window and loses rows past it; a
    /// transcript archived in the same pass stays in the archive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_caps_record_tables_on_created_at_window() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_record_caps",
            "db_retention record table caps",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        for (key, age) in [("rd-91", 91), ("rd-89", 89)] {
            seed(
                &pool,
                "INSERT INTO runtime_decisions \
                     (signal, evidence_json, chosen_action, actor, created_at) \
                 VALUES ($1, '{}'::jsonb, 'Escalate', 'supervisor', \
                         NOW() - ($2::INT || ' days')::INTERVAL)",
                key,
                age,
            )
            .await;
        }
        for (key, age) in [("slo-91", 91), ("slo-89", 89)] {
            seed(
                &pool,
                "INSERT INTO slo_aggregates \
                     (window_start_ms, window_end_ms, metric, value, created_at) \
                 VALUES (0, 1, $1, 1.0, NOW() - ($2::INT || ' days')::INTERVAL)",
                key,
                age,
            )
            .await;
        }
        // 179 days is past the hot window but inside the archive window. The ids stay
        // clear of the `session_transcripts` sequence the hot row draws from.
        for (id, key, age) in [(1_000_001_i64, "arc-181", 181), (1_000_002, "arc-179", 179)] {
            sqlx::query(
                "INSERT INTO session_transcripts_archive (id, turn_id, created_at) \
                 VALUES ($3, $1, NOW() - ($2::INT || ' days')::INTERVAL)",
            )
            .bind(key)
            .bind(age)
            .bind(id)
            .execute(&pool)
            .await
            .expect("seed archive row");
        }
        seed(
            &pool,
            "INSERT INTO session_transcripts (turn_id, created_at) \
             VALUES ($1, NOW() - ($2::INT || ' days')::INTERVAL)",
            "hot-100",
            100,
        )
        .await;

        let survivors = |pool: PgPool| async move {
            (
                ids(
                    &pool,
                    "SELECT signal FROM runtime_decisions ORDER BY signal",
                )
                .await,
                ids(&pool, "SELECT metric FROM slo_aggregates ORDER BY metric").await,
                ids(
                    &pool,
                    "SELECT turn_id FROM session_transcripts_archive ORDER BY turn_id",
                )
                .await,
            )
        };
        let seeded = survivors(pool.clone()).await;

        let dry = db_retention_job(&pool, true).await.expect("dry-run pass");
        for table in [
            "runtime_decisions",
            "slo_aggregates",
            "session_transcripts_archive",
        ] {
            assert_eq!(
                dry.get(table, "delete_would").map(|t| t.rows_affected),
                Some(1),
                "{table}: dry-run counts the one aged row"
            );
        }
        assert_eq!(
            survivors(pool.clone()).await,
            seeded,
            "dry-run deletes nothing"
        );

        let report = db_retention_job(&pool, false).await.expect("live pass");
        for table in [
            "runtime_decisions",
            "slo_aggregates",
            "session_transcripts_archive",
        ] {
            assert_eq!(
                report.get(table, "delete").map(|t| t.rows_affected),
                Some(1),
                "{table}: live pass deletes the one aged row"
            );
        }
        assert_eq!(
            survivors(pool.clone()).await,
            (
                vec!["rd-89".to_string()],
                vec!["slo-89".to_string()],
                vec!["arc-179".to_string(), "hot-100".to_string()],
            ),
            "only rows past each window are deleted; the freshly archived transcript stays"
        );

        pool.close().await;
        db.drop().await;
    }
}
