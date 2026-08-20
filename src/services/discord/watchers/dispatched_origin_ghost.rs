//! Restore-time cleanup of dispatched-origin turns whose dispatch link vanished.

/// The `sessions` shape that makes a restored turn a dispatched-origin ghost:
/// this exact turn still holds the session, its durable dispatched-origin
/// marker is still pinned to the same nonce, and the dispatch it was started
/// from is gone. The non-destructive probe and the mutating CAS interpolate
/// this one predicate, so the probe can never answer for a row the CAS would
/// have declined.
const DISPATCHED_ORIGIN_GHOST_PREDICATE: &str = "session_key = $1
            AND channel_id = $2
            AND status IN ('turn_active', 'working')
            AND active_turn_nonce = $3
            AND dispatched_origin_turn_nonce = $3
            AND COALESCE(BTRIM(active_dispatch_id), '') = ''";

/// Consume a dispatched-origin ghost — the durable marker *and* the inflight
/// row — only after proving that this turn is one. The order is the contract
/// (#5462); it is what keeps a live turn's inflight row on disk:
///
/// 1. **Non-destructive probe.** `SELECT 1 FROM sessions` under the ghost
///    predicate. A dispatch-less interactive turn never has a
///    `dispatched_origin_turn_nonce`, so it matches zero rows and this
///    returns before anything is destroyed or mutated. A query error is
///    treated the same way.
/// 2. **Ownership proof.** The identity + turn-nonce guarded inflight clear.
///    Only `Cleared`/`Missing` proceed; every other outcome (a newer turn owns
///    the row, planned restart, rebind origin, IO error) returns without
///    touching `sessions`, so no state mutation is layered on a clear that did
///    not happen.
/// 3. **State mutation.** The CAS that releases the session, under the same
///    predicate as the probe. A session that changed hands between steps 1 and
///    3 matches zero rows there.
///
/// Returns `true` only when the step-3 CAS took exactly one row: this turn
/// consumed the durable dispatched-origin marker. The caller currently uses
/// that result to skip watcher spawn and turn re-registration, but the CAS is
/// not a liveness proof for the channel. In particular, a live row readopted
/// from an earlier generation can still satisfy this S1 predicate; the S2
/// `born_generation` gate owns that defense. A clear followed by a zero-row CAS
/// returns `false` and lets restore continue.
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
    let channel_id = state.channel_id.to_string();

    // Step 1 — judge before destroying. The ghost verdict lives in `sessions`,
    // and reading it costs nothing: with no marker for this turn there is
    // nothing to consume and the inflight row belongs to a live turn.
    match sqlx::query_scalar::<_, i32>(&format!(
        "SELECT 1 FROM sessions WHERE {DISPATCHED_ORIGIN_GHOST_PREDICATE} LIMIT 1"
    ))
    .bind(session_key)
    .bind(channel_id.as_str())
    .bind(turn_nonce)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::info!(
                channel_id = state.channel_id,
                session_key,
                turn_nonce,
                user_msg_id = state.user_msg_id,
                "ghost_probe_no_match: no dispatched-origin marker for this turn — leaving its inflight row to the live owner"
            );
            return false;
        }
        Err(error) => {
            tracing::warn!(
                channel_id = state.channel_id,
                error = %error,
                "failed to probe dispatched-origin ghost marker"
            );
            return false;
        }
    }

    // Step 2 — the clear is pinned to the identity and nonce the caller just
    // read, so a newer turn that replaced the row keeps it.
    match crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
        &provider,
        state.channel_id,
        &crate::services::discord::inflight::InflightTurnIdentity::from_state(state),
        Some(turn_nonce),
    ) {
        crate::services::discord::inflight::GuardedClearOutcome::Cleared
        | crate::services::discord::inflight::GuardedClearOutcome::Missing => {}
        // Including `IoError`: the row may still be on disk, and the existing
        // sweepers reclaim it. Marking the session idle on top of a row this
        // call could not remove is the one thing that must not happen.
        _ => return false,
    }

    // Step 3 — the marker is still required here, so a concurrent newer
    // interactive turn can neither lose its inflight nor be marked idle.
    match sqlx::query(&format!(
        "UPDATE sessions
            SET status = 'idle',
                active_dispatch_id = NULL,
                dispatched_origin_turn_nonce = NULL,
                session_info = 'Cleared orphaned dispatched-origin turn',
                last_heartbeat = NOW()
          WHERE {DISPATCHED_ORIGIN_GHOST_PREDICATE}"
    ))
    .bind(session_key)
    .bind(channel_id.as_str())
    .bind(turn_nonce)
    .execute(pool)
    .await
    {
        Ok(result) if result.rows_affected() == 1 => true,
        Ok(_) => {
            tracing::info!(
                channel_id = state.channel_id,
                session_key,
                turn_nonce,
                user_msg_id = state.user_msg_id,
                "ghost_cas_no_match: the session moved on between the probe and the release — restore continues"
            );
            false
        }
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
