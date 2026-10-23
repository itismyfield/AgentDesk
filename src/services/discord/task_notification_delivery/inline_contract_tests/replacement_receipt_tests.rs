use super::*;

#[tokio::test]
async fn public_missing_monitor_rebinds_persistent_receipt_pg() {
    let (database, mut pool) = database("public missing Monitor durable receipt replacement").await;
    for (task, source) in [
        ("public-receipt-sourced", Some("public-receipt-source")),
        ("public-receipt-unsourced", None),
    ] {
        let event = event(
            task,
            source,
            None,
            "Monitor event: \"coord events\"",
            "current event\nsecond line",
        );
        let transport = Transport::default();
        let original = deliver(&pool, &transport, &event).await;
        let before = snapshot(&pool, &event).await;
        let replacement = replace_confirmed_missing_card(
            Some(&pool),
            &clients("claude"),
            &transport,
            &event,
            original.message_id,
        )
        .await
        .expect("a confirmed missing Monitor card must receive one canonical replacement");
        assert_ne!(replacement.message_id, original.message_id);
        assert_eq!(replacement.bot_key, "provider:claude");
        assert_eq!(transport.counts(), (2, 0));
        assert_inline(&transport.latest(), "claude", "📡", "current event");
        let after = snapshot(&pool, &event).await;
        assert_eq!(after.count, before.count);
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(after.message, Some(replacement.message_id as i64));

        pool.close().await;
        pool = database.connect_and_migrate().await;
        let replay_transport = Transport::default();
        let replay = deliver(&pool, &replay_transport, &event).await;
        assert_eq!(
            replay.message_id, replacement.message_id,
            "the permanent Monitor receipt must never return the confirmed missing old card"
        );
        assert_eq!(replay.bot_key, "provider:claude");
        assert_eq!(replay_transport.counts(), (0, 0));
        assert_eq!(snapshot(&pool, &event).await, after);
    }
    database.drop().await;
}

#[tokio::test]
async fn recovered_pending_source_free_monitor_retains_old_event_receipt_pg() {
    let (database, pool) =
        database("pending source-free Monitor recovery retains old payload").await;
    let first = monitor(None, 1);
    let second = monitor(None, 2);
    let bots = clients("claude");
    let transport = Transport::default();
    let claim = store::claim_event(
        Some(&pool),
        &first,
        "provider:claude",
        StoreIntent::Observation,
        None,
    )
    .await
    .expect("the original Monitor must own its durable posting row");
    let CardClaim::Owned(claim) = claim else {
        panic!("a new isolated Monitor fixture must own its first claim: {claim:?}");
    };
    store::begin_card_post(Some(&pool), &claim)
        .await
        .expect("persist the original Monitor POST boundary");
    let accepted_message = transport
        .post_card(
            bots.by_key("provider:claude").unwrap(),
            CHANNEL,
            &claim.rendered_content,
            &claim.discord_nonce,
        )
        .await
        .expect("fake Discord accepted the original nonce before the process stopped");
    let released = sqlx::query(
        "UPDATE task_notification_card_state SET lease_owner=NULL,lease_expires_at=NULL
         WHERE channel_id=$1 AND provider=$2 AND session_key=$3 AND event_key=$4 AND lease_owner=$5",
    )
    .bind(CHANNEL as i64)
    .bind(&first.scope.provider)
    .bind(&first.scope.session_key)
    .bind(&first.scope.event_key)
    .bind(&claim.lease_owner)
    .execute(&pool)
    .await
    .expect("expire only this lane-owned interrupted fixture lease")
    .rows_affected();
    assert_eq!(released, 1);

    let latest = deliver(&pool, &transport, &second).await;
    assert_eq!(latest.message_id, accepted_message);
    assert_eq!(transport.counts(), (2, 1));
    assert_inline(&transport.latest(), "claude", "📡", "event-2");
    let after = snapshot(&pool, &second).await;
    assert_eq!(after.count, 2);
    pool.close().await;
    let pool = database.connect_and_migrate().await;
    let replay_transport = Transport::default();
    let replay = deliver(&pool, &replay_transport, &first).await;
    assert_eq!(replay.message_id, accepted_message);
    assert_eq!(
        replay_transport.counts(),
        (0, 0),
        "a recovered old event must not revert the latest Monitor or count it again"
    );
    assert_eq!(snapshot(&pool, &second).await, after);
    database.drop().await;
}

#[tokio::test]
async fn modern_completion_title_containing_task_survives_stream_promotion_pg() {
    let (database, pool) = database("modern Task completion title survives footer restore").await;
    for (task, summary, title) in [
        (
            "modern-title-task-word",
            "Agent \"Task analyzer\" finished",
            "✅ **Agent \"Task analyzer\" finished**",
        ),
        ("modern-title-default", "", "✅ **Task completed**"),
    ] {
        let full = event(
            task,
            None,
            Some("completed"),
            summary,
            "full result first\nfull result second",
        );
        record_footer_only(Some(&pool), &full)
            .await
            .expect("record the modern body card before the stream context loses its result");
        let stored = snapshot(&pool, &full).await;
        assert!(stored.content.starts_with(title), "{stored:?}");
        let context = TaskNotificationContext::from_stream_json(
            &serde_json::json!({
                "type": "system",
                "subtype": "task_notification",
                "task_notification_kind": "background",
                "task_id": task,
                "status": "completed",
                "summary": summary,
            }),
            &StreamLineState::new(),
        )
        .expect("the real stream parser must retain the body-free task identity");
        let promoted = context.to_event(CHANNEL, "claude", "inline-contract-session");
        let transport = Transport::default();
        ensure_card(
            Some(&pool),
            &clients("claude"),
            &transport,
            &promoted,
            EnsureIntent::Promotion,
        )
        .await
        .expect("the real promotion entrypoint must preserve the modern completion title");
        assert_eq!(transport.counts(), (1, 0));
        assert_notify_body_card(
            &transport.latest(),
            &[title, "> full result first", "> full result second"],
        );
    }
    database.drop().await;
}

#[tokio::test]
async fn legacy_body_card_binds_its_first_source_without_reposting_pg() {
    let (database, mut pool) = database("legacy card first source binds existing receipt").await;
    for (task, status, summary, result, legacy) in [
        (
            "legacy-body-source-bind",
            "completed",
            "Agent \"Task source binding\" finished",
            "preserved body first\npreserved body second",
            "✅ Task completed\n**Agent \"Task source binding\" finished**\n\n> preserved body first\n> preserved body second\n\n-# task legacy-b",
        ),
        (
            "legacy-short-source-bind",
            "failed",
            "Agent \"status worker\" failed",
            "",
            "❌ Task failed\n**Agent \"status worker\" failed**\n\n-# task legacy-s",
        ),
    ] {
        let original = event(task, None, Some(status), summary, result);
        let hash = content_hash(legacy);
        let claim = store::claim_card(
            Some(&pool),
            &original.scope,
            "notify",
            legacy,
            &hash,
            StoreIntent::Observation,
        )
        .await
        .expect("the isolated legacy body fixture must claim its old notify authority");
        let CardClaim::Owned(claim) = claim else {
            panic!("the isolated legacy body must own its initial row: {claim:?}");
        };
        store::mark_posted(Some(&pool), &claim, LEGACY_MESSAGE, legacy, &hash)
            .await
            .expect("seed the old NULL-source body card and its durable terminal receipt");
        let before = snapshot(&pool, &original).await;
        let source = format!("first-source-{task}");
        let sourced = event(task, Some(&source), Some(status), summary, result);
        let transport = Transport::default();
        let aliased = deliver(&pool, &transport, &sourced).await;
        assert_eq!(aliased.message_id, LEGACY_MESSAGE);
        assert_eq!(aliased.bot_key, "notify");
        assert_eq!(
            transport.counts(),
            (0, 0),
            "a formatter migration must not create another completion for the same first-source alias"
        );
        assert_eq!(snapshot(&pool, &sourced).await, before);
        pool.close().await;
        pool = database.connect_and_migrate().await;
        let replay = deliver(&pool, &transport, &sourced).await;
        assert_eq!(replay.message_id, LEGACY_MESSAGE);
        assert_eq!(transport.counts(), (0, 0));
        assert_eq!(snapshot(&pool, &sourced).await, before);
    }
    database.drop().await;
}
