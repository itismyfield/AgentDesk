//! A removed input's receipt is owed in the commit that removes it and stays owed until the
//! database holds it, across every write boundary and a restart; each removing entry settles it.
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::agents::input_result::{InputResult, OriginChannel, lookup};
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
use crate::services::discord::settle_v2_drops;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::human_frame::history::v2::Registry;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, OwedDrop, PendingQueueItem,
    QueuePersistenceContext, v2_queue,
};
use chrono::{DateTime, Utc};
use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::json;

fn original(channel: u64, id: u64) -> PendingQueueItem {
    serde_json::from_value(
        json!({"author_id":1,"message_id":id,"source_message_ids":[id],
        "channel_id":channel,"queued_generation":5,"text":format!("input {id}")}),
    )
    .unwrap()
}

async fn admit(handle: &ChannelMailboxHandle, channel: u64, id: u64, origin: &str) {
    let context = json!({"origin":"headless","metadata":{"human_input":{"origin_id":origin}}});
    let admitted =
        handle.v2_admit_ticket_with_context(original(channel, id), format!("t-{id}"), context);
    admitted.await.unwrap();
}

async fn owed(handle: &ChannelMailboxHandle) -> Vec<OwedDrop> {
    let read = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops()));
    read.await.unwrap()
}

async fn queued(handle: &ChannelMailboxHandle) -> usize {
    let read =
        handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.json()?["items"].clone()));
    read.await.unwrap().as_array().map_or(0, Vec::len)
}

/// Each receipt row for `channel`: message, outcome, started, finished, confirmed.
type Receipt = (
    String,
    Option<String>,
    DateTime<Utc>,
    DateTime<Utc>,
    DateTime<Utc>,
);
async fn receipts(pool: &sqlx::PgPool, channel: u64) -> Vec<Receipt> {
    let sql = "SELECT turn_id, terminal_outcome, started_at, finished_at, terminal_confirmed_at
               FROM turns WHERE channel_id = $1 ORDER BY turn_id";
    let rows = sqlx::query_as(sql)
        .bind(channel.to_string())
        .fetch_all(pool);
    rows.await.unwrap()
}

/// The receipt each owed drop stands for, at its own removal instant.
fn expected(channel: u64, owed: &[OwedDrop], outcome: &str) -> Vec<Receipt> {
    let mut rows: Vec<Receipt> = (owed.iter())
        .map(|o| {
            let at = DateTime::from_timestamp_millis(o.at_ms as i64).unwrap();
            (
                format!("discord:{channel}:{}", o.message_id),
                Some(outcome.into()),
                at,
                at,
                at,
            )
        })
        .collect();
    rows.sort();
    rows
}

async fn answer(pool: &sqlx::PgPool, channel: u64, origin: &str) -> Option<InputResult> {
    let channels = [OriginChannel {
        provider: "claude".into(),
        channel_id: channel,
    }];
    lookup(pool, "owed-agent", origin, &channels).await.unwrap()
}

fn terminal(result: Option<InputResult>) -> (String, String, i64) {
    let Some(InputResult::Terminal(receipt)) = result else {
        panic!("a terminal answer, got {result:?}");
    };
    let at = receipt.terminal_confirmed_at.timestamp_millis();
    (receipt.turn_id, receipt.terminal_outcome, at)
}

/// Owed is read without removing it, only receipts the database holds are acked, and a write
/// that fails at any boundary leaves the drop owed for the next settle, boot recovery included.
#[tokio::test(flavor = "current_thread")]
async fn a_removal_stays_owed_across_every_commit_boundary_until_its_receipt_lands_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 7_340_501;
    let (a, b) = (8_000_000_000_000_340_511, 8_000_000_000_000_340_512);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "owed", None);
    let (path, namespace) = v2_queue(&persistence, ChannelId::new(channel)).unwrap();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(channel));
    handle
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    admit(&handle, channel, a, "G-a").await;
    admit(&handle, channel, b, "G-b").await;

    // A publish refused before the rename removes nothing and owes nothing.
    let refused = {
        let _watch = Watch::new(
            &path,
            Plan {
                fail: vec![Step::Write],
                ..Default::default()
            },
        );
        handle.clear(persistence.clone()).await
    };
    assert!(refused.persistence.failed.contains(&"queue_publish"));
    assert_eq!((queued(&handle).await, owed(&handle).await.len()), (2, 0));
    assert!(matches!(
        answer(&pool, channel, "G-a").await,
        Some(InputResult::Accepted { .. })
    ));

    // A committed removal whose process ends before any settle: the restart holds both.
    assert_eq!(
        handle
            .clear(persistence.clone())
            .await
            .discarded_message_ids
            .len(),
        2
    );
    let restarted = ChannelMailboxRegistry::default();
    let handle = restarted.handle(ChannelId::new(channel));
    handle
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    let drops = owed(&handle).await;
    assert_eq!((queued(&handle).await, drops.len()), (0, 2));
    let owed_answer = terminal(answer(&pool, channel, "G-a").await);
    let at_a = drops.iter().find(|o| o.message_id == a).unwrap().at_ms as i64;
    assert_eq!(
        owed_answer,
        (format!("discord:{channel}:{a}"), "cancelled".into(), at_a)
    );

    // The receipt write fails: nothing is acked and the lookup still answers from the owed drop.
    let closed =
        sqlx::postgres::PgPoolOptions::new().connect_lazy_with((*pool.connect_options()).clone());
    closed.close().await;
    settle_v2_drops(Some(&closed), &handle, "claude", ChannelId::new(channel)).await;
    assert_eq!(owed(&handle).await, drops);
    assert!(receipts(&pool, channel).await.is_empty());
    assert_eq!(terminal(answer(&pool, channel, "G-a").await), owed_answer);

    // The receipts land but the ack's publish fails: they stay, and so does the owed list.
    {
        let _watch = Watch::new(
            &path,
            Plan {
                fail: vec![Step::Write],
                ..Default::default()
            },
        );
        settle_v2_drops(Some(&pool), &handle, "claude", ChannelId::new(channel)).await;
    }
    assert_eq!(owed(&handle).await, drops);
    assert_eq!(
        receipts(&pool, channel).await,
        expected(channel, &drops, "cancelled")
    );

    // Boot recovery settles again: the same receipts, one per input, and nothing owed after.
    let history = tempfile::TempDir::new().unwrap();
    let registry = Registry::at(history.path().join("history.json"));
    let now = (Utc::now().timestamp_millis(), "claude", channel);
    let recovered = super::recover(
        &handle,
        (path, namespace),
        registry,
        Some(&pool),
        (now.1, now.2, now.0 as u64),
    );
    assert_eq!(recovered.await, Ok(0));
    assert!(owed(&handle).await.is_empty());
    assert_eq!(
        receipts(&pool, channel).await,
        expected(channel, &drops, "cancelled")
    );
    assert_eq!(terminal(answer(&pool, channel, "G-a").await), owed_answer);
    pool.close().await;
    db.drop().await;
}

/// The Discord cancel of one queued input and the channel clear each commit the cancelled
/// receipts of what they removed, and leave nothing owed.
#[tokio::test(flavor = "current_thread")]
async fn the_cancel_and_clear_entries_settle_what_they_remove_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (provider, channel) = (ProviderKind::Claude, ChannelId::new(7_340_502));
    let persistence =
        crate::services::discord::queue_persistence_context(&shared, &provider, channel);
    let (path, namespace) = v2_queue(&persistence, channel).unwrap();
    let handle = shared.mailbox(channel);
    handle.v2_adopt(path, namespace).await.unwrap();
    let (c, d) = (8_000_000_000_000_340_521, 8_000_000_000_000_340_522);
    admit(&handle, channel.get(), c, "G-c").await;
    admit(&handle, channel.get(), d, "G-d").await;

    let cancel = crate::services::discord::queue_io::mailbox_cancel_queued_primary_message;
    assert!(
        cancel(&shared, &provider, channel, MessageId::new(c))
            .await
            .is_some()
    );
    let rows = receipts(&pool, channel.get()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, format!("discord:{channel}:{c}"));
    assert!(owed(&handle).await.is_empty());

    let clear = crate::services::discord::mailbox_clear_channel;
    assert_eq!(
        clear(&shared, &provider, channel)
            .await
            .discarded_message_ids
            .len(),
        1
    );
    let rows = receipts(&pool, channel.get()).await;
    let ids: Vec<(&str, Option<&str>)> = rows
        .iter()
        .map(|r| (r.0.as_str(), r.1.as_deref()))
        .collect();
    let (c_id, d_id) = (
        format!("discord:{channel}:{c}"),
        format!("discord:{channel}:{d}"),
    );
    assert_eq!(
        ids,
        [
            (c_id.as_str(), Some("cancelled")),
            (d_id.as_str(), Some("cancelled"))
        ]
    );
    assert!(rows.iter().all(|r| r.2 == r.3 && r.3 == r.4), "{rows:?}");
    assert!(owed(&handle).await.is_empty());
    pool.close().await;
    db.drop().await;
}

/// A restart whose notice post fails still settles the receipt a removal owes, then reports the
/// notice failure; the notice stays owed for the next drain.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_notice_post_still_settles_what_a_removal_owes_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 7_340_503;
    let (head, removed) = (8_000_000_000_000_340_531, 8_000_000_000_000_340_532);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "owed", None);
    let (path, namespace) = v2_queue(&persistence, ChannelId::new(channel)).unwrap();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(channel));
    handle
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    admit(&handle, channel, head, "G-head").await;
    admit(&handle, channel, removed, "G-removed").await;
    let cancel = handle.cancel_queued_primary_message(MessageId::new(removed), persistence);
    assert!(cancel.await.removed.is_some());
    // The head's attempt lapses unconfirmed, so its owner owes a notice and no receipt.
    let attempt = json!({"nonce":"lapsed","provider":"claude",
        "target":{"host":"tmux","owner":"owed-lapsed","root":"/test","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let lapsed = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
        let key = owner.reserve_unit(attempt, |item| Some((String::new(), item.text.clone())))?;
        drop(owner.arm(&key, 1)?);
        owner.tick(40_000)
    });
    lapsed.await.unwrap();
    let drops = owed(&handle).await;
    assert_eq!(drops.len(), 1);
    sqlx::query("ALTER TABLE message_outbox RENAME TO message_outbox_unavailable")
        .execute(&pool)
        .await
        .unwrap();

    let history = tempfile::TempDir::new().unwrap();
    let registry = Registry::at(history.path().join("history.json"));
    let now = Utc::now().timestamp_millis() as u64;
    let recovered = super::recover(
        &handle,
        (path, namespace),
        registry,
        Some(&pool),
        ("claude", channel, now),
    );
    assert!(recovered.await.is_err(), "the notice failure is reported");
    assert!(owed(&handle).await.is_empty());
    assert_eq!(
        receipts(&pool, channel).await,
        expected(channel, &drops, "cancelled")
    );
    let untold = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.json()?));
    let untold = untold.await.unwrap()["retained_inputs"][0]["notice"]["enqueued"].clone();
    assert_eq!(untold, json!(false));
    pool.close().await;
    db.drop().await;
}
