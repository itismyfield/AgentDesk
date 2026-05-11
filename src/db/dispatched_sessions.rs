use serde_json::json;
use sqlx::{PgPool, Row};

use crate::db::agents::resolve_agent_channel_for_provider_pg;
use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
};
use crate::server::routes::session_activity::SessionActivityResolver;

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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn load_dispatch_thread_id_sqlite(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
) -> Option<String> {
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
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
    .map_err(|error| format!("load postgres session {session_key}: {error}"))?;

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
                format!(
                    "canonical fail postgres dispatch {dispatch_id} during force-kill: {error}"
                )
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
        let dispatch_type: Option<String> =
            row.try_get("dispatch_type").map_err(|error| {
                format!("decode postgres dispatch_type for session {id}: {error}")
            })?;
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
    let (_, tmux_session) = session_key?.split_once(':')?;
    let tmux_session = tmux_session.trim();
    (!tmux_session.is_empty()).then(|| tmux_session.to_string())
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
        // Defensive: callers occasionally write status values uppercase.
        assert_eq!(
            classify_delivery_state(Some("DISPATCHED"), true, false),
            "codex_active"
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
        disconnect_session_and_prepare_retry_pg, disconnect_stale_fixed_session_by_key_pg,
        gc_stale_fixed_working_sessions_db_pg,
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
            .expect("create selector cleanup postgres test db");

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
            .expect("apply selector cleanup postgres migrations")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "selector cleanup tests",
            )
            .await
            .expect("drop selector cleanup postgres test db");
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
        .unwrap();
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
        .unwrap()
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

    #[tokio::test]
    async fn disconnect_session_and_prepare_retry_pg_preserves_provider_selectors() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:selector-force-kill";

        seed_session_with_selectors(&pool, session_key, "idle", Some("dispatch-1841")).await;

        let retry_meta = disconnect_session_and_prepare_retry_pg(&pool, session_key, None, false)
            .await
            .unwrap();
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
            "INSERT INTO kanban_cards (id, title, current_status, created_at, updated_at)
             VALUES ($1, 'guard card', 'backlog', NOW(), NOW())",
        )
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();
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
        .unwrap();
        seed_session_with_selectors(&pool, session_key, "idle", Some(dispatch_id)).await;

        // Caller asks for retry=true. Guard must reject both the failure
        // overwrite and the retry creation.
        let retry_meta = disconnect_session_and_prepare_retry_pg(
            &pool,
            session_key,
            Some(dispatch_id),
            true,
        )
        .await
        .unwrap();
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
                .unwrap();
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

pub async fn gc_stale_thread_sessions_pg(pool: &PgPool) -> usize {
    let _ = backfill_legacy_thread_channel_ids_pg(pool).await;
    match sqlx::query(
        "DELETE FROM sessions
         WHERE thread_channel_id IS NOT NULL
           AND status IN ('idle', 'awaiting_user', 'disconnected', 'aborted')
           AND (
             (active_dispatch_id IS NULL
               AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '1 hour')
             OR COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '3 hours'
           )",
    )
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() as usize,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_thread_sessions_pg: failed to delete stale sessions: {error}"
            );
            0
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
    pub(crate) tokens: i64,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) active_dispatch_id: Option<&'a str>,
    pub(crate) thread_channel_id: Option<&'a str>,
    pub(crate) claude_session_id: Option<&'a str>,
    pub(crate) raw_provider_session_id: Option<&'a str>,
}

pub(crate) struct DeleteSessionResult {
    pub(crate) session_id: Option<i64>,
    pub(crate) deleted: u64,
}

pub(crate) struct ProviderSessionIds {
    pub(crate) claude_session_id: Option<String>,
    pub(crate) raw_provider_session_id: Option<String>,
}

pub(crate) struct UpdateSessionParams<'a> {
    pub(crate) status: Option<&'a str>,
    pub(crate) active_dispatch_id: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) tokens: Option<i64>,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) session_info: Option<&'a str>,
}

pub(crate) async fn session_exists_pg(pool: &PgPool, session_key: &str) -> Result<bool, String> {
    sqlx::query("SELECT 1 FROM sessions WHERE session_key = $1 LIMIT 1")
        .bind(session_key)
        .fetch_optional(pool)
        .await
        .map(|row| row.is_some())
        .map_err(|error| format!("load postgres session existence for {session_key}: {error}"))
}

pub(crate) async fn upsert_hook_session_pg(
    pool: &PgPool,
    params: HookSessionUpsert<'_>,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO sessions (
            session_key,
            instance_id,
            agent_id,
            provider,
            status,
            session_info,
            model,
            tokens,
            cwd,
            active_dispatch_id,
            thread_channel_id,
            claude_session_id,
            raw_provider_session_id,
            last_heartbeat
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, NOW()
         )
         ON CONFLICT(session_key) DO UPDATE SET
            status = EXCLUDED.status,
            instance_id = COALESCE(NULLIF(BTRIM(EXCLUDED.instance_id), ''), sessions.instance_id),
            provider = EXCLUDED.provider,
            session_info = COALESCE(EXCLUDED.session_info, sessions.session_info),
            model = COALESCE(EXCLUDED.model, sessions.model),
            tokens = EXCLUDED.tokens,
            cwd = COALESCE(EXCLUDED.cwd, sessions.cwd),
            active_dispatch_id = CASE
              WHEN lower(EXCLUDED.status) IN ('disconnected', 'aborted') THEN NULL
              WHEN EXCLUDED.active_dispatch_id IS NOT NULL THEN EXCLUDED.active_dispatch_id
              ELSE sessions.active_dispatch_id
            END,
            agent_id = COALESCE(NULLIF(BTRIM(EXCLUDED.agent_id), ''), NULLIF(BTRIM(sessions.agent_id), '')),
            thread_channel_id = COALESCE(EXCLUDED.thread_channel_id, sessions.thread_channel_id),
            claude_session_id = COALESCE(EXCLUDED.claude_session_id, sessions.claude_session_id),
            raw_provider_session_id = COALESCE(EXCLUDED.raw_provider_session_id, sessions.raw_provider_session_id),
            last_heartbeat = NOW()",
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
    .bind(params.claude_session_id)
    .bind(params.raw_provider_session_id)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| format!("upsert postgres session {}: {error}", params.session_key))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn upsert_hook_session_sqlite_for_tests(
    conn: &sqlite_test::Connection,
    params: HookSessionUpsert<'_>,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO sessions (
            session_key,
            instance_id,
            agent_id,
            provider,
            status,
            session_info,
            model,
            tokens,
            cwd,
            active_dispatch_id,
            thread_channel_id,
            claude_session_id,
            raw_provider_session_id,
            last_heartbeat
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, datetime('now'))
         ON CONFLICT(session_key) DO UPDATE SET
            status = excluded.status,
            instance_id = COALESCE(NULLIF(TRIM(excluded.instance_id), ''), sessions.instance_id),
            provider = excluded.provider,
            session_info = COALESCE(excluded.session_info, sessions.session_info),
            model = COALESCE(excluded.model, sessions.model),
            tokens = excluded.tokens,
            cwd = COALESCE(excluded.cwd, sessions.cwd),
            active_dispatch_id = CASE
              WHEN lower(excluded.status) IN ('disconnected', 'aborted') THEN NULL
              WHEN excluded.active_dispatch_id IS NOT NULL THEN excluded.active_dispatch_id
              ELSE sessions.active_dispatch_id
            END,
            agent_id = COALESCE(NULLIF(TRIM(excluded.agent_id), ''), NULLIF(TRIM(sessions.agent_id), '')),
            thread_channel_id = COALESCE(excluded.thread_channel_id, sessions.thread_channel_id),
            claude_session_id = COALESCE(excluded.claude_session_id, sessions.claude_session_id),
            raw_provider_session_id = COALESCE(excluded.raw_provider_session_id, sessions.raw_provider_session_id),
            last_heartbeat = datetime('now')",
        sqlite_test::params![
            params.session_key,
            params.instance_id,
            params.agent_id,
            params.provider,
            params.status,
            params.session_info,
            params.model,
            params.tokens,
            params.cwd,
            params.active_dispatch_id,
            params.thread_channel_id,
            params.claude_session_id,
            params.raw_provider_session_id,
        ],
    )
    .map(|_| ())
    .map_err(|error| format!("{error}"))
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
            "SELECT claude_session_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = $1 AND provider = $2",
        )
        .bind(session_key)
        .bind(provider)
        .fetch_optional(pool)
        .await
    } else {
        sqlx::query(
            "SELECT claude_session_id, raw_provider_session_id
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
        })
    })
    .transpose()
    .map_err(|error: sqlx::Error| format!("{error}"))
}

pub(crate) async fn clear_stale_session_id_pg(
    pool: &PgPool,
    session_id: &str,
) -> Result<u64, String> {
    sqlx::query(
        "UPDATE sessions
         SET claude_session_id = NULL,
             raw_provider_session_id = NULL
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
             raw_provider_session_id = NULL
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
    sqlx::query(
        "UPDATE sessions
         SET status = COALESCE($1, status),
             active_dispatch_id = COALESCE($2, active_dispatch_id),
             model = COALESCE($3, model),
             tokens = COALESCE($4, tokens),
             cwd = COALESCE($5, cwd),
             session_info = COALESCE($6, session_info)
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
