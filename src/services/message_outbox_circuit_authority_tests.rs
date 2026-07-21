use super::message_outbox::OutboxMessage;
use super::message_outbox_circuit_authority::*;
use sqlx::{PgPool, Row};

async fn setup(
    name: &str,
) -> Option<(
    crate::dispatch::test_support::DispatchPostgresTestDb,
    PgPool,
)> {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(name, name).await?;
    let pool = db.connect_and_migrate().await;
    Some((db, pool))
}

async fn owner(pool: &PgPool, channel: &str, node: &str) {
    sqlx::query("INSERT INTO intake_session_owners(provider,raw_channel_id,owner_instance_id,generation,status) VALUES('discord',$1,$2,7,'active')")
        .bind(channel).bind(node).execute(pool).await.unwrap();
}

async fn reserve(
    pool: &PgPool,
    channel: &str,
    episode: &str,
    generation: i64,
    expected: Option<i64>,
) -> CircuitCoordinate {
    match reserve_next_authority(
        pool, "discord", channel, "node-a", 7, episode, 10, generation, expected,
    )
    .await
    .unwrap()
    {
        AuthorityReservation::Reserved(value) => value,
        other => panic!("unexpected reservation: {other:?}"),
    }
}

fn message<'a>(target: &'a str, reason: &'a str) -> OutboxMessage<'a> {
    OutboxMessage {
        target,
        content: "body",
        bot: "notify",
        source: "system",
        reason_code: Some(reason),
        session_key: Some(target),
    }
}

#[tokio::test]
async fn episode_transition_allocates_next_epoch_and_stale_cas_fails_pg() {
    let Some((db, pool)) = setup("circuit_allocate_episode").await else {
        return;
    };
    owner(&pool, "501", "node-a").await;
    let first = reserve(&pool, "501", "e1", 1, None).await;
    assert_eq!(first.authority_epoch, 1);
    assert_eq!(reserve(&pool, "501", "e1", 1, Some(1)).await, first);
    let second = reserve(&pool, "501", "e2", 1, Some(1)).await;
    assert_eq!(second.authority_epoch, 2);
    assert!(matches!(
        reserve_next_authority(&pool, "discord", "501", "node-a", 7, "e3", 10, 1, Some(1))
            .await
            .unwrap(),
        AuthorityReservation::Stale
    ));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn same_episode_rollbacks_fail_but_monotonic_and_episode_reset_advance_pg() {
    let Some((db, pool)) = setup("circuit_coordinate_order").await else {
        return;
    };
    owner(&pool, "507", "node-a").await;
    let mut current =
        match reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e0", 1, 1, None)
            .await
            .unwrap()
        {
            AuthorityReservation::Reserved(value) => value,
            other => panic!("{other:?}"),
        };
    for episode in ["e0-2", "e0-3", "e0-4", "e0-5", "e0-6"] {
        current = match reserve_next_authority(
            &pool,
            "discord",
            "507",
            "node-a",
            7,
            episode,
            1,
            1,
            Some(current.authority_epoch),
        )
        .await
        .unwrap()
        {
            AuthorityReservation::Reserved(value) => value,
            other => panic!("{other:?}"),
        };
    }
    current =
        match reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 100, 4, Some(6))
            .await
            .unwrap()
        {
            AuthorityReservation::Reserved(value) => value,
            other => panic!("{other:?}"),
        };
    assert_eq!(current.authority_epoch, 7);
    assert_eq!(
        reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 100, 4, Some(7))
            .await
            .unwrap(),
        AuthorityReservation::Reserved(current.clone())
    );
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 101, 4, Some(7))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "baseline-only advance"
    );
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 100, 5, Some(7))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "open-only advance"
    );
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 99, 5, Some(7))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "baseline-only rollback"
    );
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 101, 3, Some(7))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "open-only rollback"
    );
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 140, 4, Some(7))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "baseline advance with equal generation"
    );
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 140, 3, Some(7))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "baseline advance with lower generation"
    );
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 100, 7, Some(7))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "generation advance with equal baseline"
    );
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 99, 7, Some(7))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "generation advance with lower baseline"
    );
    let increment =
        match reserve_next_authority(&pool, "discord", "507", "node-a", 7, "e1", 140, 7, Some(7))
            .await
            .unwrap()
        {
            AuthorityReservation::Reserved(value) => value,
            other => panic!("{other:?}"),
        };
    assert_eq!(increment.authority_epoch, 8);
    sqlx::query("UPDATE intake_session_owners SET generation=8 WHERE provider='discord' AND raw_channel_id='507'").execute(&pool).await.unwrap();
    assert!(
        matches!(
            reserve_next_authority(&pool, "discord", "507", "node-a", 8, "e1", 100, 4, Some(8))
                .await
                .unwrap(),
            AuthorityReservation::Stale
        ),
        "owner transfer cannot bypass coordinate ordering"
    );
    let reset =
        match reserve_next_authority(&pool, "discord", "507", "node-a", 8, "e2", 1, 1, Some(8))
            .await
            .unwrap()
        {
            AuthorityReservation::Reserved(value) => value,
            other => panic!("{other:?}"),
        };
    assert_eq!(reset.authority_epoch, 9);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn maximum_authority_epoch_fails_closed_pg() {
    let Some((db, pool)) = setup("circuit_epoch_overflow").await else {
        return;
    };
    owner(&pool, "508", "node-a").await;
    sqlx::query("INSERT INTO message_outbox_circuit_authority(provider,channel_id,owner_instance_id,owner_generation,episode_key,baseline_relay_offset,open_generation,authority_epoch) VALUES('discord','508','node-a',7,'e1',100,4,$1)").bind(i64::MAX).execute(&pool).await.unwrap();
    assert!(matches!(
        reserve_next_authority(
            &pool,
            "discord",
            "508",
            "node-a",
            7,
            "e1",
            101,
            5,
            Some(i64::MAX)
        )
        .await
        .unwrap(),
        AuthorityReservation::Stale
    ));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn vouch_then_new_epoch_reopens_and_old_activation_is_stale_pg() {
    let Some((db, pool)) = setup("circuit_vouch_reopen").await else {
        return;
    };
    owner(&pool, "502", "node-a").await;
    let first = reserve(&pool, "502", "e1", 1, None).await;
    let target = "channel:502";
    let first_id = match stage_held(&pool, message(target, "same"), &first, 300)
        .await
        .unwrap()
    {
        StageHeldOutcome::Staged { id } => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        revoke_on_fresh_vouch(&pool, &first, "live").await.unwrap(),
        FreshVouchRevoke::Revoked
    );
    assert_eq!(
        activate_fenced(&pool, first_id, &first).await.unwrap(),
        CircuitActivation::Stale
    );
    let second =
        match reserve_next_authority(&pool, "discord", "502", "node-a", 7, "e1", 11, 2, Some(1))
            .await
            .unwrap()
        {
            AuthorityReservation::Reserved(value) => value,
            other => panic!("{other:?}"),
        };
    assert_eq!(second.authority_epoch, 2);
    let second_id = match stage_held(&pool, message(target, "same"), &second, 300)
        .await
        .unwrap()
    {
        StageHeldOutcome::Staged { id } => id,
        other => panic!("{other:?}"),
    };
    assert_ne!(first_id, second_id);
    assert_eq!(
        activate_fenced(&pool, second_id, &second).await.unwrap(),
        CircuitActivation::Activated
    );
    assert_eq!(
        activate_fenced(&pool, second_id, &second).await.unwrap(),
        CircuitActivation::AlreadyActivated
    );
    sqlx::query("UPDATE message_outbox SET status='processing' WHERE id=$1")
        .bind(second_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        activate_fenced(&pool, second_id, &second).await.unwrap(),
        CircuitActivation::Stale
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn exact_stage_is_idempotent_and_mismatched_coordinate_conflicts_pg() {
    let Some((db, pool)) = setup("circuit_stage_exact").await else {
        return;
    };
    owner(&pool, "503", "node-a").await;
    let c = reserve(&pool, "503", "e1", 1, None).await;
    let target = "channel:503";
    let id = match stage_held(&pool, message(target, "identity"), &c, 300)
        .await
        .unwrap()
    {
        StageHeldOutcome::Staged { id } => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        stage_held(&pool, message(target, "identity"), &c, 300)
            .await
            .unwrap(),
        StageHeldOutcome::Idempotent { id }
    );
    assert_eq!(
        stage_held(&pool, message(target, "identity"), &c, 301)
            .await
            .unwrap(),
        StageHeldOutcome::Conflict
    );
    let c2 = reserve(&pool, "503", "e2", 1, Some(1)).await;
    assert_eq!(
        stage_held(&pool, message(target, "identity"), &c2, 300)
            .await
            .unwrap(),
        StageHeldOutcome::Conflict
    );
    assert!(
        !super::message_outbox::activate_staged_outbox_pg(&pool, id)
            .await
            .unwrap()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn non_circuit_collision_conflicts_and_expired_held_is_replaced_pg() {
    let Some((db, pool)) = setup("circuit_collision_expired").await else {
        return;
    };
    owner(&pool, "504", "node-a").await;
    let c = reserve(&pool, "504", "e1", 1, None).await;
    let target = "channel:504";
    let key = super::message_outbox::dedupe_key_for_message(
        target,
        "body",
        Some("collision"),
        Some(target),
    )
    .unwrap();
    sqlx::query("INSERT INTO message_outbox(target,content,bot,source,status,reason_code,session_key,dedupe_key) VALUES($1,'body','notify','system','held','collision',$1,$2)").bind(target).bind(&key).execute(&pool).await.unwrap();
    assert_eq!(
        stage_held(&pool, message(target, "collision"), &c, 300)
            .await
            .unwrap(),
        StageHeldOutcome::Conflict
    );
    sqlx::query("DELETE FROM message_outbox WHERE dedupe_key=$1")
        .bind(&key)
        .execute(&pool)
        .await
        .unwrap();
    let old = match stage_held(&pool, message(target, "expired"), &c, 1)
        .await
        .unwrap()
    {
        StageHeldOutcome::Staged { id } => id,
        other => panic!("{other:?}"),
    };
    sqlx::query(
        "UPDATE message_outbox SET dedupe_expires_at=NOW()-INTERVAL '1 second' WHERE id=$1",
    )
    .bind(old)
    .execute(&pool)
    .await
    .unwrap();
    let new = match stage_held(&pool, message(target, "expired"), &c, 300)
        .await
        .unwrap()
    {
        StageHeldOutcome::Staged { id } => id,
        other => panic!("{other:?}"),
    };
    assert_ne!(old, new);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn non_owner_reserve_and_stage_leave_zero_rows_pg() {
    let Some((db, pool)) = setup("circuit_nonowner_zero").await else {
        return;
    };
    owner(&pool, "505", "node-b").await;
    assert!(matches!(
        reserve_next_authority(&pool, "discord", "505", "node-a", 7, "e1", 10, 1, None)
            .await
            .unwrap(),
        AuthorityReservation::NotOwner
    ));
    let fake = CircuitCoordinate {
        provider: "discord".into(),
        channel_id: "505".into(),
        owner_instance_id: "node-a".into(),
        owner_generation: 7,
        episode_key: "e1".into(),
        baseline_relay_offset: 10,
        open_generation: 1,
        authority_epoch: 1,
    };
    assert_eq!(
        stage_held(&pool, message("channel:505", "nonowner"), &fake, 300)
            .await
            .unwrap(),
        StageHeldOutcome::NotOwner
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE target='channel:505'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn resume_activation_maps_deliverable_terminal_and_unknown_states_pg() {
    let Some((db, pool)) = setup("circuit_resume_statuses").await else {
        return;
    };
    owner(&pool, "509", "node-a").await;
    let coordinate = reserve(&pool, "509", "e1", 1, None).await;
    let id = match stage_held(&pool, message("channel:509", "resume"), &coordinate, 300)
        .await
        .unwrap()
    {
        StageHeldOutcome::Staged { id } => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        activate_fenced_by_id(&pool, id).await.unwrap(),
        ResumeActivation::Activated
    );
    for status in ["pending", "processing", "sent", "delivered"] {
        sqlx::query("UPDATE message_outbox SET status=$2 WHERE id=$1")
            .bind(id)
            .bind(status)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            activate_fenced_by_id(&pool, id).await.unwrap(),
            ResumeActivation::AlreadyDeliverable
        );
    }
    for (status, expected) in [
        ("failed", ResumeActivation::Terminal),
        ("cancelled", ResumeActivation::RevokedOrFenced),
        ("unexpected_status", ResumeActivation::Unknown),
    ] {
        sqlx::query(
            "UPDATE message_outbox
             SET status=$2,
                 cancelled_at=CASE WHEN $2='cancelled' THEN NOW() ELSE cancelled_at END
             WHERE id=$1",
        )
        .bind(id)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(activate_fenced_by_id(&pool, id).await.unwrap(), expected);
    }
    assert_eq!(
        activate_fenced_by_id(&pool, id + 100_000).await.unwrap(),
        ResumeActivation::Missing
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn activation_then_vouch_cancels_pending_and_releases_dedupe_pg() {
    let Some((db, pool)) = setup("circuit_pending_vouch").await else {
        return;
    };
    owner(&pool, "506", "node-a").await;
    let c = reserve(&pool, "506", "e1", 1, None).await;
    let target = "channel:506";
    let id = match stage_held(&pool, message(target, "release"), &c, 300)
        .await
        .unwrap()
    {
        StageHeldOutcome::Staged { id } => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        activate_fenced(&pool, id, &c).await.unwrap(),
        CircuitActivation::Activated
    );
    assert_eq!(
        revoke_on_fresh_vouch(&pool, &c, "live").await.unwrap(),
        FreshVouchRevoke::Revoked
    );
    let row = sqlx::query("SELECT status,dedupe_key FROM message_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("status"), "cancelled");
    assert!(row.get::<Option<String>, _>("dedupe_key").is_none());
    pool.close().await;
    db.drop().await;
}

// #4615 S3b: worker delivery fence — re-validates a claimed `processing` row's
// circuit authority immediately before the Discord send.

/// Drive a staged+activated circuit row to `processing` under `owner`'s lease,
/// mirroring `claim_pending_message_outbox_batch_pg` (status/claim_owner/claimed_at).
async fn claim_as(pool: &PgPool, id: i64, owner: &str) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar(
        "UPDATE message_outbox SET status='processing', claim_owner=$2, claimed_at=NOW()
          WHERE id=$1 RETURNING claimed_at",
    )
    .bind(id)
    .bind(owner)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// reserve → stage_held → activate_fenced → claim, returning the processing row
/// id, its live coordinate, and the claim's `claimed_at`.
async fn processing_circuit_row(
    pool: &PgPool,
    channel: &str,
    owner: &str,
) -> (i64, CircuitCoordinate, chrono::DateTime<chrono::Utc>) {
    let c = reserve(pool, channel, "e1", 1, None).await;
    let target = format!("channel:{channel}");
    let id = match stage_held(pool, message(&target, "fence"), &c, 300)
        .await
        .unwrap()
    {
        StageHeldOutcome::Staged { id } => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        activate_fenced(pool, id, &c).await.unwrap(),
        CircuitActivation::Activated
    );
    let claimed_at = claim_as(pool, id, owner).await;
    (id, c, claimed_at)
}

#[tokio::test]
async fn fence_revoked_authority_cancels_processing_row_pg() {
    let Some((db, pool)) = setup("fence_revoked_authority").await else {
        return;
    };
    owner(&pool, "601", "node-a").await;
    let (id, c, claimed_at) = processing_circuit_row(&pool, "601", "w1").await;
    // A fresh vouch revokes authority but only cancels held/pending rows — this
    // already-processing row escapes it. The fence must catch it.
    assert_eq!(
        revoke_on_fresh_vouch(&pool, &c, "live").await.unwrap(),
        FreshVouchRevoke::Revoked
    );
    assert_eq!(
        fence_claimed_delivery(&pool, id, "w1", claimed_at)
            .await
            .unwrap(),
        DeliveryFenceOutcome::Fenced
    );
    let row = sqlx::query(
        "SELECT status,cancel_reason,dedupe_key,cancelled_at,delivery_fence_checked_at
           FROM message_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("status"), "cancelled");
    assert_eq!(
        row.get::<Option<String>, _>("cancel_reason").as_deref(),
        Some(DELIVERY_FENCE_CANCEL_REASON)
    );
    assert!(row.get::<Option<String>, _>("dedupe_key").is_none());
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("cancelled_at")
            .is_some()
    );
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("delivery_fence_checked_at")
            .is_some(),
        "fenced row must stamp delivery_fence_checked_at"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn fence_superseded_epoch_cancels_processing_row_pg() {
    let Some((db, pool)) = setup("fence_superseded_epoch").await else {
        return;
    };
    owner(&pool, "602", "node-a").await;
    let (id, _c, claimed_at) = processing_circuit_row(&pool, "602", "w1").await;
    // A newer episode reserves epoch 2, superseding the row's stamped epoch 1.
    let second = reserve(&pool, "602", "e2", 1, Some(1)).await;
    assert_eq!(second.authority_epoch, 2);
    assert_eq!(
        fence_claimed_delivery(&pool, id, "w1", claimed_at)
            .await
            .unwrap(),
        DeliveryFenceOutcome::Fenced
    );
    let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn fence_live_authority_clears_and_stamps_pg() {
    let Some((db, pool)) = setup("fence_live_authority").await else {
        return;
    };
    owner(&pool, "603", "node-a").await;
    let (id, _c, claimed_at) = processing_circuit_row(&pool, "603", "w1").await;
    assert_eq!(
        fence_claimed_delivery(&pool, id, "w1", claimed_at)
            .await
            .unwrap(),
        DeliveryFenceOutcome::Cleared
    );
    let row = sqlx::query(
        "SELECT status,claim_owner,delivery_fence_checked_at FROM message_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row.get::<String, _>("status"),
        "processing",
        "a live row keeps its lease and proceeds to delivery"
    );
    assert_eq!(
        row.get::<Option<String>, _>("claim_owner").as_deref(),
        Some("w1")
    );
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("delivery_fence_checked_at")
            .is_some(),
        "a cleared row still stamps delivery_fence_checked_at"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn fence_stale_lease_is_noop_pg() {
    let Some((db, pool)) = setup("fence_stale_lease").await else {
        return;
    };
    owner(&pool, "604", "node-a").await;
    let (id, c, stale_claimed_at) = processing_circuit_row(&pool, "604", "w1").await;
    // Another worker steals the lease (stale-claim reclaim).
    let fresh_claimed_at = claim_as(&pool, id, "w2").await;
    // Supersede the authority so, were the stale worker allowed to act, it would
    // fence the row — proving the lease guard (not authority state) is what stops it.
    assert_eq!(
        revoke_on_fresh_vouch(&pool, &c, "live").await.unwrap(),
        FreshVouchRevoke::Revoked
    );
    assert_eq!(
        fence_claimed_delivery(&pool, id, "w1", stale_claimed_at)
            .await
            .unwrap(),
        DeliveryFenceOutcome::LeaseLost
    );
    let row = sqlx::query(
        "SELECT status,claim_owner,claimed_at,delivery_fence_checked_at
           FROM message_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("status"), "processing");
    assert_eq!(
        row.get::<Option<String>, _>("claim_owner").as_deref(),
        Some("w2")
    );
    assert_eq!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("claimed_at"),
        Some(fresh_claimed_at)
    );
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("delivery_fence_checked_at")
            .is_none(),
        "a stale worker must not stamp or mutate the row"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn fence_non_circuit_row_clears_pg() {
    let Some((db, pool)) = setup("fence_non_circuit").await else {
        return;
    };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO message_outbox(target,content,bot,source,status)
         VALUES('channel:900','body','notify','system','pending') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let claimed_at = claim_as(&pool, id, "w1").await;
    assert_eq!(
        fence_claimed_delivery(&pool, id, "w1", claimed_at)
            .await
            .unwrap(),
        DeliveryFenceOutcome::Cleared
    );
    let row =
        sqlx::query("SELECT status,delivery_fence_checked_at FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("status"), "processing");
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("delivery_fence_checked_at")
            .is_some()
    );
    pool.close().await;
    db.drop().await;
}
