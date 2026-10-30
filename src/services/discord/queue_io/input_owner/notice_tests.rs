//! Retained originals owe one notice each, worded by what the owner knows; a local submit owes none,
//! a restart posts nothing twice, and a failed history projection holds no notice.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_input::human_frame::history::v2::Registry;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use poise::serenity_prelude::ChannelId;

const CHANNEL: u64 = 7_272_601;
const BASE: u64 = 1_490_000_000_000_007_000;

fn item(n: u64, sources: &[u64], reply: Option<&str>) -> PendingQueueItem {
    let ids: Vec<u64> = sources.iter().map(|s| BASE + s).collect();
    serde_json::from_value(json!({"author_id":7,"message_id":BASE + n,"channel_id":CHANNEL,
        "source_message_ids":ids,"queued_generation":5,"text":format!("input {n}"),"reply_context":reply}))
    .unwrap()
}

async fn admit(handle: &ChannelMailboxHandle, original: PendingQueueItem, ticket: &str) {
    handle
        .v2_admit_ticket(original, ticket.into())
        .await
        .unwrap();
}

/// Reserves the queue front under `nonce` and arms it; `Some(true)` submits it, `Some(false)` lets
/// its deadline lapse, `None` leaves it armed and unsettled.
async fn attempt(handle: &ChannelMailboxHandle, nonce: &str, submitted: Option<bool>) {
    let attempt = json!({"nonce":nonce,"provider":"claude",
        "target":{"host":"tmux","owner":format!("notice-{nonce}"),"root":"/test","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
            let key =
                owner.reserve_unit(attempt, |item| Some((String::new(), item.text.clone())))?;
            drop(owner.arm(&key, 1)?);
            match submitted {
                Some(true) => owner.settle(&key, true, 2),
                Some(false) => owner.tick(40_000),
                None => Ok(()),
            }
        })
        .await
        .unwrap();
}

async fn posts(pool: &sqlx::PgPool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT content, COALESCE(reason_code, '') FROM message_outbox WHERE target = $1 ORDER BY id")
        .bind(format!("channel:{CHANNEL}"))
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn owed_notices_post_once_by_kind_and_a_local_submit_owes_none_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = root
        .join("claude")
        .join("notice")
        .join(format!("{CHANNEL}.json"));
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(CHANNEL));
    handle
        .v2_adopt(file.clone(), "notice".into())
        .await
        .unwrap();
    admit(&handle, item(1, &[1], None), "notice-1").await;
    attempt(&handle, "unconfirmed", Some(false)).await;
    admit(&handle, item(2, &[1, 2], Some("quoted")), "notice-2").await;
    admit(&handle, item(3, &[3], None), "notice-3").await;
    attempt(&handle, "submitted", Some(true)).await;

    assert_eq!(drain(&pool, &handle, "claude", CHANNEL).await.unwrap(), 2);
    let posted = posts(&pool).await;
    assert_eq!(
        posted.len(),
        2,
        "one per retained original, none for the local submit: {posted:?}"
    );
    assert!(
        posted[0].0.contains("전달 여부를 확인하지 못해"),
        "{posted:?}"
    );
    assert!(
        posted[1].0.contains("필요한 새 부분을 따로 보내 주세요"),
        "{posted:?}"
    );
    assert_eq!(
        drain(&pool, &handle, "claude", CHANNEL).await.unwrap(),
        1,
        "the attemptless entry stays owed"
    );
    assert_eq!(posts(&pool).await, posted, "but the outbox keeps one post");

    let restarted = ChannelMailboxRegistry::default();
    let handle = restarted.handle(ChannelId::new(CHANNEL));
    std::fs::write(dir.path().join("blocker"), "a file, not a directory").unwrap();
    let broken = Registry::at(dir.path().join("blocker").join("history.json"));
    let recovered = super::super::recovery::recover(
        &handle,
        (file, "notice".into()),
        broken,
        Some(&pool),
        ("claude", CHANNEL, 10_000),
    );
    assert_eq!(
        recovered.await,
        Ok(1),
        "a failed projection holds no notice"
    );
    assert_eq!(posts(&pool).await, posted, "a restart posts nothing twice");
}

/// A restart settles an armed head whose window passed while nothing ticked: the original is
/// retained and its unconfirmed notice posts once, from `recover` alone.
#[tokio::test(flavor = "current_thread")]
async fn a_restart_retains_an_expired_unsettled_head_and_posts_its_notice_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = root
        .join("claude")
        .join("expired")
        .join(format!("{CHANNEL}.json"));
    let before = ChannelMailboxRegistry::default();
    let handle = before.handle(ChannelId::new(CHANNEL));
    handle
        .v2_adopt(file.clone(), "expired".into())
        .await
        .unwrap();
    admit(&handle, item(1, &[1], None), "expired-1").await;
    attempt(&handle, "expired", None).await;
    drop((handle, before));

    let restarted = ChannelMailboxRegistry::default();
    let handle = restarted.handle(ChannelId::new(CHANNEL));
    let history = Registry::at(dir.path().join("history.json"));
    let recovered = super::super::recovery::recover(
        &handle,
        (file, "expired".into()),
        history,
        Some(&pool),
        ("claude", CHANNEL, 40_000),
    );
    assert_eq!(recovered.await, Ok(1));
    let snapshot = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    let snapshot = snapshot.await.unwrap();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(
        snapshot["retained_inputs"].as_array().map(Vec::len),
        Some(1),
        "{snapshot}"
    );
    let posted = posts(&pool).await;
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert!(
        posted[0].0.contains("전달 여부를 확인하지 못해"),
        "{posted:?}"
    );
    pool.close().await;
    db.drop().await;
}

/// The backstop's one wakeup is the moment `tick` first retains the head: nothing is due for an
/// absent or reserved head, the armed head is due at its deadline and not a millisecond before.
#[tokio::test(flavor = "current_thread")]
async fn the_settle_due_time_is_when_tick_first_retains_the_head() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = root
        .join("claude")
        .join("due")
        .join(format!("{CHANNEL}.json"));
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(CHANNEL));
    handle.v2_adopt(file, "due".into()).await.unwrap();
    let due = || super::super::recovery::settle_due(&handle);
    assert_eq!(due().await, Ok(None), "no head");
    admit(&handle, item(1, &[1], None), "due-1").await;
    let attempt = json!({"nonce":"due","provider":"claude",
        "target":{"host":"tmux","owner":"notice-due","root":"/test","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let key = handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
            owner.reserve_unit(attempt, |item| Some((String::new(), item.text.clone())))
        })
        .await
        .unwrap();
    assert_eq!(due().await, Ok(None), "a reserved head is not due");
    let armed = key.clone();
    let tick = |now: u64| {
        handle.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            owner.tick(now)?;
            Ok(owner.snapshot.head_injection.is_some())
        })
    };
    handle
        .v2_request(move |slot| Ok(drop(slot.as_mut().ok_or("no owner")?.arm(&armed, 1)?)))
        .await
        .unwrap();
    let at = due().await.unwrap().expect("an armed head is due");
    assert!(at > 1, "{at}");
    assert_eq!(tick(at - 1).await, Ok(true), "not retained before its time");
    assert_eq!(tick(at).await, Ok(false), "retained at its time");
    assert_eq!(due().await, Ok(None));
}
