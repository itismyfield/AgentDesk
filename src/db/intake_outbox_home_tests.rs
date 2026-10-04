//! The claim fence: a delegated channel's row is claimed only by the holder at the row's home epoch.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::{
    ChannelHome, HomeError, HomeWrite, adopt, begin_reclaim, delegate, finish_reclaim,
    finish_release, remove_reclaimed,
};

const C: &str = "4380201";
const OTHER: &str = "4380202";
const GW: &str = "gw-4380";
const MINI: &str = "mini-4380";

fn applied(write: Result<HomeWrite<ChannelHome>, HomeError>) -> ChannelHome {
    match write.expect("home write") {
        HomeWrite::Applied(home) => home,
        HomeWrite::Stale => panic!("expected the home write to apply"),
    }
}

async fn seed(pool: &PgPool, channel: &str, msg: &str, home_epoch: Option<i64>) -> i64 {
    let payload = InsertPendingPayload {
        target_instance_id: MINI.into(),
        forwarded_by_instance_id: GW.into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: channel.into(),
        user_msg_id: msg.into(),
        request_owner_id: "100".into(),
        request_owner_name: None,
        user_text: "hello".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: None,
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "agent-x".into(),
        provider: "claude".into(),
        home_epoch,
    };
    insert_pending(pool, &payload, 1, None)
        .await
        .expect("insert")
}

/// The id each claim branch takes: with nothing held, and with an unrelated channel held.
async fn claims(pool: &PgPool) -> [Option<i64>; 2] {
    let mut taken = [None, None];
    for (slot, held) in taken.iter_mut().zip([vec![], vec!["9".to_string()]]) {
        let row = claim_pending_for_target_except(pool, MINI, "claude", "o", &held)
            .await
            .expect("claim");
        if let Some(row) = &row {
            assert!(return_claimed_to_pending(pool, row.id, "o").await.unwrap());
        }
        *slot = row.map(|row| row.id);
    }
    taken
}

/// Whether the confirming update alone would claim `id` now.
async fn confirms(pool: &PgPool, id: i64) -> bool {
    let mut tx = pool.begin().await.unwrap();
    let row = confirm_claim(&mut tx, id, "o").await.expect("confirm");
    tx.rollback().await.unwrap();
    row.is_some()
}

async fn clear(pool: &PgPool) {
    sqlx::query("DELETE FROM intake_outbox")
        .execute(pool)
        .await
        .unwrap();
}

/// A worker-owned home of `C` held by MINI, after a full release from the gateway.
async fn delegated_to_mini(pool: &PgPool) -> i64 {
    let home = applied(delegate(pool, C, "claude", GW, MINI).await);
    let home = applied(finish_release(pool, C, GW, home.epoch).await);
    applied(adopt(pool, C, MINI, home.epoch).await).epoch
}

#[tokio::test(flavor = "current_thread")]
async fn a_delegated_row_is_claimed_only_by_its_holder_at_its_home_epoch_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;

    // No home row: a gateway-rule row is claimed by both branches as before.
    let plain = seed(&pool, C, "1", None).await;
    assert_eq!(claims(&pool).await, [Some(plain), Some(plain)]);
    let epoch = delegated_to_mini(&pool).await;
    assert_eq!(claims(&pool).await, [None, None], "home row, no epoch");
    assert!(!confirms(&pool, plain).await, "the update refuses it too");
    clear(&pool).await;

    let routed = seed(&pool, C, "2", Some(epoch)).await;
    let elsewhere = seed(&pool, OTHER, "3", None).await;
    assert_eq!(claims(&pool).await, [Some(routed), Some(routed)]);
    sqlx::query("UPDATE o_channel_homes SET provider = 'codex' WHERE channel_id = $1")
        .bind(C)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(claims(&pool).await, [Some(elsewhere), Some(elsewhere)]);
    assert!(!confirms(&pool, routed).await, "provider differs");
    sqlx::query("UPDATE o_channel_homes SET provider = 'claude' WHERE channel_id = $1")
        .bind(C)
        .execute(&pool)
        .await
        .unwrap();
    assert!(confirms(&pool, routed).await);

    // The holder starts draining: the row stays pending and unclaimed.
    let draining = applied(begin_reclaim(&pool, C, epoch, GW).await);
    assert_eq!(claims(&pool).await, [Some(elsewhere), Some(elsewhere)]);
    assert!(!confirms(&pool, routed).await, "reclaiming");

    // A later lifecycle of the same holder never revives the old epoch's row.
    let left = applied(finish_reclaim(&pool, C, MINI, draining.epoch).await);
    let removed = remove_reclaimed(&pool, C, GW, left.epoch).await.unwrap();
    assert_eq!(removed, HomeWrite::Applied(()));
    let next = delegated_to_mini(&pool).await;
    assert!(next > epoch, "{next} after {epoch}");
    assert_eq!(claims(&pool).await, [Some(elsewhere), Some(elsewhere)]);
    assert!(!confirms(&pool, routed).await, "older lifecycle epoch");
    // A router that read the old lifecycle inserts only now.
    let delete = "DELETE FROM intake_outbox WHERE channel_id = $1";
    sqlx::query(delete).bind(C).execute(&pool).await.unwrap();
    let late = seed(&pool, C, "4", Some(epoch)).await;
    assert_eq!(claims(&pool).await, [Some(elsewhere), Some(elsewhere)]);
    assert!(!confirms(&pool, late).await, "late insert at the old epoch");
    sqlx::query(delete).bind(C).execute(&pool).await.unwrap();
    let current = seed(&pool, C, "5", Some(next)).await;
    assert!(confirms(&pool, current).await, "the current epoch's row");

    pool.close().await;
    fixture.drop().await;
}
