//! A forced cancel on a channel whose queue the V2 owner holds.
use super::*;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem, v2_queue};

fn item(id: u64, channel: u64) -> PendingQueueItem {
    serde_json::from_value(
        serde_json::json!({"author_id":7,"message_id":id,"channel_id":channel,
        "source_message_ids":[id],"queued_generation":1,"text":format!("queued {id}")}),
    )
    .unwrap()
}

/// The forced purge removes what the owner queued, so nothing the cancel meant to drop runs
/// after it: the owner has no front left to start.
#[tokio::test(flavor = "current_thread")]
async fn a_forced_cancel_purges_the_v2_queue_and_nothing_reruns() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (provider, channel) = (ProviderKind::Claude, ChannelId::new(7_272_562));
    let handle = ChannelMailboxRegistry::default().handle(channel);
    let persistence = QueuePersistenceContext::new(&provider, "", None);
    let (path, namespace) = v2_queue(&persistence, channel).unwrap();
    handle.v2_adopt(path, namespace).await.unwrap();
    for id in [8_000_000_000_000_562_001, 8_000_000_000_000_562_002] {
        let queued = item(id, channel.get());
        let enqueued = handle.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            owner
                .enqueue_fixture(&queued)
                .map_err(|error| format!("{error:?}"))
        });
        assert_eq!(enqueued.await, Ok(true));
    }
    let target = TurnLifecycleTarget {
        provider: Some(provider),
        channel_id: Some(channel),
        tmux_name: String::new(),
    };
    let purged = force_purge_channel_mailbox(None, &target, None).await;
    assert_eq!(purged.map(|counts| counts.drained), Some(2));
    let front = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.front()));
    assert!(
        front.await.unwrap().is_none(),
        "a purged input never starts"
    );
}

/// A forced cancel commits a cancelled receipt for each removed origin at its removal instant,
/// the input-result lookup reads it, and the owner owes nothing once the database holds it.
#[tokio::test(flavor = "current_thread")]
async fn a_forced_cancel_settles_each_removed_origin_at_its_removal_instant_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (provider, channel) = (ProviderKind::Claude, ChannelId::new(7_340_401));
    let handle = ChannelMailboxRegistry::default().handle(channel);
    let persistence = QueuePersistenceContext::new(&provider, "", None);
    let (path, namespace) = v2_queue(&persistence, channel).unwrap();
    handle.v2_adopt(path, namespace).await.unwrap();
    let id = 8_000_000_000_000_340_401;
    let context = serde_json::json!({"metadata":{"human_input":{"origin_id":"G-forced"}}});
    let admitted =
        handle.v2_admit_ticket_with_context(item(id, channel.get()), "t-forced".into(), context);
    admitted.await.unwrap();
    let target = TurnLifecycleTarget {
        provider: Some(provider),
        channel_id: Some(channel),
        tmux_name: String::new(),
    };
    let before = chrono::Utc::now();
    let purged = force_purge_and_settle(None, &target, None, Some(&pool)).await;
    let after = chrono::Utc::now();
    assert_eq!(purged.map(|counts| counts.drained), Some(1));

    type Row = (
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    );
    let turn_id = format!("discord:{channel}:{id}");
    let sql = "SELECT terminal_outcome, started_at, finished_at, terminal_confirmed_at FROM turns WHERE turn_id = $1";
    let row: Row = sqlx::query_as(sql)
        .bind(&turn_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (outcome, started, finished, confirmed) = row.clone();
    assert_eq!(outcome.as_deref(), Some("cancelled"));
    assert!(
        started == finished && finished == confirmed,
        "one removal instant: {row:?}"
    );
    assert!(
        before.timestamp_millis() <= confirmed.timestamp_millis(),
        "{confirmed} after {before}"
    );
    assert!(confirmed <= after, "{confirmed} before {after}");
    let owed = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops()));
    assert!(
        owed.await.unwrap().is_empty(),
        "a committed receipt is no longer owed"
    );

    let channels = [crate::services::agents::input_result::OriginChannel {
        provider: "claude".into(),
        channel_id: channel.get(),
    }];
    let found =
        crate::services::agents::input_result::lookup(&pool, "agent", "G-forced", &channels);
    let Some(crate::services::agents::input_result::InputResult::Terminal(receipt)) =
        found.await.unwrap()
    else {
        panic!("the forced cancel's receipt answers the origin");
    };
    assert_eq!(
        (receipt.turn_id, receipt.terminal_outcome),
        (turn_id, "cancelled".into())
    );
    assert_eq!(receipt.terminal_confirmed_at, confirmed);
    pool.close().await;
    db.drop().await;
}
