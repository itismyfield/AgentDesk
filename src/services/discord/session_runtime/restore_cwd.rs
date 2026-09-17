use super::worktree::{is_managed_worktree_path, restored_worktree_belongs_to_parent};
use super::*;

pub(in crate::services::discord) fn session_path_is_usable(current_path: &str) -> bool {
    std::path::Path::new(current_path).is_dir()
}

pub(in crate::services::discord) fn select_restored_session_path(
    configured_path: Option<String>,
    db_cwd: Option<String>,
    yaml_path: Option<String>,
    db_cwd_is_reusable_worktree: bool,
) -> Option<String> {
    // #3219: prefer the DB cwd when it's the channel's own reusable managed
    // worktree (validated by `db_cwd_is_reusable_worktree`) — otherwise recovery
    // installs the base workspace, a fresh worktree/session-id is derived, and
    // `--resume` breaks (root cause of the 2026-06-07 resume failure). Same
    // guard set as `resolve_reusable_worktree`, so a stale/foreign worktree
    // falls through to the configured path below.
    if db_cwd_is_reusable_worktree
        && let Some(worktree) = db_cwd.as_ref().filter(|path| session_path_is_usable(path))
    {
        return Some(worktree.clone());
    }

    configured_path
        .filter(|path| session_path_is_usable(path))
        .or_else(|| db_cwd.filter(|path| session_path_is_usable(path)))
        .or_else(|| yaml_path.filter(|path| session_path_is_usable(path)))
}

/// #3219: true when the DB cwd is the channel's own managed worktree — an
/// AgentDesk-managed, linked worktree sharing the parent repo's git common dir
/// (mirrors `resolve_reusable_worktree`'s guard set). `false` with no
/// configured parent falls through to the existing configured→db_cwd→yaml order.
pub(in crate::services::discord) fn db_cwd_is_reusable_worktree(
    configured_path: Option<&str>,
    db_cwd: Option<&str>,
) -> bool {
    match (configured_path, db_cwd) {
        (Some(parent), Some(cwd)) => {
            is_managed_worktree_path(cwd) && restored_worktree_belongs_to_parent(parent, cwd)
        }
        _ => false,
    }
}

/// #3216 GAP 2: the live tmux pane is authoritative for cwd — if the DB cwd
/// diverged (e.g. a phantom worktree rotation), trusting it blindly relaunches
/// `--resume` against the wrong path and loses the conversation. Returns
/// `Some(tmux_cwd)` only when it's present, a real managed/usable worktree, and
/// differs from `db_cwd`. Pure (predicates injected) so it's unit-testable
/// without a live tmux/filesystem.
pub(super) fn reconcile_recovery_cwd(
    db_cwd: Option<&str>,
    tmux_cwd: Option<&str>,
    tmux_cwd_is_managed: bool,
    tmux_cwd_is_usable: bool,
) -> Option<String> {
    let tmux_cwd = tmux_cwd?.trim();
    if tmux_cwd.is_empty() || !tmux_cwd_is_managed || !tmux_cwd_is_usable {
        return None;
    }
    if db_cwd.map(str::trim) == Some(tmux_cwd) {
        // DB already agrees with the live pane — nothing to reconcile.
        return None;
    }
    Some(tmux_cwd.to_string())
}

/// Resolve `sessions.cwd` for `session_key`, scoped to `channel_id`, with a
/// safe legacy fallback for rows predating the `channel_id` column (#3216 GAP
/// 1: migration `0071_sessions_channel_id.sql` only added it; legacy rows stay
/// NULL and never match the strict scoped guard, #3207).
///
///   * channel-scoped match wins first;
///   * otherwise fall back ONLY when there is EXACTLY ONE row for the
///     `session_key` (globally unique, `001_initial.sql`) AND its `channel_id
///     IS NULL` — a true legacy row. A differing non-null id refuses the
///     fallback (would reintroduce the #3207 cross-channel hazard); new
///     heartbeats stamp `channel_id`, so this self-heals.
///
/// `channel_scoped = true` may still carry an empty `path` (#3219: ownership
/// comes from row existence, not a populated cwd).
async fn resolve_cwd_for_session_key(
    pool: &sqlx::PgPool,
    session_key: &str,
    channel_id: &str,
) -> Result<Option<RestoredCwd>, String> {
    // 1. Channel-scoped match (#3207). Ownership is row EXISTENCE, not a
    //    populated cwd (#3219) — the caller filters usability separately.
    let scoped = sqlx::query_scalar::<_, Option<String>>(
        "SELECT cwd FROM sessions \
         WHERE session_key = $1 AND channel_id = $2 LIMIT 1",
    )
    .bind(session_key)
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load session cwd {session_key}: {error}"))?;
    if let Some(cwd) = scoped {
        return Ok(Some(RestoredCwd {
            path: cwd.unwrap_or_default(),
            channel_scoped: true,
        }));
    }

    // 2. #3216 GAP 1 safe legacy fallback: inspect ALL rows for this
    //    session_key. Honor only the unambiguous single-NULL-channel_id case.
    let rows = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT cwd, channel_id FROM sessions WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load legacy session cwd {session_key}: {error}"))?;

    if rows.len() == 1 {
        let (cwd, row_channel_id) = &rows[0];
        if row_channel_id.is_none()
            && let Some(path) = cwd.clone().filter(|p| !p.is_empty())
        {
            tracing::info!(
                "  ↻ #3216 legacy NULL-channel_id fallback: reusing cwd {} for \
                 session_key={} (channel_id={}); row will self-heal on next heartbeat",
                path,
                session_key,
                channel_id
            );
            // #3219: a NULL-channel_id row isn't proven to belong to THIS
            // channel (name collisions share the session_key) — mark
            // non-channel-scoped so it can't outrank the configured base.
            return Ok(Some(RestoredCwd {
                path,
                channel_scoped: false,
            }));
        }
    }
    Ok(None)
}

/// A persisted `sessions.cwd` resolved during restart restore, scoped to
/// `channel_id` with the #3216 legacy-NULL fallback (see
/// [`resolve_cwd_for_session_key`]). `channel_scoped = true` for an exact
/// channel match; `false` for the legacy fallback — only the scoped case may
/// outrank the configured base in [`select_restored_session_path`] (#3219).
/// Backs the `db_cwd` lookup in [`auto_restore_session_force`]; the on-disk
/// usability filter is applied by that caller.
#[derive(Debug, Clone)]
pub(super) struct RestoredCwd {
    pub(super) path: String,
    pub(super) channel_scoped: bool,
}

pub(super) fn restore_session_cwd_from_db(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
    channel_id: u64,
) -> Option<RestoredCwd> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys = build_session_key_candidates(token_hash, provider, &tmux_name);
    let channel_id = channel_id.to_string();
    let pg_pool = pg_pool?;
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move {
            for session_key in session_keys {
                if let Some(restored) =
                    resolve_cwd_for_session_key(&pool, &session_key, &channel_id).await?
                {
                    return Ok(Some(restored));
                }
            }
            Ok(None)
        },
        |message| message,
    )
    .ok()
    .flatten()
}

/// #3216 GAP 2: correct `sessions.cwd` to the live tmux pane cwd, scoped by
/// `channel_id` so a name collision can't write another channel's row (#3207).
/// A legacy NULL-channel_id row is matched too, so it self-heals onto this
/// channel.
pub(super) fn correct_session_cwd_to_tmux(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
    channel_id: u64,
    tmux_cwd: &str,
) {
    let Some(pool) = pg_pool else {
        return;
    };
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys = build_session_key_candidates(token_hash, provider, &tmux_name);
    let channel_id_str = channel_id.to_string();
    let tmux_cwd = tmux_cwd.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let mut total = 0u64;
            for session_key in session_keys {
                let updated = sqlx::query(
                    "UPDATE sessions \
                     SET cwd = $1, channel_id = $2 \
                     WHERE session_key = $3 \
                       AND (channel_id = $2 OR channel_id IS NULL)",
                )
                .bind(&tmux_cwd)
                .bind(&channel_id_str)
                .bind(&session_key)
                .execute(&bridge_pool)
                .await
                .map_err(|err| format!("reconcile cwd for {session_key}: {err}"))?
                .rows_affected();
                total += updated;
            }
            Ok(total)
        },
        |error| error,
    );
    match result {
        Ok(updated) if updated > 0 => tracing::info!(
            "  ↻ #3216 reconciled DB cwd to live tmux pane for channel {} ({} row(s))",
            channel_id,
            updated
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(
            "  ⚠ #3216 failed to reconcile DB cwd to live tmux for channel {}: {}",
            channel_id,
            err
        ),
    }
}

/// Look up the persisted worktree path for a thread session, mirroring
/// [`auto_restore_session_force`]. After a dcserver restart the in-memory
/// `sessions` map is empty, so without this a new thread message creates a
/// fresh worktree and drops the recovery context tied to the old one (#3011);
/// the path is only honored when it still names a usable worktree on disk.
///
/// #3207 (part 2): `session_key` derives from the sanitized channel NAME, so
/// colliding names would resolve each other's cwd — scoped by the unique
/// `channel_id` to prevent that. #3216 GAP 1: legacy NULL-`channel_id` rows
/// use [`resolve_cwd_for_session_key`]'s unambiguous single-row fallback so
/// they keep their transcript-bearing worktree instead of rotating fresh.
pub(super) fn restore_thread_worktree_path_from_db(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
    channel_id: u64,
) -> Option<String> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys = build_session_key_candidates(token_hash, provider, &tmux_name);
    let channel_id = channel_id.to_string();
    let pg_pool = pg_pool?;
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move {
            for session_key in session_keys {
                if let Some(restored) =
                    resolve_cwd_for_session_key(&pool, &session_key, &channel_id).await?
                {
                    // Skip an owned row whose cwd is empty/NULL (#3219) — no
                    // reusable worktree; keep scanning remaining keys.
                    if !restored.path.is_empty() {
                        return Ok(Some(restored.path));
                    }
                }
            }
            Ok(None)
        },
        |message| message,
    )
    .ok()
    .flatten()
}
