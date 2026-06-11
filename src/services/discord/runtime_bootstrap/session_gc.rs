use super::super::*;

/// Periodic GC: delete stale idle/disconnected thread sessions from DB.
pub(super) async fn gc_stale_thread_sessions(shared: &Arc<SharedData>) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ Thread session GC skipped: postgres pool unavailable");
        return;
    };
    let deleted_keys = crate::db::dispatched_sessions::gc_stale_thread_sessions_pg(pool).await;
    if deleted_keys.is_empty() {
        return;
    }
    // Option A: kill the orphan thread tmux sessions whose DB rows we just
    // removed. Their inner CLI commonly stays at an interactive prompt (pane
    // never dies), so the dead-pane reaper skips them, and with the row gone
    // the 8h idle-kill policy can never reach them either — they would leak
    // forever. The effective grace becomes the GC TTL (1h no-dispatch / 3h).
    let killed = reap_orphan_thread_tmux(&deleted_keys).await;
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 GC: removed {} stale thread session(s) from DB, killed {} orphan tmux",
        deleted_keys.len(),
        killed
    );
}

/// Kill the tmux sessions whose stale thread rows were just GC'd. Only touches
/// sessions this runtime owns (owner marker) and that still exist locally, so a
/// co-located dev/release instance can't kill the other's sessions.
async fn reap_orphan_thread_tmux(deleted_keys: &[String]) -> usize {
    #[cfg(unix)]
    {
        let marker = crate::services::tmux_common::current_tmux_owner_marker();
        let keys = deleted_keys.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut killed = 0usize;
            for key in &keys {
                let Some(tmux_name) = deleted_thread_tmux_reap_candidate(
                    key,
                    &marker,
                    crate::services::discord::tmux::session_belongs_to_current_runtime,
                    crate::services::platform::tmux::has_session,
                ) else {
                    continue;
                };
                if crate::services::platform::tmux::kill_session(
                    tmux_name,
                    "stale thread session GC — DB row removed",
                ) {
                    killed += 1;
                }
            }
            killed
        })
        .await
        .unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = deleted_keys;
        0
    }
}

#[cfg(unix)]
fn deleted_thread_tmux_reap_candidate<'a>(
    session_key: &'a str,
    current_owner_marker: &str,
    belongs_to_current_runtime: impl Fn(&str, &str) -> bool,
    has_session: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    // session_key format is `hostname:tmux_name`.
    let (_, tmux_name) = session_key.split_once(':')?;
    let (_, channel_name) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)?;
    crate::services::discord::adk_session::parse_thread_channel_id_from_name(&channel_name)?;
    if !belongs_to_current_runtime(tmux_name, current_owner_marker) {
        return None;
    }
    if !has_session(tmux_name) {
        return None;
    }
    Some(tmux_name)
}

#[cfg(all(test, unix))]
mod thread_session_gc_tests {
    use super::deleted_thread_tmux_reap_candidate;
    use std::collections::HashSet;

    #[test]
    fn thread_gc_reap_candidate_requires_thread_owner_marker_and_existing_tmux() {
        let marker = "runtime-a";
        let owned_thread = "AgentDesk-codex-adk-cdx-t1500628371829428350";
        let foreign_thread = "AgentDesk-codex-adk-cdx-t1500628371829428351";
        let main_channel = "AgentDesk-codex-adk-cdx";
        let missing_thread = "AgentDesk-codex-adk-cdx-t1500628371829428352";
        let existing: HashSet<&str> = [owned_thread, foreign_thread, main_channel].into();
        let owned: HashSet<&str> = [owned_thread, main_channel].into();

        let belongs_to_current_runtime =
            |name: &str, owner_marker: &str| owner_marker == marker && owned.contains(name);
        let has_session = |name: &str| existing.contains(name);

        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                &format!("host:{owned_thread}"),
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            Some(owned_thread)
        );
        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                &format!("host:{foreign_thread}"),
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            None,
            "foreign owner marker must prevent killing another runtime's thread tmux"
        );
        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                &format!("host:{main_channel}"),
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            None,
            "thread GC must not kill fixed-channel tmux names"
        );
        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                &format!("host:{missing_thread}"),
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            None,
            "missing local tmux session is not a reap target"
        );
        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                "malformed-session-key",
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            None
        );
    }
}

/// Periodic GC: disconnect stale fixed-channel working sessions from the DB so
/// restart recovery cannot restore dead provider session IDs.
pub(super) async fn gc_stale_fixed_working_sessions(shared: &Arc<SharedData>) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let cleared = crate::db::dispatched_sessions::gc_stale_fixed_working_sessions_db_pg(pool).await;

    if cleared > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🧹 GC: disconnected {cleared} stale fixed-channel working session(s)"
        );
    }
}
