//! The dead-letter row a plan-driven loss writes, read back from PostgreSQL.

use super::soft_terminal_direct_send_authority_tests::{
    FRAME_END, FRAME_START, LOST_BODY, SENT_PREFIX, UNPROCESSED_TAIL, admitted_losses_under_root,
};
use super::*;
use crate::db::relay_dead_letter::{
    ClaimedDeadLetter, KIND_TERMINAL_NO_DELIVERY_OWNER, claim_pending_redeliveries,
};

/// A channel of its own for the process-global admission counter.
const ROW_CHANNEL: u64 = 1_479_671_298_497_183_838;

/// Claims the rows the way the redelivery sweep does; the seam's INSERT is detached, so poll.
async fn terminal_rows(pool: &sqlx::PgPool) -> Vec<ClaimedDeadLetter> {
    for _ in 0..200 {
        let rows = claim_pending_redeliveries(pool, KIND_TERMINAL_NO_DELIVERY_OWNER, 0, 3600, 10)
            .await
            .expect("claim terminal dead-letter rows");
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
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_terminal_lost_body",
        "terminal relay plan lost body",
    )
    .await;
    let pool = pg_db.connect_and_migrate().await;
    // An unprocessed tail splits the parser offset from the consumed end, so the
    // row can tell which of the two the call site passed as `jsonl_end`.
    let current_offset = FRAME_END + UNPROCESSED_TAIL.len() as u64;
    assert_ne!(
        current_offset, FRAME_END,
        "the fixture must read past the frame"
    );

    assert_eq!(
        admitted_losses_under_root(root.path(), ROW_CHANNEL, false, Some(pool.clone())).await,
        1
    );
    let rows = terminal_rows(&pool).await;
    assert_eq!(rows.len(), 1, "one plan pass writes one row: {rows:?}");
    let row = &rows[0];
    assert_eq!(row.content, LOST_BODY, "the row must carry the unsent body");
    assert_eq!(row.channel_id, ROW_CHANNEL.to_string());
    let reason = &row.reason;
    let fields: Vec<&str> = reason.split_whitespace().collect();
    for field in [
        format!("jsonl_end={FRAME_END}"),
        format!("current_offset={current_offset}"),
        format!("jsonl_start={FRAME_START}"),
        format!("response_sent_offset={SENT_PREFIX}"),
        format!("full_response_len={}", SENT_PREFIX + LOST_BODY.len()),
        format!(
            "denial={}",
            SoftTerminalAuthorityDenial::NoInflightRow.as_str()
        ),
        "provider=claude".to_string(),
    ] {
        assert!(
            fields.contains(&field.as_str()),
            "the reason must carry `{field}`: {reason}"
        );
    }

    pg_db.drop().await;
}
