//! How a journaled non-owner start ends, and its settlement at the queue's turn boundary.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::decisions::EventKind;
use crate::db::decisions::test_support::{insert_event, row};
use crate::db::turns::TerminalOutcome;

const CHANNEL: u64 = 8_211_501;

async fn receipt(pool: &PgPool, event: Uuid, outcome: TerminalOutcome) {
    let ids = vec![format!("decision:{event}")];
    let done = crate::db::turns::TurnReceipt {
        turn_id: &format!("discord:{CHANNEL}:{}", event.as_u128() as u32),
        channel_id: &CHANNEL.to_string(),
        agent_id: Some("decision-agent"),
        provider: "claude",
        session_key: None,
        started_at: None,
        consumed_origin_ids: &ids,
        outcome,
        confirmed_at: chrono::Utc::now(),
    };
    crate::db::turns::record_turn_receipt(pool, &done)
        .await
        .unwrap();
}

/// A start that proved nothing was written waits again under the same event, a cleared one
/// fails, an unproven failure closes unknown, and a started one waits for its turn's receipt.
#[tokio::test(flavor = "current_thread")]
async fn decision_queue_start_ends_by_what_it_proved_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let approved = (EventKind::Selection, CHANNEL, 7);
    let cases = [
        (Ended::Started, "may_have_sent", None),
        (Ended::WaitsAgain, "normal_wait", None),
        (Ended::Cleared, "failed", Some("cleared")),
        (Ended::Unknown, "unknown_closed", Some("queue_start_failed")),
    ];
    for (ended, state, detail) in cases {
        let event = insert_event(&pool, approved, "text", "may_have_sent").await;
        end(Some(&pool), Some(event), ended).await;
        let read = row(&pool, event).await;
        assert_eq!((read.0.as_str(), read.4.as_deref()), (state, detail));
    }
    // At the queue's next turn boundary only a completed receipt delivers a started event.
    let completed = insert_event(&pool, approved, "text", "may_have_sent").await;
    let incomplete = insert_event(&pool, approved, "text", "may_have_sent").await;
    let running = insert_event(&pool, approved, "text", "may_have_sent").await;
    receipt(&pool, completed, TerminalOutcome::Completed).await;
    receipt(&pool, incomplete, TerminalOutcome::Failed).await;
    note_open();
    settle_started(Some(&pool), CHANNEL).await;
    assert_eq!(row(&pool, completed).await.0, "delivered");
    assert_eq!(row(&pool, incomplete).await.0, "unknown_closed");
    assert_eq!(row(&pool, running).await.0, "may_have_sent");
    pool.close().await;
    db.drop().await;
}
