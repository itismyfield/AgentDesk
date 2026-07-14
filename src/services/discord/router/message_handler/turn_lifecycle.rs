use super::*;

pub(super) fn should_add_turn_pending_reaction(_dispatch_id: Option<&str>) -> bool {
    // #750: announce bot no longer writes lifecycle emojis, so the command bot
    // is now the single source of ⏳ for both regular and dispatch turns.
    // Users stop an active dispatch turn by removing this ⏳, which
    // intake_gate's classify_removed_control_reaction catches.
    // (#559 originally skipped this for dispatches to avoid duplicating the
    // announce bot's ⏳. With the announce-bot path gone, we must re-add it
    // here so the stop-via-reaction-removal path keeps working.)
    true
}

pub(in crate::services::discord) async fn mailbox_try_start_turn_with_terminal_marker_cleanup(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_msg_id: MessageId,
    session_key: Option<&str>,
) -> bool {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return super::super::super::mailbox_try_start_turn(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
        )
        .await;
    };
    let Some(session_key) = session_key.map(str::trim).filter(|value| !value.is_empty()) else {
        return super::super::super::mailbox_try_start_turn(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
        )
        .await;
    };
    let thread_channel_id = channel_id.get().to_string();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!(
                "[outbox] failed to begin terminal delivery marker cleanup before turn start for channel {}: {}",
                channel_id,
                error
            );
            return super::super::super::mailbox_try_start_turn(
                shared,
                channel_id,
                cancel_token,
                request_owner,
                user_msg_id,
            )
            .await;
        }
    };

    if let Err(error) = sqlx::query("SELECT pg_advisory_xact_lock(1752, hashtext($1))")
        .bind(&thread_channel_id)
        .execute(&mut *tx)
        .await
    {
        tracing::warn!(
            "[outbox] failed to lock terminal delivery marker before turn start for channel {}: {}",
            channel_id,
            error
        );
        let _ = tx.rollback().await;
        return super::super::super::mailbox_try_start_turn(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
        )
        .await;
    }

    let started = super::super::super::mailbox_try_start_turn(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_msg_id,
    )
    .await;
    if started
        && let Err(error) = sqlx::query(
            "UPDATE sessions
                SET active_turn_delivery_outbox_id = NULL
              WHERE session_key = $1
                AND thread_channel_id = $2
                AND active_turn_delivery_outbox_id IS NOT NULL",
        )
        .bind(session_key)
        .bind(&thread_channel_id)
        .execute(&mut *tx)
        .await
    {
        tracing::warn!(
            "[outbox] failed to clear terminal delivery marker after new turn start for channel {}: {}",
            channel_id,
            error
        );
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(
            "[outbox] failed to commit terminal delivery marker cleanup after turn start for channel {}: {}",
            channel_id,
            error
        );
    }
    started
}

pub(super) async fn cleanup_terminal_delivery_marker_after_turn_start(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    session_key: Option<&str>,
) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let Some(session_key) = session_key.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    let thread_channel_id = channel_id.get().to_string();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!(
                "[outbox] failed to begin terminal delivery marker cleanup after turn start for channel {}: {}",
                channel_id,
                error
            );
            return;
        }
    };

    if let Err(error) = sqlx::query("SELECT pg_advisory_xact_lock(1752, hashtext($1))")
        .bind(&thread_channel_id)
        .execute(&mut *tx)
        .await
    {
        tracing::warn!(
            "[outbox] failed to lock terminal delivery marker after turn start for channel {}: {}",
            channel_id,
            error
        );
        let _ = tx.rollback().await;
        return;
    }

    if let Err(error) = sqlx::query(
        "UPDATE sessions
            SET active_turn_delivery_outbox_id = NULL
          WHERE session_key = $1
            AND thread_channel_id = $2
            AND active_turn_delivery_outbox_id IS NOT NULL",
    )
    .bind(session_key)
    .bind(&thread_channel_id)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(
            "[outbox] failed to clear terminal delivery marker after turn start for channel {}: {}",
            channel_id,
            error
        );
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(
            "[outbox] failed to commit terminal delivery marker cleanup after turn start for channel {}: {}",
            channel_id,
            error
        );
    }
}
