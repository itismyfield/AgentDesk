//! Gateway intake while the replay lookup is down: dormant builds keep the legacy hold, and an
//! active deferral never reports custody it does not have.

use super::{CHANNEL_ID, RelayE2eHarness, discord_mock::user_message};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::router::replay_test_activation as test_activation;
use crate::services::discord::{advance_last_message_checkpoint, runtime_store};
use poise::serenity_prelude::{ChannelId, MessageId};

const EARLIER_MESSAGE_ID: u64 = 940_487_400_006_001;
const MESSAGE_ID: u64 = 940_487_400_006_002;

async fn harness_with_replay_lookup_down() -> (RelayE2eHarness, TestPostgresDb) {
    let mut database = None;
    let harness = RelayE2eHarness::start_bound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let pool = harness.shared.pg_pool.clone().expect("PG-backed runtime");
    // Both replay lookups fail; queue custody and the rest of the schema stay intact.
    sqlx::raw_sql(
        "CREATE OR REPLACE FUNCTION replay_sources_blocked(
           p_provider TEXT, p_channel TEXT, p_sources TEXT[], p_except BIGINT DEFAULT NULL)
           RETURNS BOOLEAN LANGUAGE plpgsql AS $$
           BEGIN RAISE EXCEPTION 'replay lookup unavailable'; END $$;
         ALTER TABLE intake_outbox RENAME COLUMN replay_preserved TO replay_preserved_unavailable;",
    )
    .execute(&pool)
    .await
    .expect("break the replay lookups");
    (harness, database.expect("isolated PostgreSQL database"))
}

fn checkpoints(harness: &RelayE2eHarness) -> (Option<u64>, Option<u64>) {
    let memory = harness
        .shared
        .last_message_ids
        .get(&ChannelId::new(CHANNEL_ID))
        .map(|id| *id);
    let path = runtime_store::last_message_root()
        .expect("checkpoint root")
        .join("claude")
        .join(format!("{CHANNEL_ID}.txt"));
    let disk = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok());
    (memory, disk)
}

async fn queued_ids(harness: &RelayE2eHarness) -> Vec<u64> {
    let snapshot = harness
        .shared
        .mailbox(ChannelId::new(CHANNEL_ID))
        .snapshot()
        .await;
    snapshot
        .intervention_queue
        .iter()
        .map(|item| item.message_id.get())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dormant_replay_lookup_failure_keeps_the_legacy_hold_at_the_gateway_pg() {
    let (harness, database) = harness_with_replay_lookup_down().await;

    harness
        .deliver_user_message(MESSAGE_ID, "a request while the replay lookup is down")
        .await
        .expect("the held message is handled");

    assert_eq!(
        queued_ids(&harness).await,
        Vec::<u64>::new(),
        "a dormant build holds the input as before; nothing waits in custody"
    );
    assert_eq!(checkpoints(&harness), (Some(MESSAGE_ID), Some(MESSAGE_ID)));
    assert!(harness.provider_inputs().is_empty());
    drop(harness);
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_deferral_refused_by_the_queue_keeps_the_checkpoint_pg() {
    let (harness, database) = harness_with_replay_lookup_down().await;
    let _active = test_activation::activate(CHANNEL_ID);
    advance_last_message_checkpoint(
        &harness.shared,
        &crate::services::provider::ProviderKind::Claude,
        ChannelId::new(CHANNEL_ID),
        MessageId::new(EARLIER_MESSAGE_ID),
    );
    // A directory where the durable queue file belongs makes the queue refuse custody.
    let queue_file = harness
        .root
        .path()
        .join("runtime/discord_pending_queue/claude")
        .join(&harness.shared.token_hash)
        .join(format!("{CHANNEL_ID}.json"));
    std::fs::create_dir_all(&queue_file).expect("block the durable queue file");

    let refused = harness
        .deliver_user_message(MESSAGE_ID, "deferred while the queue cannot persist")
        .await;

    assert!(
        refused.is_err(),
        "refused custody is not a deferred success"
    );
    assert_eq!(
        checkpoints(&harness),
        (Some(EARLIER_MESSAGE_ID), Some(EARLIER_MESSAGE_ID)),
        "the unheld message stays behind the checkpoint for recollection"
    );
    assert!(harness.provider_inputs().is_empty());
    drop(harness);
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_deferral_of_an_attachment_is_refused_not_kept_text_only_pg() {
    let (harness, database) = harness_with_replay_lookup_down().await;
    let _active = test_activation::activate(CHANNEL_ID);
    let mut message = user_message(MESSAGE_ID, "please read the attached file");
    message.attachments = vec![
        serde_json::from_value(serde_json::json!({
            "id": "940487400006003",
            "filename": "report.txt",
            "size": 12,
            "url": "https://cdn.discordapp.com/attachments/1/2/report.txt",
            "proxy_url": "https://media.discordapp.net/attachments/1/2/report.txt",
        }))
        .expect("Discord attachment"),
    ];

    let refused = harness.spawn_message(message).await.expect("intake task");

    assert!(
        refused.is_err(),
        "a deferral that drops the file is not a success"
    );
    assert_eq!(
        queued_ids(&harness).await,
        Vec::<u64>::new(),
        "no text-only item reaches the queue drain"
    );
    assert_eq!(checkpoints(&harness), (None, None));
    assert!(harness.provider_inputs().is_empty());
    drop(harness);
    database.drop().await;
}
