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
        return crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
        )
        .await;
    };
    let Some(session_key) = session_key.map(str::trim).filter(|value| !value.is_empty()) else {
        return crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue(
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
            return crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue(
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
        return crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
        )
        .await;
    }

    let started = crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    use std::time::Instant;

    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        ScopedRuntimeRoot {
            _lock: lock,
            _temp: temp,
            previous,
        }
    }

    fn queued_user_intervention(message_id: MessageId) -> Intervention {
        Intervention {
            author_id: UserId::new(5_937),
            author_is_bot: false,
            message_id,
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![message_id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued inbound".to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    /// #5937 — the production text-intake entry point, not just the mailbox
    /// gate underneath it, must admit behind queued inbound work. Routing this
    /// function back to the unordered claim turns this test red.
    #[tokio::test(flavor = "current_thread")]
    async fn text_intake_turn_start_waits_behind_queued_inbound() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_937_301);
        let queued = MessageId::new(5_937_302);
        let arrival = MessageId::new(5_937_303);
        let persistence = crate::services::discord::queue_dispatch::persistence_context(
            &shared, &provider, channel_id,
        );
        shared
            .mailbox(channel_id)
            .replace_queue(vec![queued_user_intervention(queued)], persistence.clone())
            .await;

        let started = mailbox_try_start_turn_with_terminal_marker_cleanup(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            UserId::new(5_937),
            arrival,
            None,
        )
        .await;

        assert!(
            !started,
            "text intake must not take the idle slot ahead of a queued inbound message"
        );
        assert!(
            shared.pg_pool.is_none(),
            "this test drives the no-pool branch of the production entry point"
        );
    }
}
