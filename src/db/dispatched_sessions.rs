use serde_json::json;
use sqlx::{PgPool, Row};

use crate::db::agents::resolve_agent_channel_for_provider_pg;
use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
};
use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::session_activity::SessionActivityResolver;

pub(crate) async fn load_dispatch_thread_id_pg(pool: &PgPool, dispatch_id: &str) -> Option<String> {
    let thread_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT thread_id FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();
    normalize_thread_channel_id(thread_id.as_deref())
}

#[derive(Debug)]
pub(crate) struct RetryDispatchMeta {
    pub(crate) card_id: String,
    pub(crate) to_agent_id: Option<String>,
    pub(crate) dispatch_type: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) context: Option<String>,
    pub(crate) retry_count: i64,
}

pub(crate) async fn load_force_kill_session_pg(
    pool: &PgPool,
    session_key: &str,
    provider_name: Option<&str>,
) -> Result<
    Option<(
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )>,
    String,
> {
    let row = sqlx::query(
        "SELECT active_dispatch_id, agent_id, thread_channel_id, provider, instance_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres session {session_key}: {}",
            crate::utils::redact::redact_known_secrets(&error.to_string())
        )
    })?;

    let Some(row) = row else {
        return Ok(None);
    };

    let active_dispatch_id: Option<String> = row
        .try_get("active_dispatch_id")
        .map_err(|error| format!("decode active_dispatch_id for {session_key}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode agent_id for {session_key}: {error}"))?;
    let thread_channel_id: Option<String> = row
        .try_get("thread_channel_id")
        .map_err(|error| format!("decode thread_channel_id for {session_key}: {error}"))?;
    let session_provider: Option<String> = row
        .try_get("provider")
        .map_err(|error| format!("decode provider for {session_key}: {error}"))?;
    let instance_id: Option<String> = row
        .try_get("instance_id")
        .map_err(|error| format!("decode instance_id for {session_key}: {error}"))?;

    let effective_provider = provider_name.or(session_provider.as_deref());
    let runtime_channel_id =
        if let Some(channel_id) = normalize_thread_channel_id(thread_channel_id.as_deref()) {
            Some(channel_id)
        } else if let Some(agent_id) = agent_id.as_deref() {
            resolve_agent_channel_for_provider_pg(pool, agent_id, effective_provider)
            .await
            .map_err(|error| {
                format!(
                    "resolve postgres channel for session {session_key} / agent {agent_id}: {error}"
                )
            })?
            .and_then(|channel| normalize_thread_channel_id(Some(channel.as_str())))
        } else {
            None
        };

    Ok(Some((
        active_dispatch_id,
        agent_id,
        runtime_channel_id,
        session_provider,
        instance_id,
    )))
}

/// #3306: narrow durable-truth accessor for the idle-relay drift self-heal.
///
/// Reads the `sessions.channel_id` column (the dispatch-time owner channel a
/// session's own hook upsert records via
/// `channel_id = COALESCE(EXCLUDED.channel_id, sessions.channel_id)`) plus the
/// owning `instance_id`, so a registry-drifted ROUTINE tmux session — which has
/// no settings channel binding and whose `agent_id`/`thread_channel_id` are NULL
/// (so `load_force_kill_session_pg` cannot resolve it) — can re-derive its
/// authoritative owner channel. The drift self-heal still gates this value
/// behind a live-pane check, an instance-id guard, and a dedupe-mirror agreement
/// check before promoting it; this function is read-only and never authoritative
/// on its own.
pub(crate) async fn load_session_channel_id_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<(u64, Option<String>)>, String> {
    let row = sqlx::query("SELECT channel_id, instance_id FROM sessions WHERE session_key = $1")
        .bind(session_key)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!(
                "load session channel_id {session_key}: {}",
                crate::utils::redact::redact_known_secrets(&error.to_string())
            )
        })?;

    let Some(row) = row else {
        return Ok(None);
    };
    let channel_id: Option<String> = row
        .try_get("channel_id")
        .map_err(|error| format!("decode channel_id for {session_key}: {error}"))?;
    let instance_id: Option<String> = row
        .try_get("instance_id")
        .map_err(|error| format!("decode instance_id for {session_key}: {error}"))?;
    let Some(channel_id) = channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value != 0)
    else {
        return Ok(None);
    };
    Ok(Some((channel_id, instance_id)))
}

/// Current provider-binding context for a session row, used by the
/// `/resume` rebind path. Returns the row's `active_dispatch_id` (so the caller
/// can refuse to rebind a channel with in-flight dispatch work), plus the
/// currently-bound `cwd` and `claude_session_id` that the rebind will replace.
/// `Ok(None)` means no session row exists for `session_key`.
pub(crate) struct SessionRebindContext {
    pub(crate) active_dispatch_id: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) claude_session_id: Option<String>,
}

pub(crate) async fn load_session_rebind_context_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<SessionRebindContext>, String> {
    let row = sqlx::query(
        "SELECT active_dispatch_id, cwd, claude_session_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        format!(
            "load rebind context for session {session_key}: {}",
            crate::utils::redact::redact_known_secrets(&error.to_string())
        )
    })?;

    let Some(row) = row else {
        return Ok(None);
    };

    let active_dispatch_id: Option<String> = row
        .try_get("active_dispatch_id")
        .map_err(|error| format!("decode active_dispatch_id for {session_key}: {error}"))?;
    let cwd: Option<String> = row
        .try_get("cwd")
        .map_err(|error| format!("decode cwd for {session_key}: {error}"))?;
    let claude_session_id: Option<String> = row
        .try_get("claude_session_id")
        .map_err(|error| format!("decode claude_session_id for {session_key}: {error}"))?;

    Ok(Some(SessionRebindContext {
        active_dispatch_id,
        cwd,
        claude_session_id,
    }))
}

/// #4790 `/resume` auto-select guard: every provider session id that is
/// *currently bound* to some channel's session row. The auto-select path must
/// never adopt a transcript that another live channel is actively using —
/// doing so would repoint two channels at one session id and thrash their
/// bindings (the #2843 live-binding hazard). A rotated/superseded prior session
/// is not in this set (its row was overwritten), so it stays selectable.
pub(crate) async fn load_live_bound_session_ids_pg(
    pool: &PgPool,
) -> Result<std::collections::HashSet<String>, String> {
    let rows = sqlx::query_scalar::<_, Option<String>>(
        "SELECT claude_session_id FROM sessions
         WHERE claude_session_id IS NOT NULL AND BTRIM(claude_session_id) <> ''
         UNION
         SELECT raw_provider_session_id FROM sessions
         WHERE raw_provider_session_id IS NOT NULL AND BTRIM(raw_provider_session_id) <> ''",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load live-bound session ids: {error}"))?;
    Ok(rows.into_iter().flatten().collect())
}

/// Rebind a session row to a previous provider session: point `cwd` at the
/// target worktree and `claude_session_id`/`raw_provider_session_id` at the
/// target provider session id so the next turn resumes that conversation
/// (`--resume <id>` in the target cwd). `claude_session_id_recorded_at` is
/// refreshed when the id actually changes. Returns the number of rows updated
/// (0 when the `session_key` no longer exists).
///
/// N2 caveat: both `claude_session_id` and `raw_provider_session_id` are set to
/// the same `$3`. The auto-select path is Claude-only, where these coincide. For
/// an *explicit* non-Claude rebind (e.g. Codex, where the raw provider session
/// id can differ from the resume selector) this collapses the two ids; callers
/// needing a distinct raw id must widen this signature. Acceptable today because
/// the only non-Claude entry is an explicit `session_id` the caller already
/// treats as the provider resume token.
pub(crate) async fn rebind_session_provider_pg(
    pool: &PgPool,
    session_key: &str,
    target_cwd: &str,
    target_session_id: &str,
) -> Result<u64, String> {
    sqlx::query(
        "UPDATE sessions
         SET cwd = $2,
             claude_session_id = $3,
             raw_provider_session_id = $3,
             claude_session_id_recorded_at = CASE
               WHEN claude_session_id IS DISTINCT FROM $3 THEN NOW()
               ELSE COALESCE(claude_session_id_recorded_at, NOW())
             END
         WHERE session_key = $1",
    )
    .bind(session_key)
    .bind(target_cwd)
    .bind(target_session_id)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("rebind session {session_key} provider binding: {error}"))
}

pub(crate) async fn disconnect_session_and_prepare_retry_pg(
    pool: &PgPool,
    session_key: &str,
    active_dispatch_id: Option<&str>,
    retry: bool,
) -> Result<Option<RetryDispatchMeta>, String> {
    // #2045 Finding 4 (P0): force-kill used to issue a raw `UPDATE
    // task_dispatches SET status='failed'` inside the same tx that disconnects
    // the session row. That bypassed semaphore release, auto_queue_entries
    // reconcile, phase-gate reconcile, observability emit, and wait-queue
    // wake — i.e. the same cleanup hazards described in Finding 3. The fix:
    //   1) disconnect the session row in its own short tx (so we don't hold a
    //      tx open across the canonical pipeline call below),
    //   2) load retry metadata (still pending/dispatched at that point),
    //   3) delegate the dispatch terminal transition to the canonical
    //      `set_dispatch_status_on_pg_async`, which owns the full cleanup
    //      pipeline,
    //   4) guard against `cancelled → failed` — cancelled is already terminal
    //      and overwriting it would corrupt incident metrics and double-count
    //      failures on retry.
    {
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| format!("begin postgres force-kill transaction: {error}"))?;

        sqlx::query(
            "UPDATE sessions
             SET status = 'disconnected',
                 active_dispatch_id = NULL
             WHERE session_key = $1",
        )
        .bind(session_key)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("disconnect postgres session {session_key}: {error}"))?;

        tx.commit()
            .await
            .map_err(|error| format!("commit postgres force-kill transaction: {error}"))?;
    }

    let mut retry_meta = None;
    if let Some(dispatch_id) = active_dispatch_id {
        let current_status = sqlx::query_scalar::<_, Option<String>>(
            "SELECT status
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres dispatch status {dispatch_id}: {error}"))?
        .flatten();

        let current_status_str = current_status.as_deref();
        // Terminal states: never overwrite. `cancelled` in particular must
        // remain `cancelled`; rewriting it to `failed` would corrupt
        // incident metrics (Finding 4).
        let is_terminal = matches!(
            current_status_str,
            Some("completed") | Some("cancelled") | Some("failed")
        );

        if !is_terminal {
            let reason = json!({
                "reason": "force_kill_session",
                "session_key": session_key,
            });
            let allowed_from: &[&str] = &["pending", "dispatched"];
            // Delegate to the canonical pipeline. The async variant is used
            // here because we're already inside a tokio runtime (axum
            // handler); the sync wrapper would `block_on` and panic.
            crate::dispatch::set_dispatch_status_on_pg_async(
                pool,
                dispatch_id,
                "failed",
                Some(&reason),
                "force_kill_session",
                Some(allowed_from),
                true,
            )
            .await
            .map_err(|error| {
                format!("canonical fail postgres dispatch {dispatch_id} during force-kill: {error}")
            })?;
        }

        // #2045 Finding 4 cancelled→failed guard: if the dispatch was already
        // `cancelled` (or otherwise terminal) before force-kill ran, do not
        // synthesize a retry on top of that. The original cancel intent — or
        // the completion that already happened — must remain authoritative.
        if retry && !is_terminal {
            retry_meta = sqlx::query(
                "SELECT
                    kanban_card_id,
                    to_agent_id,
                    dispatch_type,
                    title,
                    context,
                    COALESCE(retry_count, 0)::BIGINT AS retry_count
                 FROM task_dispatches
                 WHERE id = $1",
            )
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load postgres retry metadata {dispatch_id}: {error}"))?
            .map(|row| {
                Ok(RetryDispatchMeta {
                    card_id: row.try_get("kanban_card_id")?,
                    to_agent_id: row.try_get("to_agent_id")?,
                    dispatch_type: row.try_get("dispatch_type")?,
                    title: row.try_get("title")?,
                    context: row.try_get("context")?,
                    retry_count: row.try_get("retry_count")?,
                })
            })
            .transpose()
            .map_err(|error: sqlx::Error| {
                format!("decode postgres retry metadata {dispatch_id}: {error}")
            })?;
        }
    }

    Ok(retry_meta)
}

pub(crate) async fn create_retry_dispatch_pg(
    pool: &PgPool,
    meta: &RetryDispatchMeta,
) -> Result<String, String> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    let dispatch_type = meta
        .dispatch_type
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("implementation");
    let title = meta
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("retry dispatch");

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres retry dispatch transaction: {error}"))?;

    sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            retry_count,
            created_at,
            updated_at
        ) VALUES (
            $1, $2, $3, $4, 'pending', $5, $6, $7, NOW(), NOW()
        )",
    )
    .bind(&dispatch_id)
    .bind(&meta.card_id)
    .bind(meta.to_agent_id.as_deref())
    .bind(dispatch_type)
    .bind(title)
    .bind(meta.context.as_deref().unwrap_or("{}"))
    .bind(meta.retry_count + 1)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres retry dispatch {dispatch_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES (
            $1, $2, $3, NULL, 'pending', 'force_kill_session_retry', NULL
        )",
    )
    .bind(&dispatch_id)
    .bind(&meta.card_id)
    .bind(dispatch_type)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres retry dispatch event {dispatch_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title, required_capabilities
         )
         SELECT $1, 'notify', $2, $3, $4, required_capabilities
           FROM task_dispatches
          WHERE id = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(&dispatch_id)
    .bind(meta.to_agent_id.as_deref())
    .bind(&meta.card_id)
    .bind(title)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres retry dispatch outbox {dispatch_id}: {error}"))?;

    sqlx::query(
        "UPDATE kanban_cards
         SET latest_dispatch_id = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(&dispatch_id)
    .bind(&meta.card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        format!(
            "update postgres card latest_dispatch_id for {}: {error}",
            meta.card_id
        )
    })?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres retry dispatch {dispatch_id}: {error}"))?;

    Ok(dispatch_id)
}

/// #2036 Surface 1+2: format a fresh dispatch-type label that the API response
/// can substitute for the (possibly stale) `sessions.session_info` value. The
/// session_info column lags behind same-thread dispatch transitions because it
/// is only refreshed once codex receives the new dispatch prompt and emits its
/// first message — during the queue-pending window between `task_dispatches`
/// status flipping to `dispatched` and the bridge actually delivering the
/// prompt, the row still reflects the *previous* dispatch.
fn dispatch_type_label_for_session_info(dispatch_type: Option<&str>) -> String {
    let kind = match dispatch_type {
        Some("implementation") => "구현",
        Some("review") => "리뷰",
        Some("rework") => "리워크",
        Some("review-decision") => "리뷰 검토",
        Some("pm-decision") => "PM 판단",
        Some("e2e-test") => "E2E 테스트",
        Some("consultation") => "상담",
        Some("phase-gate") => "phase-gate",
        Some(other) => return format!("{other} dispatch"),
        None => return "dispatch".to_string(),
    };
    format!("{kind} dispatch")
}

/// #2036 Surface 2: collapse `(task_dispatches.status, sessions.status,
/// last_heartbeat, dispatch_delivery_events.sent_at)` into one of three
/// observable sub-states so the API consumer can tell apart "bridge is still
/// holding the prompt for an earlier turn" from "codex has the prompt and is
/// actively processing it" from terminal states. Backward-compat: callers that
/// only inspect the legacy `status` field see the same value as before.
fn classify_delivery_state(
    td_status: Option<&str>,
    session_is_working: bool,
    sent_at_is_set: bool,
) -> &'static str {
    match td_status.map(str::to_ascii_lowercase).as_deref() {
        Some("completed") => "completed",
        Some("failed") => "failed",
        Some("cancelled") => "cancelled",
        Some("dispatched") | Some("in_progress") => {
            // sent_at is the per-dispatch bridge delivery signal: it flips
            // only after the bridge actually hands this dispatch's prompt to
            // codex. session_is_working is per-session (tmux liveness +
            // heartbeat) so during the bridge per-channel queue window it
            // reflects the *previous* turn of the same session, not the new
            // dispatch. sent_at must dominate is_working (#2036 review).
            if !sent_at_is_set {
                "queued"
            } else if session_is_working {
                "codex_active"
            } else {
                "delivered"
            }
        }
        Some("pending") => "queued",
        _ => "unknown",
    }
}

pub(crate) async fn list_dispatched_sessions_pg(
    pool: &PgPool,
    include_all: bool,
) -> Result<Vec<serde_json::Value>, String> {
    let sql = if include_all {
        "SELECT
            s.id,
            s.session_key,
            s.instance_id,
            s.agent_id,
            s.provider,
            s.status,
            s.active_dispatch_id,
            s.model,
            s.tokens,
            s.cwd,
            s.last_heartbeat,
            s.session_info,
            a.department,
            a.sprite_number,
            a.avatar_emoji,
            COALESCE(a.xp, 0)::BIGINT AS stats_xp,
            d.name AS department_name,
            d.name_ko AS department_name_ko,
            d.color AS department_color,
            s.thread_channel_id,
            td.thread_id AS dispatch_thread_id,
            td.dispatch_type AS dispatch_type,
            td.status AS dispatch_row_status,
            td.created_at AS dispatch_created_at,
            sent_evt.sent_at AS dispatch_sent_at,
            aqe.id AS auto_queue_entry_id,
            aqe.run_id AS auto_queue_run_id,
            aqe.slot_index::BIGINT AS auto_queue_slot_index,
            aqe.thread_group::BIGINT AS auto_queue_thread_group
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         LEFT JOIN task_dispatches td ON td.id = s.active_dispatch_id
         LEFT JOIN LATERAL (
            SELECT MIN(created_at) AS sent_at
            FROM dispatch_delivery_events
            WHERE dispatch_id = s.active_dispatch_id AND status = 'sent'
         ) sent_evt ON TRUE
         LEFT JOIN LATERAL (
            SELECT id, run_id, slot_index, thread_group
            FROM auto_queue_entries
            WHERE dispatch_id = s.active_dispatch_id
            ORDER BY created_at DESC, id ASC
            LIMIT 1
         ) aqe ON TRUE
         ORDER BY s.id"
    } else {
        "SELECT
            s.id,
            s.session_key,
            s.instance_id,
            s.agent_id,
            s.provider,
            s.status,
            s.active_dispatch_id,
            s.model,
            s.tokens,
            s.cwd,
            s.last_heartbeat,
            s.session_info,
            a.department,
            a.sprite_number,
            a.avatar_emoji,
            COALESCE(a.xp, 0)::BIGINT AS stats_xp,
            d.name AS department_name,
            d.name_ko AS department_name_ko,
            d.color AS department_color,
            s.thread_channel_id,
            td.thread_id AS dispatch_thread_id,
            td.dispatch_type AS dispatch_type,
            td.status AS dispatch_row_status,
            td.created_at AS dispatch_created_at,
            sent_evt.sent_at AS dispatch_sent_at,
            aqe.id AS auto_queue_entry_id,
            aqe.run_id AS auto_queue_run_id,
            aqe.slot_index::BIGINT AS auto_queue_slot_index,
            aqe.thread_group::BIGINT AS auto_queue_thread_group
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         LEFT JOIN task_dispatches td ON td.id = s.active_dispatch_id
         LEFT JOIN LATERAL (
            SELECT MIN(created_at) AS sent_at
            FROM dispatch_delivery_events
            WHERE dispatch_id = s.active_dispatch_id AND status = 'sent'
         ) sent_evt ON TRUE
         LEFT JOIN LATERAL (
            SELECT id, run_id, slot_index, thread_group
            FROM auto_queue_entries
            WHERE dispatch_id = s.active_dispatch_id
            ORDER BY created_at DESC, id ASC
            LIMIT 1
         ) aqe ON TRUE
         WHERE s.active_dispatch_id IS NOT NULL
         ORDER BY s.id"
    };

    let rows = sqlx::query(sql)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("list postgres sessions: {error}"))?;

    let mut resolver = SessionActivityResolver::new();
    let mut sessions = Vec::with_capacity(rows.len());

    for row in rows {
        let id: i64 = row
            .try_get("id")
            .map_err(|error| format!("decode postgres session id: {error}"))?;
        let session_key: Option<String> = row
            .try_get("session_key")
            .map_err(|error| format!("decode postgres session_key for session {id}: {error}"))?;
        let instance_id: Option<String> = row
            .try_get("instance_id")
            .map_err(|error| format!("decode postgres instance_id for session {id}: {error}"))?;
        let agent_id: Option<String> = row
            .try_get("agent_id")
            .map_err(|error| format!("decode postgres agent_id for session {id}: {error}"))?;
        let provider: Option<String> = row
            .try_get("provider")
            .map_err(|error| format!("decode postgres provider for session {id}: {error}"))?;
        let status: Option<String> = row
            .try_get("status")
            .map_err(|error| format!("decode postgres status for session {id}: {error}"))?;
        let active_dispatch_id: Option<String> =
            row.try_get("active_dispatch_id").map_err(|error| {
                format!("decode postgres active_dispatch_id for session {id}: {error}")
            })?;
        let model: Option<String> = row
            .try_get("model")
            .map_err(|error| format!("decode postgres model for session {id}: {error}"))?;
        let tokens: i64 = row
            .try_get("tokens")
            .map_err(|error| format!("decode postgres tokens for session {id}: {error}"))?;
        let cwd: Option<String> = row
            .try_get("cwd")
            .map_err(|error| format!("decode postgres cwd for session {id}: {error}"))?;
        let last_heartbeat: Option<chrono::DateTime<chrono::Utc>> =
            row.try_get("last_heartbeat").map_err(|error| {
                format!("decode postgres last_heartbeat for session {id}: {error}")
            })?;
        let last_heartbeat = last_heartbeat.map(|value| value.to_rfc3339());
        let session_info: Option<String> = row
            .try_get("session_info")
            .map_err(|error| format!("decode postgres session_info for session {id}: {error}"))?;
        let department_id: Option<String> = row
            .try_get("department")
            .map_err(|error| format!("decode postgres department for session {id}: {error}"))?;
        let sprite_number: Option<i64> = row
            .try_get("sprite_number")
            .map_err(|error| format!("decode postgres sprite_number for session {id}: {error}"))?;
        let avatar_emoji: Option<String> = row
            .try_get("avatar_emoji")
            .map_err(|error| format!("decode postgres avatar_emoji for session {id}: {error}"))?;
        let stats_xp: i64 = row
            .try_get("stats_xp")
            .map_err(|error| format!("decode postgres stats_xp for session {id}: {error}"))?;
        let department_name: Option<String> = row.try_get("department_name").map_err(|error| {
            format!("decode postgres department_name for session {id}: {error}")
        })?;
        let department_name_ko: Option<String> =
            row.try_get("department_name_ko").map_err(|error| {
                format!("decode postgres department_name_ko for session {id}: {error}")
            })?;
        let department_color: Option<String> =
            row.try_get("department_color").map_err(|error| {
                format!("decode postgres department_color for session {id}: {error}")
            })?;
        let thread_channel_id: Option<String> =
            row.try_get("thread_channel_id").map_err(|error| {
                format!("decode postgres thread_channel_id for session {id}: {error}")
            })?;
        let dispatch_thread_id: Option<String> =
            row.try_get("dispatch_thread_id").map_err(|error| {
                format!("decode postgres dispatch_thread_id for session {id}: {error}")
            })?;
        // #2036 Surface 1: dispatch_type, dispatch_row_status, dispatch_created_at
        // are joined from the *currently linked* task_dispatches row, so the API
        // response can substitute a fresh `── <type> dispatch ──`-shaped label
        // instead of trusting the (possibly stale) sessions.session_info column.
        let dispatch_type: Option<String> = row
            .try_get("dispatch_type")
            .map_err(|error| format!("decode postgres dispatch_type for session {id}: {error}"))?;
        let dispatch_row_status: Option<String> =
            row.try_get("dispatch_row_status").map_err(|error| {
                format!("decode postgres dispatch_row_status for session {id}: {error}")
            })?;
        let dispatch_created_at: Option<chrono::DateTime<chrono::Utc>> =
            row.try_get("dispatch_created_at").map_err(|error| {
                format!("decode postgres dispatch_created_at for session {id}: {error}")
            })?;
        let dispatch_sent_at: Option<chrono::DateTime<chrono::Utc>> =
            row.try_get("dispatch_sent_at").map_err(|error| {
                format!("decode postgres dispatch_sent_at for session {id}: {error}")
            })?;
        let auto_queue_entry_id: Option<String> =
            row.try_get("auto_queue_entry_id").map_err(|error| {
                format!("decode postgres auto_queue_entry_id for session {id}: {error}")
            })?;
        let auto_queue_run_id: Option<String> =
            row.try_get("auto_queue_run_id").map_err(|error| {
                format!("decode postgres auto_queue_run_id for session {id}: {error}")
            })?;
        let auto_queue_slot_index: Option<i64> =
            row.try_get("auto_queue_slot_index").map_err(|error| {
                format!("decode postgres auto_queue_slot_index for session {id}: {error}")
            })?;
        let auto_queue_thread_group: Option<i64> =
            row.try_get("auto_queue_thread_group").map_err(|error| {
                format!("decode postgres auto_queue_thread_group for session {id}: {error}")
            })?;
        let tmux_session = tmux_session_name_from_session_key(session_key.as_deref());
        let resolved_thread_channel_id = normalize_thread_channel_id(dispatch_thread_id.as_deref())
            .or_else(|| normalize_thread_channel_id(thread_channel_id.as_deref()))
            .or_else(|| {
                session_key
                    .as_deref()
                    .and_then(parse_thread_channel_id_from_session_key)
            });

        let effective = resolver.resolve(
            session_key.as_deref(),
            status.as_deref(),
            active_dispatch_id.as_deref(),
            last_heartbeat.as_deref(),
        );
        if !include_all && !effective.is_working && effective.active_dispatch_id.is_none() {
            continue;
        }
        if !include_all && thread_channel_id.is_some() && !effective.is_working {
            continue;
        }

        // #2036 Surface 1: when an active dispatch is linked, derive the
        // session_info label from task_dispatches.dispatch_type instead of
        // trusting the cached sessions.session_info string. The cached value
        // only refreshes once codex receives the new dispatch prompt and emits
        // its first reply, which leaves the row showing the *previous*
        // dispatch's `── <type> dispatch ──` header for the entire
        // queue-pending window after a same-thread phase-gate → impl handoff.
        //
        // We only override when the cached label clearly belongs to a
        // different dispatch_type — i.e. it starts with `── ` (the dispatch
        // decorator) but the type token in it does not match the live
        // dispatch_type. Free-form `<repo> 작업 진행 중`-style summaries
        // produced by `derive_adk_session_info` are preserved untouched.
        let active_dispatch_present = effective.active_dispatch_id.is_some();
        let dispatch_type_label = active_dispatch_present
            .then(|| dispatch_type_label_for_session_info(dispatch_type.as_deref()));
        let session_info_effective: Option<String> = match (
            active_dispatch_present,
            dispatch_type.as_deref(),
            session_info.as_deref(),
        ) {
            (true, Some(td_type), Some(existing))
                if existing.trim_start().starts_with("── ")
                    && !existing.contains(&format!("── {td_type} dispatch ──")) =>
            {
                dispatch_type_label.map(|label| format!("── {label} ──"))
            }
            (true, Some(_), None) => dispatch_type_label.map(|label| format!("── {label} ──")),
            _ => session_info.clone(),
        };

        // #2036 Surface 2: collapse the (task_dispatches.status,
        // sessions.is_working, sent_at) tuple into a single `delivery_state`
        // field so callers can tell apart bridge-queue-pending from
        // codex-actively-running without re-deriving the join themselves.
        let delivery_state = if active_dispatch_present {
            classify_delivery_state(
                dispatch_row_status.as_deref(),
                effective.is_working,
                dispatch_sent_at.is_some(),
            )
        } else {
            "none"
        };
        let dispatch_created_at_iso = dispatch_created_at.map(|value| value.to_rfc3339());
        let dispatch_sent_at_iso = dispatch_sent_at.map(|value| value.to_rfc3339());

        sessions.push(json!({
            "id": id.to_string(),
            "session_key": session_key,
            "instance_id": instance_id,
            "agent_id": agent_id,
            "provider": provider,
            "status": effective.status,
            "active_dispatch_id": effective.active_dispatch_id,
            "model": model,
            "tokens": tokens,
            "cwd": cwd,
            "last_heartbeat": last_heartbeat,
            "session_info": session_info_effective,
            "session_info_raw": session_info,
            "dispatch_type": dispatch_type,
            "dispatch_row_status": dispatch_row_status,
            "delivery_state": delivery_state,
            "dispatch_created_at": dispatch_created_at_iso,
            "dispatch_sent_at": dispatch_sent_at_iso,
            "linked_agent_id": agent_id,
            "last_seen_at": last_heartbeat,
            "name": session_key,
            "department_id": department_id,
            "sprite_number": sprite_number,
            "avatar_emoji": avatar_emoji.unwrap_or_else(|| "\u{1F916}".to_string()),
            "stats_xp": stats_xp,
            "connected_at": null,
            "department_name": department_name,
            "department_name_ko": department_name_ko,
            "department_color": department_color,
            "thread_channel_id": thread_channel_id,
            "dispatch_thread_id": dispatch_thread_id,
            "resolved_thread_channel_id": resolved_thread_channel_id,
            "tmux_session": tmux_session,
            "auto_queue_entry_id": auto_queue_entry_id,
            "auto_queue_run_id": auto_queue_run_id,
            "auto_queue_slot_index": auto_queue_slot_index,
            "auto_queue_thread_group": auto_queue_thread_group,
            "recovery_identifiers": {
                "session_key": session_key,
                "tmux_session": tmux_session,
                "active_dispatch_id": effective.active_dispatch_id,
                "thread_channel_id": resolved_thread_channel_id,
                "auto_queue_entry_id": auto_queue_entry_id,
                "auto_queue_run_id": auto_queue_run_id,
                "auto_queue_slot_index": auto_queue_slot_index,
                "auto_queue_thread_group": auto_queue_thread_group,
            },
        }));
    }

    Ok(sessions)
}

fn tmux_session_name_from_session_key(session_key: Option<&str>) -> Option<String> {
    tmux_name_from_session_key(session_key?)
}

#[cfg(test)]
mod recovery_identifier_tests {
    use super::tmux_session_name_from_session_key;

    #[test]
    fn tmux_session_name_from_session_key_preserves_provider_prefixed_hosts() {
        assert_eq!(
            tmux_session_name_from_session_key(Some(
                "codex/hash123/mac-mini:AgentDesk-codex-adk-cdx"
            ))
            .as_deref(),
            Some("AgentDesk-codex-adk-cdx")
        );
        assert_eq!(
            tmux_session_name_from_session_key(Some("missing-colon")),
            None
        );
        assert_eq!(tmux_session_name_from_session_key(Some("host:   ")), None);
    }
}

#[cfg(test)]
mod dispatch_surface_tests {
    use super::{classify_delivery_state, dispatch_type_label_for_session_info};

    // #2036 Surface 1: label resolver derives the dispatch decorator string
    // straight from task_dispatches.dispatch_type so a same-thread
    // phase-gate → implementation transition does not show a stale label.
    #[test]
    fn dispatch_type_label_covers_known_types() {
        assert_eq!(
            dispatch_type_label_for_session_info(Some("implementation")),
            "구현 dispatch"
        );
        assert_eq!(
            dispatch_type_label_for_session_info(Some("phase-gate")),
            "phase-gate dispatch"
        );
        assert_eq!(
            dispatch_type_label_for_session_info(Some("review")),
            "리뷰 dispatch"
        );
        assert_eq!(
            dispatch_type_label_for_session_info(Some("review-decision")),
            "리뷰 검토 dispatch"
        );
        assert_eq!(dispatch_type_label_for_session_info(None), "dispatch");
    }

    // #2036 Surface 2: delivery_state collapses bridge-queue-pending vs
    // codex-actively-running into a single API field.
    #[test]
    fn delivery_state_dispatched_and_working_is_codex_active() {
        assert_eq!(
            classify_delivery_state(Some("dispatched"), true, true),
            "codex_active"
        );
        // #2036 review fix: when sent_at is NOT set, session.is_working reflects
        // the previous turn on the same session (bridge per-channel queue
        // window). The new dispatch hasn't been delivered to codex yet, so
        // delivery_state must be "queued", not "codex_active".
        assert_eq!(
            classify_delivery_state(Some("in_progress"), true, false),
            "queued"
        );
    }

    #[test]
    fn delivery_state_dispatched_not_working_with_sent_event_is_delivered() {
        assert_eq!(
            classify_delivery_state(Some("dispatched"), false, true),
            "delivered"
        );
    }

    #[test]
    fn delivery_state_dispatched_not_working_no_sent_event_is_queued() {
        assert_eq!(
            classify_delivery_state(Some("dispatched"), false, false),
            "queued"
        );
        assert_eq!(
            classify_delivery_state(Some("pending"), false, false),
            "queued"
        );
    }

    #[test]
    fn delivery_state_terminal_statuses_pass_through() {
        assert_eq!(
            classify_delivery_state(Some("completed"), false, true),
            "completed"
        );
        assert_eq!(
            classify_delivery_state(Some("failed"), false, false),
            "failed"
        );
        assert_eq!(
            classify_delivery_state(Some("cancelled"), false, false),
            "cancelled"
        );
    }

    #[test]
    fn delivery_state_uppercase_status_is_normalized() {
        // Defensive: callers occasionally write status values uppercase, so
        // classify_delivery_state lowercases before matching.
        // NOTE: per #2036, sent_at dominates session_is_working — so a
        // DISPATCHED turn only reads "codex_active" once sent_at is set
        // (the bridge handed the prompt to codex); with sent_at unset it is
        // still "queued". This test pins the case-normalization, not the
        // sent_at gating, so it passes sent_at_is_set=true for the active case.
        assert_eq!(
            classify_delivery_state(Some("DISPATCHED"), true, true),
            "codex_active"
        );
        assert_eq!(
            classify_delivery_state(Some("DISPATCHED"), true, false),
            "queued"
        );
        assert_eq!(
            classify_delivery_state(Some("Completed"), false, false),
            "completed"
        );
    }
}

#[cfg(test)]
mod selector_cleanup_tests {
    use super::{
        HookSessionUpsert, clear_session_id_by_key_pg, clear_stale_session_id_pg,
        disconnect_session_and_prepare_retry_pg, disconnect_stale_fixed_session_by_key_pg,
        gc_stale_fixed_working_sessions_db_pg, load_provider_session_ids_pg,
        mark_raw_provider_transcript_growth_if_observed_pg, reconcile_orphaned_tmuxless_session_pg,
        update_raw_provider_transcript_len_watermark_pg, upsert_hook_session_pg,
    };

    struct TestPostgresDb {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_selector_cleanup_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "selector cleanup tests",
            )
            .await
            .expect("create selector cleanup postgres test db"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests

            Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "selector cleanup tests",
            )
            .await
            .expect("apply selector cleanup postgres migrations") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "selector cleanup tests",
            )
            .await
            .expect("drop selector cleanup postgres test db"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    async fn seed_session_with_selectors(
        pool: &sqlx::PgPool,
        session_key: &str,
        status: &str,
        active_dispatch_id: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO sessions
             (session_key, status, active_dispatch_id, provider, last_heartbeat,
              claude_session_id, raw_provider_session_id, created_at)
             VALUES ($1, $2, $3, 'claude', NOW() - INTERVAL '7 hours',
                     'claude-selector-1841', 'raw-selector-1841',
                     NOW() - INTERVAL '7 hours')",
        )
        .bind(session_key)
        .bind(status)
        .bind(active_dispatch_id)
        .execute(pool)
        .await
        .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
    }

    async fn session_state(
        pool: &sqlx::PgPool,
        session_key: &str,
    ) -> (String, Option<String>, Option<String>, Option<String>) {
        sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
            "SELECT status, active_dispatch_id, claude_session_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(pool)
        .await
        .unwrap() // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
    }

    async fn assert_cleanup_preserved_selectors(pool: &sqlx::PgPool, session_key: &str) {
        let (status, active_dispatch_id, claude_session_id, raw_provider_session_id) =
            session_state(pool, session_key).await;

        assert_eq!(status, "disconnected");
        assert_eq!(active_dispatch_id, None);
        assert_eq!(claude_session_id.as_deref(), Some("claude-selector-1841"));
        assert_eq!(
            raw_provider_session_id.as_deref(),
            Some("raw-selector-1841")
        );
    }

    async fn upsert_claude_selector_session(
        pool: &sqlx::PgPool,
        session_key: &str,
        claude_session_id: Option<&str>,
        raw_provider_session_id: Option<&str>,
    ) {
        upsert_hook_session_pg(
            pool,
            HookSessionUpsert {
                session_key,
                instance_id: None,
                agent_id: None,
                provider: "claude",
                status: "idle",
                session_info: None,
                model: None,
                tokens: None,
                cwd: None,
                active_dispatch_id: None,
                thread_channel_id: None,
                channel_id: None,
                claude_session_id,
                raw_provider_session_id,
                turn_start_nonce: None,
                dispatched_origin: false,
            },
        )
        .await
        .expect("upsert selector session"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
    }

    #[tokio::test]
    async fn disconnect_session_and_prepare_retry_pg_preserves_provider_selectors() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:selector-force-kill";

        seed_session_with_selectors(&pool, session_key, "idle", Some("dispatch-1841")).await;

        let retry_meta = disconnect_session_and_prepare_retry_pg(&pool, session_key, None, false)
            .await
            .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert!(retry_meta.is_none());
        assert_cleanup_preserved_selectors(&pool, session_key).await;

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn gc_stale_fixed_working_sessions_db_pg_preserves_provider_selectors() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:selector-gc-stale";

        seed_session_with_selectors(&pool, session_key, "turn_active", Some("dispatch-1841-gc"))
            .await;

        assert_eq!(gc_stale_fixed_working_sessions_db_pg(&pool).await, 1);
        assert_cleanup_preserved_selectors(&pool, session_key).await;

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn disconnect_stale_fixed_session_by_key_pg_preserves_provider_selectors() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:selector-stale-by-key";

        seed_session_with_selectors(&pool, session_key, "turn_active", Some("dispatch-1841-key"))
            .await;

        assert_eq!(
            disconnect_stale_fixed_session_by_key_pg(&pool, session_key).await,
            1
        );
        assert_cleanup_preserved_selectors(&pool, session_key).await;

        pool.close().await;
        pg_db.drop().await;
    }

    /// #2861: an idle row whose tmux already vanished must be reconciled to
    /// `disconnected` (selectors preserved) so it leaves the idle-kill pool.
    #[tokio::test]
    async fn reconcile_orphaned_tmuxless_session_pg_disconnects_idle_row_preserving_selectors() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:tmuxless-idle-zombie";

        seed_session_with_selectors(&pool, session_key, "idle", None).await;

        assert!(reconcile_orphaned_tmuxless_session_pg(&pool, session_key).await);
        assert_cleanup_preserved_selectors(&pool, session_key).await;

        // Idempotent: an already-disconnected row reports no further transition.
        assert!(!reconcile_orphaned_tmuxless_session_pg(&pool, session_key).await);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #2861: a row with an in-flight dispatch is owned by force-kill / the
    /// stuck-dispatch watchdog — the stale-tmux reconcile must leave it alone.
    #[tokio::test]
    async fn reconcile_orphaned_tmuxless_session_pg_skips_rows_with_active_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:tmuxless-with-dispatch";

        seed_session_with_selectors(&pool, session_key, "idle", Some("dispatch-2861")).await;

        assert!(!reconcile_orphaned_tmuxless_session_pg(&pool, session_key).await);
        let (status, active_dispatch_id, _, _) = session_state(&pool, session_key).await;
        assert_eq!(status, "idle");
        assert_eq!(active_dispatch_id.as_deref(), Some("dispatch-2861"));

        pool.close().await;
        pg_db.drop().await;
    }

    /// #3052: a tmux-only idle cleanup (the reconcile path `/kill-tmux` runs
    /// when tmux is already gone) must leave BOTH provider resume selector
    /// columns intact, and the resume lookup (`load_provider_session_ids_pg`)
    /// must still surface them so provider-native resume can succeed.
    #[tokio::test]
    async fn tmux_only_kill_preserves_resume_selectors() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:tmux-only-resume-selector";

        seed_session_with_selectors(&pool, session_key, "idle", None).await;

        // Simulate the tmux-only idle cleanup reconcile.
        assert!(reconcile_orphaned_tmuxless_session_pg(&pool, session_key).await);

        // Both selector columns must survive the cleanup.
        assert_cleanup_preserved_selectors(&pool, session_key).await;

        // The resume lookup used by kill-tmux's resumable check and by the
        // session-restore fallback must still return both selectors.
        let ids = load_provider_session_ids_pg(&pool, session_key, Some("claude"))
            .await
            .unwrap() // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            .expect("session row must still exist after tmux-only cleanup"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert_eq!(
            ids.claude_session_id.as_deref(),
            Some("claude-selector-1841")
        );
        assert_eq!(
            ids.raw_provider_session_id.as_deref(),
            Some("raw-selector-1841"),
            "raw provider selector must survive so native resume can fall back to it"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn same_claude_session_id_heartbeats_do_not_refresh_recorded_at_guard() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:claude-selector-recorded-at";
        let claude_session_id = "c62c2dc8-0000-4000-8000-000000000000";

        upsert_claude_selector_session(&pool, session_key, Some(claude_session_id), None).await;
        sqlx::query(
            "UPDATE sessions
                SET claude_session_id_recorded_at = NOW() - INTERVAL '61 seconds',
                    last_heartbeat = NOW() - INTERVAL '61 seconds'
              WHERE session_key = $1",
        )
        .bind(session_key)
        .execute(&pool)
        .await
        .expect("age selector timestamp"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests

        upsert_claude_selector_session(&pool, session_key, Some(claude_session_id), None).await;
        upsert_claude_selector_session(&pool, session_key, Some(claude_session_id), None).await;

        let ids = load_provider_session_ids_pg(&pool, session_key, Some("claude"))
            .await
            .expect("load provider ids") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            .expect("session row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert!(
            ids.cache_entry_age_secs.unwrap_or_default() >= 60,
            "same-id heartbeats must not extend the missing-transcript grace window"
        );
        let heartbeat_age_secs: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - last_heartbeat))::BIGINT
               FROM sessions
              WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("heartbeat age"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert!(
            heartbeat_age_secs < 60,
            "heartbeat should still refresh independently of the selector timestamp"
        );

        upsert_claude_selector_session(
            &pool,
            session_key,
            Some("48fdb7f3-0000-4000-8000-000000000000"),
            None,
        )
        .await;
        let ids = load_provider_session_ids_pg(&pool, session_key, Some("claude"))
            .await
            .expect("load provider ids after selector change") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            .expect("session row after selector change"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert!(
            ids.cache_entry_age_secs.unwrap_or(i64::MAX) < 60,
            "a changed cached selector value must restart the short grace window"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn raw_provider_transcript_len_watermark_is_monotonic() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:raw-selector-watermark";
        let raw_session_id = "48fdb7f3-0000-4000-8000-000000000000";

        upsert_claude_selector_session(
            &pool,
            session_key,
            Some("c62c2dc8-0000-4000-8000-000000000000"),
            Some(raw_session_id),
        )
        .await;

        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_id,
            10,
        )
        .await
        .expect("write initial watermark"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_id,
            8,
        )
        .await
        .expect("write lower watermark"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        let ids = load_provider_session_ids_pg(&pool, session_key, Some("claude"))
            .await
            .expect("load provider ids") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            .expect("session row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert_eq!(ids.raw_provider_transcript_len_watermark, Some(10));
        assert_eq!(
            ids.raw_provider_transcript_watermark_session_id.as_deref(),
            Some(raw_session_id)
        );
        assert!(
            !ids.raw_provider_transcript_growth_proven,
            "lower/equal observations do not prove growth"
        );

        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_id,
            12,
        )
        .await
        .expect("write higher watermark"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        let ids = load_provider_session_ids_pg(&pool, session_key, Some("claude"))
            .await
            .expect("load provider ids after growth") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            .expect("session row after growth"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert_eq!(ids.raw_provider_transcript_len_watermark, Some(12));
        assert!(
            ids.raw_provider_transcript_growth_proven,
            "growth proof must stay sticky after the watermark advances to the observed length"
        );

        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_id,
            12,
        )
        .await
        .expect("record equal watermark after proof"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        let ids = load_provider_session_ids_pg(&pool, session_key, Some("claude"))
            .await
            .expect("load provider ids after equal proof") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            .expect("session row after equal proof"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert_eq!(ids.raw_provider_transcript_len_watermark, Some(12));
        assert!(
            ids.raw_provider_transcript_growth_proven,
            "recording the current final length must not erase prior growth proof"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn raw_provider_transcript_watermark_raw_id_mismatch_resets_baseline() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:raw-selector-watermark-id-reset";
        let raw_session_a = "48fdb7f3-0000-4000-8000-000000000000";
        let raw_session_b = "8f0e3a1c-0000-4000-8000-000000000000";

        upsert_claude_selector_session(
            &pool,
            session_key,
            Some("c62c2dc8-0000-4000-8000-000000000000"),
            Some(raw_session_a),
        )
        .await;
        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_a,
            10_000_000,
        )
        .await
        .expect("write raw A watermark"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_a,
            10_000_001,
        )
        .await
        .expect("prove raw A growth"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests

        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_b,
            50_000,
        )
        .await
        .expect("raw B mismatch resets baseline"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        let ids = load_provider_session_ids_pg(&pool, session_key, Some("claude"))
            .await
            .expect("load provider ids after id reset") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            .expect("session row after id reset"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests

        assert_eq!(ids.raw_provider_transcript_len_watermark, Some(50_000));
        assert_eq!(
            ids.raw_provider_transcript_watermark_session_id.as_deref(),
            Some(raw_session_b)
        );
        assert!(
            !ids.raw_provider_transcript_growth_proven,
            "a fresh raw id starts from its own baseline, not the old file's proof"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn raw_provider_transcript_growth_flag_only_observation_does_not_raise_watermark() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:raw-selector-watermark-kill-path";
        let raw_session_id = "48fdb7f3-0000-4000-8000-000000000000";

        upsert_claude_selector_session(
            &pool,
            session_key,
            Some("c62c2dc8-0000-4000-8000-000000000000"),
            Some(raw_session_id),
        )
        .await;
        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_id,
            10,
        )
        .await
        .expect("write baseline watermark"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests

        mark_raw_provider_transcript_growth_if_observed_pg(
            &pool,
            session_key,
            Some("claude"),
            raw_session_id,
            12,
        )
        .await
        .expect("kill path records sticky proof only"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        let ids = load_provider_session_ids_pg(&pool, session_key, Some("claude"))
            .await
            .expect("load provider ids after growth-only observation") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            .expect("session row after growth-only observation"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests

        assert_eq!(
            ids.raw_provider_transcript_len_watermark,
            Some(10),
            "kill-path evidence must not record the dead transcript's final length"
        );
        assert!(
            ids.raw_provider_transcript_growth_proven,
            "kill-path evidence may preserve growth proof without raising the watermark"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn clearing_session_ids_resets_raw_transcript_watermark_evidence() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let rows = [
            (
                "host:raw-selector-clear-by-id",
                "48fdb7f3-0000-4000-8000-000000000000",
            ),
            (
                "host:raw-selector-clear-by-key",
                "8f0e3a1c-0000-4000-8000-000000000000",
            ),
        ];

        for (session_key, raw_session_id) in rows {
            upsert_claude_selector_session(
                &pool,
                session_key,
                Some("c62c2dc8-0000-4000-8000-000000000000"),
                Some(raw_session_id),
            )
            .await;
            update_raw_provider_transcript_len_watermark_pg(
                &pool,
                session_key,
                Some("claude"),
                raw_session_id,
                10,
            )
            .await
            .expect("write baseline before clear"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
            update_raw_provider_transcript_len_watermark_pg(
                &pool,
                session_key,
                Some("claude"),
                raw_session_id,
                11,
            )
            .await
            .expect("prove growth before clear"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        }

        clear_stale_session_id_pg(&pool, rows[0].1)
            .await
            .expect("clear by raw session id"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        let ids =
            load_provider_session_ids_pg(&pool, "host:raw-selector-clear-by-id", Some("claude"))
                .await
                .expect("load clear-by-id row") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
                .expect("clear-by-id row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert_eq!(ids.raw_provider_session_id, None);
        assert_eq!(ids.raw_provider_transcript_len_watermark, Some(0));
        assert_eq!(ids.raw_provider_transcript_watermark_session_id, None);
        assert!(!ids.raw_provider_transcript_growth_proven);

        clear_session_id_by_key_pg(&pool, "host:raw-selector-clear-by-key")
            .await
            .expect("clear by session key"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        let ids =
            load_provider_session_ids_pg(&pool, "host:raw-selector-clear-by-key", Some("claude"))
                .await
                .expect("load clear-by-key row") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
                .expect("clear-by-key row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert_eq!(ids.raw_provider_session_id, None);
        assert_eq!(ids.raw_provider_transcript_len_watermark, Some(0));
        assert_eq!(ids.raw_provider_transcript_watermark_session_id, None);
        assert!(!ids.raw_provider_transcript_growth_proven);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #2861 (review): `/kill-tmux` is a public route, so the reconcile must
    /// only touch `idle` rows — never terminal (`aborted`) or other live-ish
    /// states (`turn_active`/`awaiting_user`/`awaiting_bg`). Those must be left
    /// for force-kill / the dispatch watchdog, not rewritten to `disconnected`.
    #[tokio::test]
    async fn reconcile_orphaned_tmuxless_session_pg_only_touches_idle_status() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        for non_idle in ["aborted", "turn_active", "awaiting_user", "awaiting_bg"] {
            let session_key = format!("host:tmuxless-{non_idle}");
            seed_session_with_selectors(&pool, &session_key, non_idle, None).await;

            assert!(!reconcile_orphaned_tmuxless_session_pg(&pool, &session_key).await);
            let (status, _, _, _) = session_state(&pool, &session_key).await;
            assert_eq!(status, non_idle, "non-idle status must not be rewritten");
        }

        pool.close().await;
        pg_db.drop().await;
    }

    /// #2045 Finding 4 cancelled→failed guard:
    /// force-kill on a session whose active dispatch is already `cancelled`
    /// must NOT overwrite the dispatch status, and must NOT synthesize a retry.
    /// The session row itself is still disconnected (operator intent).
    #[tokio::test]
    async fn disconnect_session_and_prepare_retry_pg_skips_cancelled_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:selector-cancelled-guard";
        let dispatch_id = "dispatch-2045-cancelled-guard";
        let card_id = "card-2045-cancelled-guard";

        // Seed: card + cancelled dispatch + session pointing at it.
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ($1, 'guard card', 'backlog', NOW(), NOW())",
        )
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, dispatch_type, status, title, context,
              created_at, updated_at, completed_at)
             VALUES ($1, $2, 'implementation', 'cancelled', 'guard',
                     '{}', NOW(), NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        seed_session_with_selectors(&pool, session_key, "idle", Some(dispatch_id)).await;

        // Caller asks for retry=true. Guard must reject both the failure
        // overwrite and the retry creation.
        let retry_meta =
            disconnect_session_and_prepare_retry_pg(&pool, session_key, Some(dispatch_id), true)
                .await
                .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert!(
            retry_meta.is_none(),
            "cancelled dispatch must not produce retry metadata"
        );

        // Dispatch status remains 'cancelled' (NOT overwritten to 'failed').
        let dispatch_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_one(&pool)
                .await
                .unwrap(); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] mod selector_cleanup_tests
        assert_eq!(
            dispatch_status, "cancelled",
            "force-kill must not overwrite cancelled → failed"
        );

        // Session row is still disconnected (force-kill side effect is fine).
        let (session_status, active_dispatch_id, _, _) = session_state(&pool, session_key).await;
        assert_eq!(session_status, "disconnected");
        assert_eq!(active_dispatch_id, None);

        pool.close().await;
        pg_db.drop().await;
    }
}

pub(crate) async fn load_session_event_payload_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<serde_json::Value>, String> {
    let row = sqlx::query(
        "SELECT
            s.id,
            s.session_key,
            s.instance_id,
            s.agent_id,
            s.provider,
            s.status,
            s.active_dispatch_id,
            s.model,
            s.tokens,
            s.cwd,
            s.last_heartbeat,
            s.session_info,
            a.department,
            a.sprite_number,
            a.avatar_emoji,
            COALESCE(a.xp, 0)::BIGINT AS stats_xp,
            s.thread_channel_id,
            d.name AS department_name,
            d.name_ko AS department_name_ko,
            d.color AS department_color
         FROM sessions s
         LEFT JOIN agents a ON s.agent_id = a.id
         LEFT JOIN departments d ON a.department = d.id
         WHERE s.session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres session event payload for {session_key}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let id: i64 = row
        .try_get("id")
        .map_err(|error| format!("decode postgres session event id for {session_key}: {error}"))?;
    let session_key_value: Option<String> = row.try_get("session_key").map_err(|error| {
        format!("decode postgres session_key for session event {session_key}: {error}")
    })?;
    let last_seen_at: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("last_heartbeat").map_err(|error| {
            format!("decode postgres last_heartbeat for session event {session_key}: {error}")
        })?;

    Ok(Some(json!({
        "id": id.to_string(),
        "session_key": session_key_value,
        "instance_id": row.try_get::<Option<String>, _>("instance_id").map_err(|error| format!("decode postgres instance_id for session event {session_key}: {error}"))?,
        "name": session_key_value,
        "linked_agent_id": row.try_get::<Option<String>, _>("agent_id").map_err(|error| format!("decode postgres agent_id for session event {session_key}: {error}"))?,
        "provider": row.try_get::<Option<String>, _>("provider").map_err(|error| format!("decode postgres provider for session event {session_key}: {error}"))?,
        "status": row.try_get::<Option<String>, _>("status").map_err(|error| format!("decode postgres status for session event {session_key}: {error}"))?,
        "active_dispatch_id": row.try_get::<Option<String>, _>("active_dispatch_id").map_err(|error| format!("decode postgres active_dispatch_id for session event {session_key}: {error}"))?,
        "model": row.try_get::<Option<String>, _>("model").map_err(|error| format!("decode postgres model for session event {session_key}: {error}"))?,
        "tokens": row.try_get::<i64, _>("tokens").map_err(|error| format!("decode postgres tokens for session event {session_key}: {error}"))?,
        "cwd": row.try_get::<Option<String>, _>("cwd").map_err(|error| format!("decode postgres cwd for session event {session_key}: {error}"))?,
        "last_seen_at": last_seen_at.map(|value| value.to_rfc3339()),
        "session_info": row.try_get::<Option<String>, _>("session_info").map_err(|error| format!("decode postgres session_info for session event {session_key}: {error}"))?,
        "department_id": row.try_get::<Option<String>, _>("department").map_err(|error| format!("decode postgres department for session event {session_key}: {error}"))?,
        "sprite_number": row.try_get::<Option<i64>, _>("sprite_number").map_err(|error| format!("decode postgres sprite_number for session event {session_key}: {error}"))?,
        "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").map_err(|error| format!("decode postgres avatar_emoji for session event {session_key}: {error}"))?.unwrap_or_else(|| "\u{1F916}".to_string()),
        "stats_xp": row.try_get::<i64, _>("stats_xp").map_err(|error| format!("decode postgres stats_xp for session event {session_key}: {error}"))?,
        "thread_channel_id": row.try_get::<Option<String>, _>("thread_channel_id").map_err(|error| format!("decode postgres thread_channel_id for session event {session_key}: {error}"))?,
        "department_name": row.try_get::<Option<String>, _>("department_name").map_err(|error| format!("decode postgres department_name for session event {session_key}: {error}"))?,
        "department_name_ko": row.try_get::<Option<String>, _>("department_name_ko").map_err(|error| format!("decode postgres department_name_ko for session event {session_key}: {error}"))?,
        "department_color": row.try_get::<Option<String>, _>("department_color").map_err(|error| format!("decode postgres department_color for session event {session_key}: {error}"))?,
        "connected_at": null,
    })))
}

pub(crate) async fn load_agent_status_payload_pg(
    pool: &PgPool,
    agent_id: &str,
    session_key: &str,
) -> Result<Option<serde_json::Value>, String> {
    let row = sqlx::query(
        "SELECT
            a.id,
            a.name,
            a.name_ko,
            s.status,
            s.session_info,
            a.provider AS cli_provider,
            a.avatar_emoji,
            a.department,
            a.discord_channel_id,
            a.discord_channel_alt,
            a.discord_channel_cc,
            a.discord_channel_cdx
         FROM agents a
         LEFT JOIN sessions s
           ON s.agent_id = a.id
          AND s.session_key = $2
         WHERE a.id = $1",
    )
    .bind(agent_id)
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        format!("load postgres agent status payload for {agent_id}/{session_key}: {error}")
    })?;

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode postgres agent id for {agent_id}: {error}"))?,
        "name": row.try_get::<String, _>("name").map_err(|error| format!("decode postgres agent name for {agent_id}: {error}"))?,
        "name_ko": row.try_get::<Option<String>, _>("name_ko").map_err(|error| format!("decode postgres agent name_ko for {agent_id}: {error}"))?,
        "status": row.try_get::<Option<String>, _>("status").map_err(|error| format!("decode postgres agent status for {agent_id}: {error}"))?,
        "session_info": row.try_get::<Option<String>, _>("session_info").map_err(|error| format!("decode postgres agent session_info for {agent_id}: {error}"))?,
        "cli_provider": row.try_get::<Option<String>, _>("cli_provider").map_err(|error| format!("decode postgres cli_provider for {agent_id}: {error}"))?,
        "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").map_err(|error| format!("decode postgres avatar_emoji for {agent_id}: {error}"))?,
        "department": row.try_get::<Option<String>, _>("department").map_err(|error| format!("decode postgres department for {agent_id}: {error}"))?,
        "discord_channel_id": row.try_get::<Option<String>, _>("discord_channel_id").map_err(|error| format!("decode postgres discord_channel_id for {agent_id}: {error}"))?,
        "discord_channel_alt": row.try_get::<Option<String>, _>("discord_channel_alt").map_err(|error| format!("decode postgres discord_channel_alt for {agent_id}: {error}"))?,
        "discord_channel_cc": row.try_get::<Option<String>, _>("discord_channel_cc").map_err(|error| format!("decode postgres discord_channel_cc for {agent_id}: {error}"))?,
        "discord_channel_cdx": row.try_get::<Option<String>, _>("discord_channel_cdx").map_err(|error| format!("decode postgres discord_channel_cdx for {agent_id}: {error}"))?,
        "discord_channel_id_codex": row.try_get::<Option<String>, _>("discord_channel_cdx").map_err(|error| format!("decode postgres discord_channel_id_codex for {agent_id}: {error}"))?,
    })))
}

pub(crate) async fn load_session_update_payload_pg(
    pool: &PgPool,
    id: i64,
) -> Result<Option<serde_json::Value>, String> {
    let row = sqlx::query(
        "SELECT
            id,
            session_key,
            instance_id,
            agent_id,
            status,
            provider,
            session_info,
            model,
            tokens,
            cwd,
            active_dispatch_id,
            last_heartbeat
         FROM sessions
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres session update payload for {id}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let last_heartbeat: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("last_heartbeat").map_err(|error| {
            format!("decode postgres last_heartbeat for session update {id}: {error}")
        })?;

    Ok(Some(json!({
        "id": row.try_get::<i64, _>("id").map_err(|error| format!("decode postgres session id for update {id}: {error}"))?.to_string(),
        "session_key": row.try_get::<String, _>("session_key").map_err(|error| format!("decode postgres session_key for update {id}: {error}"))?,
        "instance_id": row.try_get::<Option<String>, _>("instance_id").map_err(|error| format!("decode postgres instance_id for update {id}: {error}"))?,
        "agent_id": row.try_get::<Option<String>, _>("agent_id").map_err(|error| format!("decode postgres agent_id for update {id}: {error}"))?,
        "status": row.try_get::<Option<String>, _>("status").map_err(|error| format!("decode postgres status for update {id}: {error}"))?,
        "provider": row.try_get::<Option<String>, _>("provider").map_err(|error| format!("decode postgres provider for update {id}: {error}"))?,
        "session_info": row.try_get::<Option<String>, _>("session_info").map_err(|error| format!("decode postgres session_info for update {id}: {error}"))?,
        "model": row.try_get::<Option<String>, _>("model").map_err(|error| format!("decode postgres model for update {id}: {error}"))?,
        "tokens": row.try_get::<i64, _>("tokens").map_err(|error| format!("decode postgres tokens for update {id}: {error}"))?,
        "cwd": row.try_get::<Option<String>, _>("cwd").map_err(|error| format!("decode postgres cwd for update {id}: {error}"))?,
        "active_dispatch_id": row.try_get::<Option<String>, _>("active_dispatch_id").map_err(|error| format!("decode postgres active_dispatch_id for update {id}: {error}"))?,
        "last_heartbeat": last_heartbeat.map(|value| value.to_rfc3339()),
    })))
}

async fn backfill_legacy_thread_channel_ids_pg(pool: &PgPool) -> usize {
    let session_keys = match sqlx::query_scalar::<_, String>(
        "SELECT session_key
         FROM sessions
         WHERE thread_channel_id IS NULL",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] backfill_legacy_thread_channel_ids_pg: failed to load session keys: {error}"
            );
            return 0;
        }
    };

    let mut updated = 0usize;
    for session_key in session_keys {
        let Some(thread_channel_id) = parse_thread_channel_id_from_session_key(&session_key) else {
            continue;
        };

        match sqlx::query(
            "UPDATE sessions
             SET thread_channel_id = $1
             WHERE session_key = $2
               AND thread_channel_id IS NULL",
        )
        .bind(&thread_channel_id)
        .bind(&session_key)
        .execute(pool)
        .await
        {
            Ok(result) => updated += result.rows_affected() as usize,
            Err(error) => tracing::warn!(
                "[dispatched-sessions] backfill_legacy_thread_channel_ids_pg: failed to update {}: {}",
                session_key,
                error
            ),
        }
    }

    updated
}

/// Delete stale thread session rows and return the `session_key`s removed so
/// the caller can reap the matching orphan tmux sessions. The inner CLI of a
/// thread tmux session usually stays at an interactive prompt (its pane never
/// goes dead), so the dead-pane reaper can't reap it; and once this GC removes
/// the row, the idle-kill policy can no longer see it either. Returning the
/// deleted keys lets the periodic GC kill those tmux sessions directly.
pub async fn gc_stale_thread_sessions_pg(pool: &PgPool) -> Vec<String> {
    let _ = backfill_legacy_thread_channel_ids_pg(pool).await;
    match sqlx::query_scalar::<_, String>(
        "DELETE FROM sessions
         WHERE thread_channel_id IS NOT NULL
           AND status IN ('idle', 'awaiting_user', 'disconnected', 'aborted')
           AND (
             (active_dispatch_id IS NULL
               AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '1 hour')
             OR COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '3 hours'
           )
         RETURNING session_key",
    )
    .fetch_all(pool)
    .await
    {
        Ok(keys) => keys,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_thread_sessions_pg: failed to delete stale sessions: {error}"
            );
            Vec::new()
        }
    }
}

/// Reconcile an **idle** session row whose tmux session has already vanished.
///
/// When `kill-tmux` discovers `tmux_was_alive == false`, an idle row claims a
/// live provider process that no longer exists. Left as-is (status `idle`), such
/// a row stays in the `idle-kill` candidate pool forever and — being among the
/// oldest — monopolizes the per-tick kill budget, starving genuinely-alive idle
/// sessions behind it (#2861). Transition it to `disconnected` while preserving
/// provider selectors (claude_session_id etc.) so resume on the next user
/// message still works via the selector path.
///
/// The guard is deliberately tight: **only `status = 'idle'` rows with no
/// in-flight dispatch are touched.** `kill-tmux` is a public API route, so a
/// caller could hit a tmuxless `completed`/`failed`/`cancelled`/`aborted` row;
/// those terminal/history states must NOT be rewritten to `disconnected`.
/// Sessions with an active dispatch are owned by force-kill / the stuck-dispatch
/// watchdog, not this reconcile.
///
/// Sibling of `mark_session_disconnected_for_idle_cleanup` in the discord
/// module (in-memory expiry path); both preserve selectors but guard differently.
/// Returns true if a row transitioned.
pub(crate) async fn reconcile_orphaned_tmuxless_session_pg(
    pool: &PgPool,
    session_key: &str,
) -> bool {
    match sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected'
         WHERE session_key = $1
           AND status = 'idle'
           AND active_dispatch_id IS NULL",
    )
    .bind(session_key)
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() > 0,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] reconcile_orphaned_tmuxless_session_pg: failed for {}: {}",
                session_key,
                error
            );
            false
        }
    }
}

/// Return the effective "last seen" instant idle-kill selects on for a session
/// (`COALESCE(last_heartbeat, created_at)`), as a unix-epoch nanosecond count.
/// Used by the kill-time live-activity guard (#3053) to compare against runtime
/// file mtimes (relay output / generation marker / provider transcript).
/// `None` when the row is absent or the timestamp cannot be decoded.
pub(crate) async fn session_last_seen_unix_nanos_pg(
    pool: &PgPool,
    session_key: &str,
) -> Option<i64> {
    let last_seen: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT COALESCE(last_heartbeat, created_at) FROM sessions WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    last_seen.map(|value| value.timestamp_nanos_opt().unwrap_or(0))
}

/// Refresh `sessions.last_heartbeat` to the exact runtime activity timestamp.
/// This keeps idle cleanup anchored to "last output" rather than the later
/// cleanup tick that noticed the missed heartbeat.
pub(crate) async fn refresh_session_heartbeat_by_key_to_unix_nanos_pg(
    pool: &PgPool,
    session_key: &str,
    unix_nanos: i64,
) -> bool {
    let secs = unix_nanos.div_euclid(1_000_000_000);
    let nanos = unix_nanos.rem_euclid(1_000_000_000) as u32;
    let Some(activity_at) = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos) else {
        return false;
    };
    match sqlx::query(
        "UPDATE sessions
         SET last_heartbeat = GREATEST(
             COALESCE(last_heartbeat, TIMESTAMPTZ 'epoch'),
             $2
         )
         WHERE session_key = $1",
    )
    .bind(session_key)
    .bind(activity_at)
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() > 0,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] refresh_session_heartbeat_by_key_to_unix_nanos_pg: failed for {}: {}",
                session_key,
                error
            );
            false
        }
    }
}

/// Mark stale fixed-channel working sessions as disconnected without clearing
/// provider selectors needed for resume after runtime cleanup.
pub async fn gc_stale_fixed_working_sessions_db_pg(pool: &PgPool) -> usize {
    let stale_dispatches = match sqlx::query_scalar::<_, String>(
        "SELECT active_dispatch_id
         FROM sessions
         WHERE thread_channel_id IS NULL
           AND status IN ('working', 'turn_active')
           AND active_dispatch_id IS NOT NULL
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_fixed_working_sessions_db_pg: failed to load stale dispatches: {error}"
            );
            return 0;
        }
    };

    for dispatch_id in stale_dispatches {
        if let Err(error) = sqlx::query(
            "UPDATE task_dispatches
             SET status = 'failed',
                 updated_at = NOW(),
                 last_stuck_alert_at = NULL,
                 completed_at = COALESCE(completed_at, NOW())
             WHERE id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&dispatch_id)
        .execute(pool)
        .await
        {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_fixed_working_sessions_db_pg: failed to mark stale dispatch {} as failed: {}",
                dispatch_id,
                error
            );
        }
    }

    match sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL
         WHERE thread_channel_id IS NULL
           AND status IN ('working', 'turn_active')
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours'",
    )
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() as usize,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_fixed_working_sessions_db_pg: failed to disconnect stale sessions: {error}"
            );
            0
        }
    }
}

pub(crate) async fn disconnect_stale_fixed_session_by_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> usize {
    let stale_dispatches = match sqlx::query_scalar::<_, String>(
        "SELECT active_dispatch_id
         FROM sessions
         WHERE session_key = $1
           AND thread_channel_id IS NULL
           AND status IN ('working', 'turn_active')
           AND active_dispatch_id IS NOT NULL
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours'",
    )
    .bind(session_key)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] disconnect_stale_fixed_session_by_key_pg: failed to load stale dispatches for {}: {}",
                session_key,
                error
            );
            return 0;
        }
    };

    for dispatch_id in stale_dispatches {
        if let Err(error) = sqlx::query(
            "UPDATE task_dispatches
             SET status = 'failed',
                 updated_at = NOW(),
                 last_stuck_alert_at = NULL,
                 completed_at = COALESCE(completed_at, NOW())
             WHERE id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&dispatch_id)
        .execute(pool)
        .await
        {
            tracing::warn!(
                "[dispatched-sessions] disconnect_stale_fixed_session_by_key_pg: failed to mark stale dispatch {} as failed: {}",
                dispatch_id,
                error
            );
        }
    }

    match sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL
         WHERE session_key = $1
           AND thread_channel_id IS NULL
           AND status IN ('working', 'turn_active')
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours'",
    )
    .bind(session_key)
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() as usize,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] disconnect_stale_fixed_session_by_key_pg: failed to disconnect stale session {}: {}",
                session_key,
                error
            );
            0
        }
    }
}
pub(crate) async fn load_session_by_id_pg(
    pool: &PgPool,
    id: i64,
) -> Result<
    Option<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )>,
    String,
> {
    let row = sqlx::query(
        "SELECT session_key, agent_id, provider, status, instance_id
         FROM sessions
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres session #{id}: {error}"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let session_key: Option<String> = row
        .try_get("session_key")
        .map_err(|error| format!("decode session_key for #{id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode agent_id for #{id}: {error}"))?;
    let provider: Option<String> = row
        .try_get("provider")
        .map_err(|error| format!("decode provider for #{id}: {error}"))?;
    let status: Option<String> = row
        .try_get("status")
        .map_err(|error| format!("decode status for #{id}: {error}"))?;
    let instance_id: Option<String> = row
        .try_get("instance_id")
        .map_err(|error| format!("decode instance_id for #{id}: {error}"))?;
    let Some(session_key) = session_key else {
        return Ok(None);
    };
    Ok(Some((session_key, agent_id, provider, status, instance_id)))
}

pub(crate) struct HookSessionUpsert<'a> {
    pub(crate) session_key: &'a str,
    pub(crate) instance_id: Option<&'a str>,
    pub(crate) agent_id: Option<&'a str>,
    pub(crate) provider: &'a str,
    pub(crate) status: &'a str,
    pub(crate) session_info: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    /// `None` means "metadata-only hook (e.g. provider session id save) —
    /// do not touch `sessions.tokens` or `sessions.tokens_updated_at`".
    /// `Some` means "authoritative turn-end snapshot — overwrite both".
    /// Avoids the prior `i64` design where `0` meant both "no data" and
    /// "really zero" and silently zeroed out real values.
    pub(crate) tokens: Option<i64>,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) active_dispatch_id: Option<&'a str>,
    pub(crate) thread_channel_id: Option<&'a str>,
    /// #3207 (part 2) P0: the unique Discord channel the turn runs in (thread id
    /// for threads, channel id for ordinary channels). Persisted so worktree
    /// reuse can require an exact channel match and never cross two channels
    /// whose names collide onto the same `session_key`.
    pub(crate) channel_id: Option<&'a str>,
    pub(crate) claude_session_id: Option<&'a str>,
    pub(crate) raw_provider_session_id: Option<&'a str>,
    pub(crate) turn_start_nonce: Option<&'a str>,
    pub(crate) dispatched_origin: bool,
}

pub(crate) struct DeleteSessionResult {
    pub(crate) session_id: Option<i64>,
    pub(crate) deleted: u64,
}

pub(crate) struct ProviderSessionIds {
    pub(crate) claude_session_id: Option<String>,
    pub(crate) raw_provider_session_id: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) cache_entry_age_secs: Option<i64>,
    pub(crate) raw_provider_transcript_len_watermark: Option<i64>,
    pub(crate) raw_provider_transcript_watermark_session_id: Option<String>,
    pub(crate) raw_provider_transcript_growth_proven: bool,
}

pub(crate) struct UpdateSessionParams<'a> {
    pub(crate) status: Option<&'a str>,
    pub(crate) active_dispatch_id: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) tokens: Option<i64>,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) session_info: Option<&'a str>,
}

/// Upsert a hook session row.
///
/// #2045 Finding 7 (P2): the helper now returns whether the row was inserted
/// (`Ok(true)`) or already existed (`Ok(false)`). The previous design relied
/// on a separate `session_exists_pg` SELECT in the caller to decide between
/// `dispatched_session_new` and `dispatched_session_update` WS broadcasts,
/// which races with concurrent hook calls on the same `session_key`. The new
/// signature decides inside the same INSERT statement via `xmax = 0` so the
/// caller can emit the correct WS event even under cluster hand-off.
pub(crate) async fn upsert_hook_session_pg(
    pool: &PgPool,
    params: HookSessionUpsert<'_>,
) -> Result<bool, String> {
    // `tokens` is now an `Option<i64>`. The UPSERT preserves the previous
    // value when the caller didn't supply one (metadata-only hook), and only
    // refreshes `tokens_updated_at` when an explicit snapshot arrives.
    let row = sqlx::query(
        "INSERT INTO sessions (
            session_key,
            instance_id,
            agent_id,
            provider,
            status,
            session_info,
            model,
            tokens,
            tokens_updated_at,
            cwd,
            active_dispatch_id,
            thread_channel_id,
            channel_id,
            claude_session_id,
            raw_provider_session_id,
            active_turn_nonce,
            dispatched_origin_turn_nonce,
            claude_session_id_recorded_at,
            last_heartbeat
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7,
            COALESCE($8, 0),
            CASE WHEN $8 IS NOT NULL THEN NOW() ELSE NULL END,
            $9, $10, $11, $12, $13, $14, $15,
            CASE WHEN $16 THEN $15 ELSE NULL END,
            CASE WHEN NULLIF(BTRIM($13), '') IS NOT NULL THEN NOW() ELSE NULL END,
            NOW()
         )
         ON CONFLICT(session_key) DO UPDATE SET
            status = EXCLUDED.status,
            instance_id = COALESCE(NULLIF(BTRIM(EXCLUDED.instance_id), ''), sessions.instance_id),
            provider = EXCLUDED.provider,
            session_info = COALESCE(EXCLUDED.session_info, sessions.session_info),
            model = COALESCE(EXCLUDED.model, sessions.model),
            tokens = CASE WHEN $8 IS NOT NULL THEN EXCLUDED.tokens ELSE sessions.tokens END,
            tokens_updated_at = CASE WHEN $8 IS NOT NULL THEN NOW() ELSE sessions.tokens_updated_at END,
            cwd = COALESCE(EXCLUDED.cwd, sessions.cwd),
            active_dispatch_id = CASE
              WHEN lower(EXCLUDED.status) IN ('disconnected', 'aborted') THEN NULL
              WHEN EXCLUDED.active_dispatch_id IS NOT NULL THEN EXCLUDED.active_dispatch_id
              ELSE sessions.active_dispatch_id
            END,
            agent_id = COALESCE(NULLIF(BTRIM(EXCLUDED.agent_id), ''), NULLIF(BTRIM(sessions.agent_id), '')),
            thread_channel_id = COALESCE(EXCLUDED.thread_channel_id, sessions.thread_channel_id),
            channel_id = COALESCE(EXCLUDED.channel_id, sessions.channel_id),
            claude_session_id = COALESCE(EXCLUDED.claude_session_id, sessions.claude_session_id),
            claude_session_id_recorded_at = CASE
              WHEN EXCLUDED.claude_session_id IS NULL THEN sessions.claude_session_id_recorded_at
              WHEN sessions.claude_session_id IS DISTINCT FROM EXCLUDED.claude_session_id THEN NOW()
              ELSE COALESCE(sessions.claude_session_id_recorded_at, NOW())
            END,
            raw_provider_session_id = COALESCE(EXCLUDED.raw_provider_session_id, sessions.raw_provider_session_id),
            active_turn_nonce = COALESCE(EXCLUDED.active_turn_nonce, sessions.active_turn_nonce),
            dispatched_origin_turn_nonce = CASE
              WHEN EXCLUDED.active_turn_nonce IS NULL THEN sessions.dispatched_origin_turn_nonce
              WHEN EXCLUDED.dispatched_origin_turn_nonce IS NULL THEN NULL
              ELSE EXCLUDED.dispatched_origin_turn_nonce
            END,
            last_heartbeat = NOW()
         RETURNING (xmax = 0) AS inserted",
    )
    .bind(params.session_key)
    .bind(params.instance_id)
    .bind(params.agent_id)
    .bind(params.provider)
    .bind(params.status)
    .bind(params.session_info)
    .bind(params.model)
    .bind(params.tokens)
    .bind(params.cwd)
    .bind(params.active_dispatch_id)
    .bind(params.thread_channel_id)
    .bind(params.channel_id)
    .bind(params.claude_session_id)
    .bind(params.raw_provider_session_id)
    .bind(params.turn_start_nonce)
    .bind(params.dispatched_origin)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("upsert postgres session {}: {error}", params.session_key))?;
    row.try_get::<bool, _>("inserted").map_err(|error| {
        format!(
            "decode upsert outcome for session {}: {error}",
            params.session_key
        )
    })
}

pub(crate) async fn cleanup_disconnected_sessions_pg(pool: &PgPool) -> Result<u64, String> {
    sqlx::query("DELETE FROM sessions WHERE status = 'disconnected'")
        .execute(pool)
        .await
        .map(|result| result.rows_affected())
        .map_err(|error| format!("{error}"))
}

pub(crate) async fn delete_session_by_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<DeleteSessionResult, String> {
    let session_id = sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
        .bind(session_key)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("{error}"))?;

    let deleted = sqlx::query("DELETE FROM sessions WHERE session_key = $1")
        .bind(session_key)
        .execute(pool)
        .await
        .map_err(|error| format!("{error}"))?
        .rows_affected();

    Ok(DeleteSessionResult {
        session_id,
        deleted,
    })
}

pub(crate) async fn load_provider_session_ids_pg(
    pool: &PgPool,
    session_key: &str,
    provider: Option<&str>,
) -> Result<Option<ProviderSessionIds>, String> {
    let result = if let Some(provider) = provider {
        sqlx::query(
            "SELECT claude_session_id, raw_provider_session_id, cwd,
                    EXTRACT(EPOCH FROM (NOW() - COALESCE(claude_session_id_recorded_at, created_at)))::BIGINT
                        AS cache_entry_age_secs,
                    raw_provider_transcript_len_watermark,
                    raw_provider_transcript_watermark_session_id,
                    raw_provider_transcript_growth_proven
             FROM sessions
             WHERE session_key = $1 AND provider = $2",
        )
        .bind(session_key)
        .bind(provider)
        .fetch_optional(pool)
        .await
    } else {
        sqlx::query(
            "SELECT claude_session_id, raw_provider_session_id, cwd,
                    EXTRACT(EPOCH FROM (NOW() - COALESCE(claude_session_id_recorded_at, created_at)))::BIGINT
                        AS cache_entry_age_secs,
                    raw_provider_transcript_len_watermark,
                    raw_provider_transcript_watermark_session_id,
                    raw_provider_transcript_growth_proven
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_optional(pool)
        .await
    };

    let row = result.map_err(|error| format!("{error}"))?;
    row.map(|row| {
        Ok(ProviderSessionIds {
            claude_session_id: row.try_get("claude_session_id")?,
            raw_provider_session_id: row.try_get("raw_provider_session_id")?,
            cwd: row.try_get("cwd")?,
            cache_entry_age_secs: row.try_get("cache_entry_age_secs")?,
            raw_provider_transcript_len_watermark: row
                .try_get("raw_provider_transcript_len_watermark")?,
            raw_provider_transcript_watermark_session_id: row
                .try_get("raw_provider_transcript_watermark_session_id")?,
            raw_provider_transcript_growth_proven: row
                .try_get("raw_provider_transcript_growth_proven")?,
        })
    })
    .transpose()
    .map_err(|error: sqlx::Error| format!("{error}"))
}

pub(crate) async fn update_raw_provider_transcript_len_watermark_pg(
    pool: &PgPool,
    session_key: &str,
    provider: Option<&str>,
    raw_provider_session_id: &str,
    observed_len: u64,
) -> Result<u64, String> {
    let observed_len = i64::try_from(observed_len).unwrap_or(i64::MAX);
    sqlx::query(
        "UPDATE sessions
         SET raw_provider_transcript_len_watermark = CASE
               WHEN NULLIF(BTRIM($3), '') IS NULL THEN raw_provider_transcript_len_watermark
               WHEN NULLIF(BTRIM(raw_provider_transcript_watermark_session_id), '')
                    IS DISTINCT FROM NULLIF(BTRIM($3), '') THEN $4
               ELSE GREATEST(COALESCE(raw_provider_transcript_len_watermark, 0), $4)
             END,
             raw_provider_transcript_watermark_session_id = CASE
               WHEN NULLIF(BTRIM($3), '') IS NULL THEN raw_provider_transcript_watermark_session_id
               ELSE NULLIF(BTRIM($3), '')
             END,
             raw_provider_transcript_growth_proven = CASE
               WHEN NULLIF(BTRIM($3), '') IS NULL THEN raw_provider_transcript_growth_proven
               WHEN NULLIF(BTRIM(raw_provider_transcript_watermark_session_id), '')
                    IS DISTINCT FROM NULLIF(BTRIM($3), '') THEN FALSE
               ELSE COALESCE(raw_provider_transcript_growth_proven, FALSE)
                    OR ($4 > COALESCE(raw_provider_transcript_len_watermark, 0))
             END
         WHERE session_key = $1
           AND ($2::TEXT IS NULL OR provider = $2)",
    )
    .bind(session_key)
    .bind(provider)
    .bind(raw_provider_session_id)
    .bind(observed_len)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("{error}"))
}

pub(crate) async fn mark_raw_provider_transcript_growth_if_observed_pg(
    pool: &PgPool,
    session_key: &str,
    provider: Option<&str>,
    raw_provider_session_id: &str,
    observed_len: u64,
) -> Result<u64, String> {
    let observed_len = i64::try_from(observed_len).unwrap_or(i64::MAX);
    sqlx::query(
        "UPDATE sessions
         SET raw_provider_transcript_growth_proven = CASE
               WHEN NULLIF(BTRIM(raw_provider_transcript_watermark_session_id), '')
                    = NULLIF(BTRIM($3), '')
                AND $4 > COALESCE(raw_provider_transcript_len_watermark, 0)
               THEN TRUE
               ELSE raw_provider_transcript_growth_proven
             END
         WHERE session_key = $1
           AND ($2::TEXT IS NULL OR provider = $2)",
    )
    .bind(session_key)
    .bind(provider)
    .bind(raw_provider_session_id)
    .bind(observed_len)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("{error}"))
}

pub(crate) async fn clear_stale_session_id_pg(
    pool: &PgPool,
    session_id: &str,
) -> Result<u64, String> {
    sqlx::query(
        "UPDATE sessions
         SET claude_session_id = NULL,
             raw_provider_session_id = NULL,
             claude_session_id_recorded_at = NULL,
             raw_provider_transcript_len_watermark = 0,
             raw_provider_transcript_watermark_session_id = NULL,
             raw_provider_transcript_growth_proven = FALSE
         WHERE claude_session_id = $1
            OR raw_provider_session_id = $1",
    )
    .bind(session_id)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("{error}"))
}

pub(crate) async fn clear_session_id_by_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<u64, String> {
    sqlx::query(
        "UPDATE sessions
         SET claude_session_id = NULL,
             raw_provider_session_id = NULL,
             claude_session_id_recorded_at = NULL,
             raw_provider_transcript_len_watermark = 0,
             raw_provider_transcript_watermark_session_id = NULL,
             raw_provider_transcript_growth_proven = FALSE
         WHERE session_key = $1",
    )
    .bind(session_key)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("{error}"))
}

pub(crate) async fn update_session_pg(
    pool: &PgPool,
    id: i64,
    params: UpdateSessionParams<'_>,
) -> Result<u64, String> {
    // #2045 Finding 6 (P1): mirror `upsert_hook_session_pg` semantics so PATCH
    // callers cannot leave a zombie `active_dispatch_id` linked to a session
    // they just transitioned to `disconnected`/`aborted`, and so the PATCH
    // bumps `last_heartbeat` the same way the hook does. Without these two,
    // PATCH self-reports from a worker would leave dashboard rows displaying
    // stale active dispatches and the SessionActivityResolver would treat the
    // session as inactive even when the caller has just provided a fresh
    // state report.
    sqlx::query(
        "UPDATE sessions
         SET status = COALESCE($1, status),
             active_dispatch_id = CASE
                 WHEN $1 IS NOT NULL AND lower($1) IN ('disconnected', 'aborted') THEN NULL
                 WHEN $2 IS NOT NULL THEN $2
                 ELSE active_dispatch_id
             END,
             model = COALESCE($3, model),
             tokens = COALESCE($4, tokens),
             cwd = COALESCE($5, cwd),
             session_info = COALESCE($6, session_info),
             last_heartbeat = NOW()
         WHERE id = $7",
    )
    .bind(params.status)
    .bind(params.active_dispatch_id)
    .bind(params.model)
    .bind(params.tokens)
    .bind(params.cwd)
    .bind(params.session_info)
    .bind(id)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("{error}"))
}
