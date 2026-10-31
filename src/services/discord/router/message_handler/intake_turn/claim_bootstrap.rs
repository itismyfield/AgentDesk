use super::*;

/// Record the claimed request anchor and clear its previously stored idle card.
pub(super) async fn bootstrap_claimed_turn(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    started: bool,
    channel_id: ChannelId,
    user_msg_id: MessageId,
) {
    // #3811: record the original-request anchor (the real Discord user_msg_id)
    // ONLY once THIS message won the mailbox claim (`started == true`) and the
    // caller's stale-dispatch guard passed. A message that merely QUEUES behind
    // an active turn (`started == false`) must NOT touch that active turn's
    // anchor — it records its own anchor when later dequeued/promoted and
    // re-enters intake with `started == true`. Internal ids back no real
    // Discord message → `None` (no fake link).
    if started {
        shared
            .ui
            .placeholder_live_events
            .resolve_turn_request_anchor(
                http,
                channel_id,
                (!crate::services::discord::is_internal_message_id(user_msg_id))
                    .then(|| user_msg_id.get()),
            )
            .await;
    }

    #[cfg(test)]
    let started = started
        ^ crate::services::discord::session_idle_cleanup::recap_cleanup_mutant(
            "invert_discord_started",
        );
    #[cfg(test)]
    if crate::services::discord::session_idle_cleanup::recap_cleanup_mutant("skip_discord_cleanup")
    {
        return;
    }
    if started && let Some(pool) = shared.pg_pool.as_ref().cloned() {
        crate::services::discord::session_idle_cleanup::spawn_clear_captured_idle_recap_for_channel(
            http.clone(), pool, channel_id.get(),
        ).await;
    }
}

#[cfg(test)]
mod recap_cleanup_tests {
    use super::*;
    use crate::services::discord::session_idle_cleanup::recap_cleanup_tests::{
        assert_cleared, discord, pointer, seed_for_channel,
    };
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn discord_claim_clears_the_idle_card_only_for_the_winning_turn_pg() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "discord_claim_recap",
            "Discord claim legacy card wiring",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().pg_pool = Some(pool.clone());
        let channel = ChannelId::new(7_274_201);
        seed_for_channel(&pool, 4242, channel.get()).await;
        let (http, deletes, server) = discord("📦 응답 완료 · 이어서 질문 가능", true).await;
        bootstrap_claimed_turn(&http, &shared, false, channel, MessageId::new(7_274_202)).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            pointer(&pool).await,
            Some(4242),
            "a queued Discord turn must preserve the idle-card pointer"
        );
        assert_eq!(
            deletes.load(Ordering::SeqCst),
            0,
            "a queued Discord turn must not delete the idle card"
        );
        bootstrap_claimed_turn(&http, &shared, true, channel, MessageId::new(7_274_202)).await;
        assert_cleared(&pool, &deletes, "the winning Discord claim").await;
        server.abort();
        pool.close().await;
        db.drop().await;
    }
}
