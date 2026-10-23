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
pub(in crate::services::discord) async fn refresh_session_heartbeat_from_tmux_output(
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
    .await
    .refreshed()
}

/// Same as `refresh_session_heartbeat_from_tmux_output` but reports which
/// candidate key matched and how many rows were touched, so callers can emit
/// auditable activity logs (#3053).
pub(in crate::services::discord) async fn refresh_session_heartbeat_from_tmux_output_detailed(
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
        let result = async {
            let updated = sqlx::query("UPDATE sessions SET last_heartbeat = GREATEST(COALESCE(last_heartbeat, TIMESTAMPTZ 'epoch'), $3) WHERE session_key = $1 OR session_key = $2")
                .bind(&session_keys[0])
                .bind(&session_keys[1])
                .bind(activity_at)
                .execute(pg_pool)
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
                .execute(pg_pool)
                .await
                .map_err(|error| {
                    format!("refresh pg watcher heartbeat by thread channel: {error}")
                })?
                .rows_affected();
            Ok::<_, String>(HeartbeatRefreshOutcome {
                matched: if updated > 0 {
                    HeartbeatRefreshMatch::ThreadChannelFallback
                } else {
                    HeartbeatRefreshMatch::NoMatch
                },
                rows_affected: updated,
            })
        }
        .await;
        return result.unwrap_or(HeartbeatRefreshOutcome {
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
pub(in crate::services::discord) async fn touch_session_activity(
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
    )
    .await;

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

pub(in crate::services::discord::tmux) async fn maybe_refresh_watcher_activity_heartbeat(
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
    )
    .await
    {
        *last_heartbeat_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_heartbeat_yields_while_session_row_is_locked_pg() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let provider = ProviderKind::Claude;
        let session = "adk-heartbeat-7207";
        let key = crate::services::discord::adk_session::build_session_key_candidates(
            "heartbeat-test",
            &provider,
            session,
        )[0]
        .clone();
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, cwd, last_heartbeat)
             VALUES ($1, 'claude', 'idle', '/tmp', NOW())",
        )
        .bind(&key)
        .execute(&pool)
        .await
        .expect("seed session");

        let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let lock_database_url = db.database_url.clone();
        let holder = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("lock runtime")
                .block_on(async move {
                    let lock_pool = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(1)
                        .connect(&lock_database_url)
                        .await
                        .expect("connect row lock holder pool");
                    let mut tx = lock_pool.begin().await.expect("begin row lock");
                    sqlx::query(
                        "SELECT session_key FROM sessions WHERE session_key = $1 FOR UPDATE",
                    )
                    .bind(&key)
                    .execute(&mut *tx)
                    .await
                    .expect("lock session row");
                    let backend_pid = sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
                        .fetch_one(&mut *tx)
                        .await
                        .expect("read lock holder backend pid");
                    locked_tx.send(backend_pid).expect("signal row lock");
                    let main_thread_ran = release_rx.recv_timeout(Duration::from_secs(5)).is_ok();
                    tx.commit().await.expect("release row lock");
                    lock_pool.close().await;
                    main_thread_ran
                })
        });
        let lock_holder_pid = locked_rx.recv().expect("wait for row lock");

        let heartbeat_pool = pool.clone();
        let heartbeat = tokio::spawn(async move {
            refresh_session_heartbeat_from_tmux_output(
                Some(&heartbeat_pool),
                "heartbeat-test",
                &provider,
                session,
                None,
            )
            .await
        });
        let heartbeat_waited_on_lock = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let blocked_by_lock_holder = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                        SELECT 1 FROM pg_stat_activity
                        WHERE wait_event_type = 'Lock'
                          AND $1 = ANY(pg_blocking_pids(pid))
                    )",
                )
                .bind(lock_holder_pid)
                .fetch_one(&pool)
                .await
                .expect("inspect blocked heartbeat query");
                if blocked_by_lock_holder {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        release_tx.send(()).expect("release row lock");
        let holder_observed_runtime_progress = holder.join().expect("row lock holder");
        let heartbeat_succeeded = heartbeat.await.expect("heartbeat task");
        pool.close().await;
        db.drop().await;
        assert!(
            heartbeat_waited_on_lock.is_ok(),
            "heartbeat query never waited on the row lock while the current-thread runtime remained responsive"
        );
        assert!(
            holder_observed_runtime_progress,
            "the heartbeat blocked the current-thread runtime while waiting for a row lock"
        );
        assert!(heartbeat_succeeded);
    }
}
