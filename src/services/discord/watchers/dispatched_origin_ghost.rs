//! Restore-time cleanup of dispatched-origin turns whose dispatch link vanished.

/// Consume only a dispatched-origin marker for this exact turn after its
/// identity-guarded inflight clear succeeded. Dispatch-less interactive turns
/// never have `dispatched_origin_turn_nonce`, so this fails closed for them.
pub(super) async fn consume_dispatched_origin_ghost_if_current(
    pg_pool: Option<&sqlx::PgPool>,
    state: &crate::services::discord::inflight::InflightTurnState,
) -> bool {
    let (Some(pool), Some(session_key), Some(turn_nonce)) = (
        pg_pool,
        state.session_key.as_deref(),
        state
            .turn_nonce
            .as_deref()
            .filter(|value| !value.is_empty()),
    ) else {
        return false;
    };

    let Some(provider) = state.provider_kind() else {
        return false;
    };
    let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(state);
    match crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
        &provider,
        state.channel_id,
        &identity,
        Some(turn_nonce),
    ) {
        crate::services::discord::inflight::GuardedClearOutcome::Cleared
        | crate::services::discord::inflight::GuardedClearOutcome::Missing => {}
        _ => return false,
    }

    // The inflight clear is pinned to the same identity and nonce. This CAS
    // independently requires the durable origin marker, so a concurrent newer
    // interactive turn can neither lose its inflight nor be marked idle.
    let channel_id = state.channel_id.to_string();
    match sqlx::query(
        "UPDATE sessions
            SET status = 'idle',
                active_dispatch_id = NULL,
                dispatched_origin_turn_nonce = NULL,
                session_info = 'Cleared orphaned dispatched-origin turn',
                last_heartbeat = NOW()
          WHERE session_key = $1
            AND channel_id = $2
            AND status IN ('turn_active', 'working')
            AND active_turn_nonce = $3
            AND dispatched_origin_turn_nonce = $3
            AND COALESCE(BTRIM(active_dispatch_id), '') = ''",
    )
    .bind(session_key)
    .bind(channel_id)
    .bind(turn_nonce)
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() == 1,
        Err(error) => {
            tracing::warn!(
                channel_id = state.channel_id,
                error = %error,
                "failed to consume dispatched-origin ghost marker"
            );
            false
        }
    }
}
