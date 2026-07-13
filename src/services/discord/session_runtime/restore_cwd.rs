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
    // #3219: when the channel-scoped DB cwd is the channel's OWN existing managed
    // worktree (caller-validated by `db_cwd_is_reusable_worktree`: under
    // `worktrees_root`, a linked worktree, and sharing the configured parent
    // repo's git common dir), prefer it over the configured *base* workspace.
    // Otherwise crash/kill recovery installs the base as cwd, provider-channel
    // worktree isolation re-derives a FRESH worktree + provider session-id, and
    // `--resume` breaks — abandoning the session's transcript. The live-TUI
    // -binding recovery path masks this only while the tmux pane survives; once
    // the pane dies there is no fallback (root cause of the 2026-06-07 resume
    // failure: recovery read the correct worktree from the DB, logged "Ignoring
    // restored DB cwd", then built a fresh worktree + session-id).
    //
    // The predicate uses the SAME guard set as `resolve_reusable_worktree`, so a
    // stale/foreign/relocated worktree — including a workspace repointed to a
    // different repo under the same `worktrees_root` — is NOT elevated and falls
    // through to the configured path below.
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

/// #3219: true when the recovered DB cwd is the channel's own reusable managed
/// worktree and must therefore outrank the configured base workspace in
/// [`select_restored_session_path`]. Mirrors the exact guard set used by
/// [`resolve_reusable_worktree`]: the cwd must be an AgentDesk-managed
/// (`is_managed_worktree_path`) linked worktree that shares the configured
/// parent repo's git common dir (`restored_worktree_belongs_to_parent`, which
/// also rejects non-existent and remote-only paths). Returns `false` when there
/// is no configured parent (then `select_restored_session_path`'s existing
/// configured→db_cwd→yaml fallback already does the right thing).
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

/// #3216 GAP 2: decide whether a live tmux pane's cwd should override the
/// DB-resolved cwd during recovery. The live tmux pane is the authoritative
/// source of truth for where a session is actually running — if the DB cwd has
/// diverged (e.g. a phantom worktree rotation stamped a transcript-less path),
/// trusting the DB blindly relaunches `--resume` against the wrong cwd and the
/// conversation is lost.
///
/// Returns `Some(tmux_cwd)` only when ALL hold:
///   * a live tmux pane cwd is present (`tmux_cwd`);
///   * it is a real AgentDesk-managed, on-disk-usable worktree
///     (guarded by `tmux_cwd_is_managed` / `tmux_cwd_is_usable` predicates so we
///     never adopt a transient or garbage path);
///   * it actually DIFFERS from the DB cwd (nothing to reconcile otherwise).
///
/// Kept as a pure function (predicate results injected) so the reconcile policy
/// is unit-testable without a live tmux / filesystem.
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

/// Resolve a channel's persisted `sessions.cwd` for the given `session_key`,
/// scoped to the unique Discord `channel_id`, with a SAFE legacy fallback for
/// rows that predate the `channel_id` column.
///
/// #3216 GAP 1: migration `0071_sessions_channel_id.sql` only added the column;
/// existing rows kept `channel_id = NULL`. The strict `channel_id = $2` guard
/// (#3207) therefore never matches a legacy row, so the FIRST restore after
/// deploy still rotates a brand-new worktree and divorces the live session from
/// its transcript. We cannot backfill the numeric id in pure SQL (the id is not
/// derivable from `session_key`, which holds the channel NAME), so instead we
/// fall back to a name-only lookup — but ONLY when it is unambiguous:
///
///   * channel-scoped match (`session_key = $1 AND channel_id = $2`) wins first;
///   * otherwise, fall back ONLY when there is EXACTLY ONE row for the
///     `session_key` AND that row's `channel_id IS NULL` (a true legacy row).
///
/// `sessions.session_key` is globally UNIQUE (migration `001_initial.sql`), so
/// there is at most one row per key; the `rows.len() == 1` check is therefore a
/// defensive belt-and-braces guard. If the single row carries a DIFFERENT
/// non-null `channel_id`, the fallback is refused — that would reintroduce the
/// #3207 cross-channel hazard. New heartbeats stamp `channel_id`, so this
/// self-heals over time. Returns a [`RestoredCwd`] for this `session_key`:
/// `channel_scoped = true` for an exact channel-id match (whose `path` may be
/// empty when the owned row's `cwd` is NULL/missing — ownership comes from row
/// existence, #3219), or `channel_scoped = false` with a non-empty path for the
/// legacy NULL fallback.
async fn resolve_cwd_for_session_key(
    pool: &sqlx::PgPool,
    session_key: &str,
    channel_id: &str,
) -> Result<Option<RestoredCwd>, String> {
    // 1. Channel-scoped match (the #3207 cross-channel guard). `fetch_optional`
    //    returns the OUTER Option: `Some(_)` iff an exact channel-owned row
    //    exists, independent of whether its `cwd` is currently populated. #3219:
    //    ownership is reported (`channel_scoped: true`) from row EXISTENCE — a
    //    NULL/empty cwd must NOT erase ownership, or an owned channel whose row
    //    has a stale/missing cwd could not elevate the valid worktree the tmux
    //    reconcile later supplies. The caller filters the (possibly empty) path
    //    for usability separately.
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
            // #3219: a NULL-channel_id row is NOT proven to belong to THIS
            // channel (a name-collision channel resolves the same globally-unique
            // session_key). Mark it non-channel-scoped so it never gets elevated
            // over the safe configured base in `select_restored_session_path`.
            return Ok(Some(RestoredCwd {
                path,
                channel_scoped: false,
            }));
        }
    }
    Ok(None)
}

/// Resolve a channel's persisted `sessions.cwd` for restart restore, scoped to
/// the unique Discord `channel_id` (with the #3216 safe legacy fallback).
///
/// Backs the `db_cwd` lookup in [`auto_restore_session_from_resolution`], which installs
/// the resolved path into `session.current_path`. See
/// [`resolve_cwd_for_session_key`] for the channel-scoping / legacy-fallback
/// semantics.
///
/// The on-disk usability filter (`session_path_is_usable`) is applied by the
/// caller so this helper stays a pure DB resolve.
/// A persisted `sessions.cwd` resolved during restart recovery, tagged with
/// whether it came from an exact channel-id match (`channel_scoped = true`) or
/// the #3216 legacy NULL-channel_id fallback (`false`). Only a channel-scoped
/// cwd is eligible to outrank the configured base in
/// [`select_restored_session_path`] (#3219) — a NULL-fallback cwd is not proven
/// to belong to this channel.
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

/// #3216 GAP 2: correct the persisted `sessions.cwd` to the authoritative live
/// tmux pane cwd during recovery reconciliation. Scoped by `session_key` AND the
/// unique `channel_id` so a name collision can never write into another channel's
/// row (the #3207 cross-channel guard). A legacy NULL-channel_id row is matched
/// too — only the row whose id equals THIS channel OR is NULL is updated — so the
/// row both adopts the correct cwd and gets self-healed onto this channel.
///
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

/// Look up the persisted worktree path for a thread session from the `sessions`
/// DB table, mirroring the restore lookup in [`auto_restore_session_from_resolution`].
///
/// After a dcserver restart the in-memory `sessions` map is empty, so without
/// this lookup a new thread message would create a brand-new worktree and drop
/// the provider session fingerprint / recovery context tied to the previous
/// worktree path (#3011). The returned path is only honored when it still names
/// a usable git worktree on disk; otherwise we fall back to creating a fresh one.
///
/// #3207 (part 2) P0: the `session_key` is derived from the sanitized/truncated
/// channel NAME, so two distinct channels whose names collide produce the SAME
/// `session_key` and would resolve EACH OTHER's persisted cwd. The lookup is
/// therefore scoped by the unique `channel_id` — only a row stamped with THIS
/// channel's id is honored, so a name collision can never cross channels.
///
/// #3216 GAP 1: legacy rows written before the `channel_id` column existed carry
/// `channel_id = NULL` and so never match the strict scoped predicate, forcing a
/// worktree rotation on the first restore after deploy. [`resolve_cwd_for_session_key`]
/// adds a SAFE fallback that reuses such a row ONLY when it is unambiguous
/// (exactly one row for the `session_key` and its `channel_id IS NULL`), which
/// preserves the cross-channel guard while letting legacy sessions keep their
/// transcript-bearing worktree.
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
                    // The worktree-reuse path applies its own managed/belongs-to
                    // -parent guards downstream; it only needs the path here. Skip
                    // an owned row whose cwd is empty/NULL (#3219) — it carries no
                    // reusable worktree, so continue scanning the remaining keys.
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
