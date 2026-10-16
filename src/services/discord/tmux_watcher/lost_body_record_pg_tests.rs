//! The dead-letter row a plan-driven loss writes, read back from PostgreSQL.

use super::*;

/// A channel of its own for the process-global admission counter.
const ROW_CHANNEL: u64 = 1_479_671_298_497_183_838;

/// The seam's INSERT runs on a detached task, so poll until it lands.
async fn terminal_rows(pool: &sqlx::PgPool) -> Vec<(String, String, String)> {
    for _ in 0..200 {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT channel_id, content, reason FROM relay_dead_letter WHERE kind = $1",
        )
        .bind(crate::db::relay_dead_letter::KIND_TERMINAL_NO_DELIVERY_OWNER)
        .fetch_all(pool)
        .await
        .expect("read terminal dead-letter rows");
        if !rows.is_empty() {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Vec::new()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_production_call_site_hands_the_seam_the_lost_body_5941() {
    // The plan itself feeds the seam, so the row must hold the unsent body and
    // the consumed JSONL end, not merely some non-empty body and some offset.
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_terminal_lost_body",
        "terminal relay plan lost body",
    )
    .await;
    let pool = pg_db.connect_and_migrate().await;

    assert_eq!(
        admitted_losses_over_one_plan_pass(ROW_CHANNEL, false, Some(pool.clone())).await,
        1
    );
    let rows = terminal_rows(&pool).await;
    assert_eq!(rows.len(), 1, "one plan pass writes one row: {rows:?}");
    let (channel_id, content, reason) = &rows[0];
    assert_eq!(content, LOST_BODY, "the row must carry the unsent body");
    assert_eq!(channel_id, &ROW_CHANNEL.to_string());
    let fields: Vec<&str> = reason.split_whitespace().collect();
    for field in [
        format!("jsonl_end={FRAME_END}"),
        format!("jsonl_start={FRAME_START}"),
        format!("response_sent_offset={SENT_PREFIX}"),
        format!("full_response_len={}", SENT_PREFIX + LOST_BODY.len()),
        format!("denial={}", SoftTerminalAuthorityDenial::NoInflightRow.as_str()),
        "provider=claude".to_string(),
    ] {
        assert!(
            fields.contains(&field.as_str()),
            "the reason must carry `{field}`: {reason}"
        );
    }

    pg_db.drop().await;
}
