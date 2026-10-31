//! A batch held because some of its sources were already submitted asks only for its new part; a
//! proven-unsent original still asks for a plain resend.
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::queue_io::input_owner::notice::drain;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use poise::serenity_prelude::ChannelId;
use serde_json::json;

const CHANNEL: u64 = 7_272_611;
const BASE: u64 = 1_490_000_000_000_008_000;
const OPAQUE: &str = "필요한 새 부분을 따로 보내 주세요";
const RESEND: &str = "필요하면 같은 내용을 다시 보내 주세요";

fn item(n: u64, sources: &[u64], reply: Option<&str>) -> PendingQueueItem {
    let ids: Vec<u64> = sources.iter().map(|s| BASE + s).collect();
    serde_json::from_value(json!({"author_id":7,"message_id":BASE + n,"channel_id":CHANNEL,
        "source_message_ids":ids,"queued_generation":5,"text":format!("input {n}"),"reply_context":reply}))
    .unwrap()
}

async fn posts(pool: &sqlx::PgPool, channel: u64) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM message_outbox WHERE target = $1 ORDER BY id")
        .bind(format!("channel:{channel}"))
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_batch_over_a_submitted_source_asks_only_for_its_new_part_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(CHANNEL));
    handle
        .v2_adopt(
            root.join("claude")
                .join("opaque")
                .join(format!("{CHANNEL}.json")),
            "opaque".into(),
        )
        .await
        .unwrap();
    handle
        .v2_admit_ticket(item(1, &[1], None), "opaque-1".into())
        .await
        .unwrap();
    let attempt = json!({"nonce":"q1","provider":"claude",
        "target":{"host":"tmux","owner":"opaque-q1","root":"/test","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
            let key =
                owner.reserve_unit(attempt, |item| Some((String::new(), item.text.clone())))?;
            drop(owner.arm(&key, 1)?);
            owner.settle(&key, true, 2)
        })
        .await
        .unwrap();
    handle
        .v2_admit_ticket(item(2, &[1, 2], Some("quoted")), "opaque-2".into())
        .await
        .unwrap();
    let retained = handle
        .v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.json()?["retained_inputs"].clone()))
        .await
        .unwrap();
    assert_eq!(retained[0]["disposition"], "opaque_unsent", "{retained}");

    assert_eq!(drain(&pool, &handle, "claude", CHANNEL).await.unwrap(), 1);
    let posted = posts(&pool, CHANNEL).await;
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert!(posted[0].contains(OPAQUE), "{posted:?}");
    assert!(!posted[0].contains(RESEND), "{posted:?}");

    // The same owed entry proven unsent keeps the resend wording.
    let mut known = retained[0].clone();
    known["disposition"] = json!("known_not_sent");
    super::enqueue(&pool, "claude", CHANNEL + 1, "opaque", &known)
        .await
        .unwrap();
    let posted = posts(&pool, CHANNEL + 1).await;
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert!(posted[0].contains(RESEND), "{posted:?}");
    assert!(!posted[0].contains(OPAQUE), "{posted:?}");
}
