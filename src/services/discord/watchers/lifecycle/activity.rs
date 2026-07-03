use crate::services::discord::watcher_lifecycle_decision::runtime_activity_heartbeat_at;
use crate::services::provider::ProviderKind;

use super::super::WATCHER_ACTIVITY_HEARTBEAT_INTERVAL;

/// Outcome of a single `last_heartbeat` refresh attempt, used for auditable
/// logging at the `touch_session_activity` boundary (#3053). Distinguishes
/// which candidate key path matched so silent no-ops (the original failure
/// mode where TUI/watcher activity refreshed a non-matching row) are visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum HeartbeatRefreshMatch {
    /// One of the namespaced/legacy `session_key` candidates matched.
    SessionKey,
    /// Fell back to `provider + thread_channel_id` and matched.
    ThreadChannelFallback,
    /// No row matched any candidate — activity went unobserved by idle-kill.
    NoMatch,
}

pub(in crate::services::discord) struct HeartbeatRefreshOutcome {
    pub matched: HeartbeatRefreshMatch,
    pub rows_affected: u64,
}

impl HeartbeatRefreshOutcome {
    pub fn refreshed(&self) -> bool {
        self.rows_affected > 0
    }
}

// Tmux watcher output is activity, but reusing hook_session here would also
// overwrite status/tokens defaults. Touch only last_heartbeat instead.
pub(in crate::services::discord) fn refresh_session_heartbeat_from_tmux_output(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    thread_channel_id: Option<u64>,
) -> bool {
    refresh_session_heartbeat_from_tmux_output_detailed(
        pg_pool,
        token_hash,
        provider,
        tmux_session_name,
        thread_channel_id,
    )
    .refreshed()
}

/// Same as `refresh_session_heartbeat_from_tmux_output` but reports which
/// candidate key matched and how many rows were touched, so callers can emit
/// auditable activity logs (#3053).
pub(in crate::services::discord) fn refresh_session_heartbeat_from_tmux_output_detailed(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    thread_channel_id: Option<u64>,
) -> HeartbeatRefreshOutcome {
    let session_keys = super::super::super::adk_session::build_session_key_candidates(
        token_hash,
        provider,
        tmux_session_name,
    );

    if let Some(pg_pool) = pg_pool {
        let provider_name = provider.as_str().to_string();
        let thread_channel_id = thread_channel_id.map(|value| value.to_string());
        let activity_at = runtime_activity_heartbeat_at(tmux_session_name, chrono::Utc::now());
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                let updated = sqlx::query("UPDATE sessions SET last_heartbeat = GREATEST(COALESCE(last_heartbeat, TIMESTAMPTZ 'epoch'), $3) WHERE session_key = $1 OR session_key = $2")
                .bind(&session_keys[0])
                .bind(&session_keys[1])
                .bind(activity_at)
                .execute(&pool)
                .await
                .map_err(|error| format!("refresh pg watcher heartbeat by session key: {error}"))?
                .rows_affected();
                if updated > 0 {
                    return Ok(HeartbeatRefreshOutcome {
                        matched: HeartbeatRefreshMatch::SessionKey,
                        rows_affected: updated,
                    });
                }

                let Some(thread_channel_id) = thread_channel_id else {
                    return Ok(HeartbeatRefreshOutcome {
                        matched: HeartbeatRefreshMatch::NoMatch,
                        rows_affected: 0,
                    });
                };
                let updated = sqlx::query("UPDATE sessions SET last_heartbeat = GREATEST(COALESCE(last_heartbeat, TIMESTAMPTZ 'epoch'), $3) WHERE provider = $1 AND thread_channel_id = $2 AND status IN ('idle', 'working')")
                .bind(&provider_name)
                .bind(&thread_channel_id)
                .bind(activity_at)
                .execute(&pool)
                .await
                .map_err(|error| {
                    format!("refresh pg watcher heartbeat by thread channel: {error}")
                })?
                .rows_affected();
                Ok(HeartbeatRefreshOutcome {
                    matched: if updated > 0 {
                        HeartbeatRefreshMatch::ThreadChannelFallback
                    } else {
                        HeartbeatRefreshMatch::NoMatch
                    },
                    rows_affected: updated,
                })
            },
            |message| message,
        )
        .unwrap_or(HeartbeatRefreshOutcome {
            matched: HeartbeatRefreshMatch::NoMatch,
            rows_affected: 0,
        });
    }

    let _ = (provider, thread_channel_id, session_keys);
    HeartbeatRefreshOutcome {
        matched: HeartbeatRefreshMatch::NoMatch,
        rows_affected: 0,
    }
}

/// Single auditable entry point for runtime-observed session activity (#3053).
/// Refreshes `sessions.last_heartbeat = NOW()` for the row idle-kill selects
/// on and logs the resolved `session_key`, BOTH candidate keys (namespaced +
/// legacy `host:tmux`), rows-affected, `reason`/`source`, and whether the
/// `thread_channel_id` fallback was used — the original #3053 failure mode was
/// a silent no-op refresh of a non-matching row, after which idle-kill killed
/// the live session. Returns true when at least one row was touched.
pub(in crate::services::discord) fn touch_session_activity(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    thread_channel_id: Option<u64>,
    reason: &str,
    source: &str,
) -> bool {
    let session_keys = super::super::super::adk_session::build_session_key_candidates(
        token_hash,
        provider,
        tmux_session_name,
    );
    let outcome = refresh_session_heartbeat_from_tmux_output_detailed(
        pg_pool,
        token_hash,
        provider,
        tmux_session_name,
        thread_channel_id,
    );

    let used_thread_fallback = outcome.matched == HeartbeatRefreshMatch::ThreadChannelFallback;
    if outcome.refreshed() {
        tracing::debug!(
            source,
            reason,
            tmux_session = %tmux_session_name,
            namespaced_key = %session_keys[0],
            legacy_key = %session_keys[1],
            rows_affected = outcome.rows_affected,
            used_thread_fallback,
            thread_channel_id = ?thread_channel_id,
            "touch_session_activity: refreshed idle-kill heartbeat (#3053)"
        );
    } else {
        // No row matched — idle-kill will not observe this activity. This is the
        // exact #3053 failure mode, so warn (not debug) to make it actionable.
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            source,
            reason,
            tmux_session = %tmux_session_name,
            namespaced_key = %session_keys[0],
            legacy_key = %session_keys[1],
            rows_affected = outcome.rows_affected,
            thread_channel_id = ?thread_channel_id,
            "  [{ts}] ⚠ touch_session_activity: NO session row matched runtime activity — idle-kill heartbeat NOT refreshed (#3053)",
        );
    }
    outcome.refreshed()
}

pub(in crate::services::discord::tmux) fn maybe_refresh_watcher_activity_heartbeat(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    thread_channel_id: Option<u64>,
    last_heartbeat_at: &mut Option<std::time::Instant>,
) {
    let now = std::time::Instant::now();
    if last_heartbeat_at
        .is_some_and(|last| now.duration_since(last) < WATCHER_ACTIVITY_HEARTBEAT_INTERVAL)
    {
        return;
    }

    if refresh_session_heartbeat_from_tmux_output(
        pg_pool,
        token_hash,
        provider,
        tmux_session_name,
        thread_channel_id,
    ) {
        *last_heartbeat_at = Some(now);
    }
}
