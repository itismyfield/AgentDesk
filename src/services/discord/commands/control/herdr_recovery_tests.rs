//! The Herdr recovery clear's discard settles the inputs it removes, as the plain clear does.
use super::*;
use crate::services::turn_orchestrator::{QueuePersistenceContext, v2_queue};

/// An origin the recovery discard removes gets its cancelled receipt and is owed no longer.
#[tokio::test(flavor = "current_thread")]
async fn the_recovery_discard_settles_each_removed_origin_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (provider, channel) = (ProviderKind::Claude, ChannelId::new(7_340_503));
    let persistence = QueuePersistenceContext::new(&provider, &shared.token_hash, None);
    let (path, namespace) = v2_queue(&persistence, channel).unwrap();
    let mailbox = shared.mailbox(channel);
    mailbox.v2_adopt(path, namespace).await.unwrap();
    let id = 8_000_000_000_000_340_531_u64;
    let original = serde_json::from_value(serde_json::json!({"author_id":1,"message_id":id,
        "source_message_ids":[id],"channel_id":channel.get(),"queued_generation":5,"text":"x"}))
    .unwrap();
    let context =
        serde_json::json!({"origin":"headless","metadata":{"human_input":{"origin_id":"G-h"}}});
    let admitted = mailbox.v2_admit_ticket_with_context(original, "t-h".into(), context);
    admitted.await.unwrap();

    let mut report = ClearReport::default();
    discard_waiting(
        &shared,
        &provider,
        channel,
        None,
        &BTreeSet::new(),
        &mut report,
    )
    .await;
    let sql = "SELECT turn_id, terminal_outcome FROM turns WHERE channel_id = $1";
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(sql)
        .bind(channel.get().to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
    let turn = format!("discord:{channel}:{id}");
    assert_eq!(rows, [(turn, Some("cancelled".to_string()))], "{report:?}");
    let owed = mailbox.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops()));
    assert!(owed.await.unwrap().is_empty());
    pool.close().await;
    db.drop().await;
}
