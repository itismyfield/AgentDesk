#![cfg(unix)]

use axum::extract::{Query, State};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::sync::Arc;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::host_defer_gate::tests::ScriptedTmux;

use crate::db::session_agent_resolution::{
    normalize_thread_channel_id, parse_thread_channel_id_from_session_key,
};
use crate::domain::session_identity::tmux_name_from_session_key;
use crate::services::session_activity::SessionActivityResolver;

// Frozen listing implementation from d419392; the service result is compared independently.
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

async fn legacy_list_dispatched_sessions_pg(
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

const RECENT: &str = "2100-01-02T03:04:05Z";
const STALE: &str = "2000-01-02T03:04:05Z";

async fn seed_session(
    pool: &PgPool,
    key: &str,
    status: &str,
    heartbeat: Option<&str>,
    dispatch: bool,
) {
    sqlx::query(
        "INSERT INTO sessions
         (session_key, instance_id, agent_id, provider, status, active_dispatch_id,
          model, tokens, cwd, last_heartbeat, session_info, thread_channel_id)
         VALUES ($1, 'listing-node', 'listing-agent', 'claude', $2,
                 CASE WHEN $4 THEN 'listing-dispatch' END,
                 'fixture-model', 321, '/fixture/repo', $3::TEXT::TIMESTAMPTZ,
                 '── phase-gate dispatch ──', '123456789012345')",
    )
    .bind(key)
    .bind(status)
    .bind(heartbeat)
    .bind(dispatch)
    .execute(pool)
    .await
    .expect("seed listing session");
}

async fn seed_joined_rows(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO departments (id, name, name_ko, color)
         VALUES ('listing-department', 'Engineering', '개발', '#123456')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, department, provider, sprite_number, avatar_emoji)
         VALUES ('listing-agent', 'Listing Agent', 'listing-department', 'claude', 17, '🦊')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches
         (id, to_agent_id, dispatch_type, status, title, context, thread_id, created_at)
         VALUES ('listing-dispatch', 'listing-agent', 'implementation', 'dispatched',
                 'Listing fixture', '{}', '234567890123456', '2001-02-03T04:05:06Z')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO dispatch_delivery_events
         (dispatch_id, correlation_id, semantic_event_id, status, created_at)
         VALUES ('listing-dispatch', 'listing-correlation', 'listing-sent',
                 'sent', '2001-02-03T04:06:07Z')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('listing-run', 'listing-repo', 'listing-agent', 'active')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries
         (id, run_id, agent_id, status, dispatch_id, slot_index, thread_group, created_at)
         VALUES ('listing-entry-old', 'listing-run', 'listing-agent', 'done',
                 'listing-dispatch', 1, 2, '2001-01-01'),
                ('listing-entry-current', 'listing-run', 'listing-agent', 'dispatched',
                 'listing-dispatch', 3, 4, '2001-01-02')",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn session_rows(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn assert_same_bytes(actual: &[Value], expected: &[Value]) {
    assert_eq!(
        serde_json::to_vec(actual).unwrap(),
        serde_json::to_vec(expected).unwrap(),
        "the full listing JSON must match the frozen implementation"
    );
}

#[tokio::test]
async fn listing_json_matches_frozen_oracle_and_leaves_rows_unchanged_pg() {
    let runtime_root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    seed_joined_rows(&pool).await;
    for (key, status, heartbeat, dispatch) in [
        (
            "s410-remote:AgentDesk-claude-recent",
            "turn_active",
            Some(RECENT),
            true,
        ),
        (
            "s410-remote:AgentDesk-claude-stale",
            "turn_active",
            Some(STALE),
            true,
        ),
        ("s410-unknown-key-recent", "turn_active", Some(RECENT), true),
        ("s410-unknown-key-stale", "turn_active", None, true),
        (
            "s410-remote:AgentDesk-claude-terminal",
            "aborted",
            Some(RECENT),
            true,
        ),
        (
            "s410-remote:AgentDesk-claude-background",
            "awaiting_bg",
            Some(STALE),
            true,
        ),
        (
            "s410-remote:AgentDesk-claude-idle",
            "idle",
            Some(STALE),
            false,
        ),
    ] {
        seed_session(&pool, key, status, heartbeat, dispatch).await;
    }
    sqlx::query("INSERT INTO sessions (status, tokens) VALUES ('disconnected', 0)")
        .execute(&pool)
        .await
        .unwrap();
    let before = session_rows(&pool).await;
    for include_all in [false, true] {
        let expected = legacy_list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        let actual = super::list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        assert_same_bytes(&actual, &expected);
        assert_eq!(actual.len(), if include_all { 8 } else { 3 });
        let recent = actual
            .iter()
            .find(|row| row["session_key"] == "s410-remote:AgentDesk-claude-recent")
            .unwrap();
        assert_eq!(recent["session_info"], "── 구현 dispatch ──");
        assert_eq!(recent["session_info_raw"], "── phase-gate dispatch ──");
        assert_eq!(recent["delivery_state"], "codex_active");
        assert_eq!(recent["resolved_thread_channel_id"], "234567890123456");
        assert_eq!(
            recent["recovery_identifiers"]["auto_queue_entry_id"],
            "listing-entry-current"
        );
    }
    assert_eq!(session_rows(&pool).await, before);
    assert!(
        tmux.take_calls().is_empty(),
        "remote and unknown keys never probe tmux"
    );
    pool.close().await;
    database.drop().await;
    drop(tmux);
    drop(runtime_root);
}

#[tokio::test]
async fn listing_marker_refusal_preserves_heartbeat_projection_and_avoids_tmux_pg() {
    let runtime_root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    seed_joined_rows(&pool).await;
    let host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .unwrap();
    assert!(host.status.success());
    let host = String::from_utf8(host.stdout).unwrap();
    let host = host.trim();
    for (suffix, marker_kind, heartbeat) in [
        ("herdr-recent", "herdr", RECENT),
        ("herdr-stale", "herdr", STALE),
        ("unknown-recent", "unrecognized-host", RECENT),
        ("unknown-stale", "unrecognized-host", STALE),
    ] {
        let name = format!("AgentDesk-claude-listing-{suffix}");
        let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, marker_kind).unwrap();
        seed_session(
            &pool,
            &format!("{host}:{name}"),
            "turn_active",
            Some(heartbeat),
            true,
        )
        .await;
    }
    let before = session_rows(&pool).await;
    for include_all in [false, true] {
        let expected = legacy_list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        let actual = super::list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        assert_same_bytes(&actual, &expected);
        assert_eq!(actual.len(), if include_all { 4 } else { 2 });
        if include_all {
            assert_eq!(actual[0]["status"], "turn_active");
            assert_eq!(actual[1]["status"], "idle");
            assert_eq!(actual[2]["status"], "turn_active");
            assert_eq!(actual[3]["active_dispatch_id"], Value::Null);
        }
    }
    assert!(
        tmux.take_calls().is_empty(),
        "refused markers never fall back to local tmux"
    );
    let local_key = format!("{host}:AgentDesk-claude-listing-legacy-absent");
    seed_session(&pool, &local_key, "turn_active", Some(RECENT), true).await;
    let expected = legacy_list_dispatched_sessions_pg(&pool, true)
        .await
        .unwrap();
    let actual = super::list_dispatched_sessions_pg(&pool, true)
        .await
        .unwrap();
    assert_same_bytes(&actual, &expected);
    assert!(
        !tmux.take_calls().is_empty(),
        "the unmarked local key still probes tmux"
    );
    assert_eq!(session_rows(&pool).await[..4], before);
    pool.close().await;
    database.drop().await;
    drop(tmux);
    drop(runtime_root);
}

fn app_state(pool: &PgPool, runtime_root: &std::path::Path) -> crate::app_state::AppState {
    let mut config = crate::config::Config::default();
    config.policies.hot_reload = false;
    config.policies.dir = runtime_root.join("empty-policies");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let broadcast_tx = crate::server::ws::new_broadcast();
    crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::server::ws::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: Some("listing-node".to_string()),
    }
}

#[tokio::test]
async fn listing_get_and_cluster_owner_summary_consume_the_same_projection_pg() {
    let runtime_root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    seed_joined_rows(&pool).await;
    seed_session(
        &pool,
        "s410-remote:AgentDesk-claude-handler-live",
        "turn_active",
        Some(RECENT),
        true,
    )
    .await;
    seed_session(
        &pool,
        "s410-remote:AgentDesk-claude-handler-stale",
        "turn_active",
        Some(STALE),
        true,
    )
    .await;
    let before = session_rows(&pool).await;
    let state = app_state(&pool, runtime_root.path());
    for include_all in [false, true] {
        let mut expected = legacy_list_dispatched_sessions_pg(&pool, include_all)
            .await
            .unwrap();
        crate::services::cluster::session_routing::enrich_session_owner_routing(
            &mut expected,
            Some("listing-node"),
            &[],
        );
        let (status, body) = crate::server::routes::dispatched_sessions::list_dispatched_sessions(
            State(state.clone()),
            Query(super::ListDispatchedSessionsQuery {
                include_merged: include_all.then(|| "1".to_string()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            serde_json::to_vec(&body.0).unwrap(),
            serde_json::to_vec(&json!({"sessions": expected})).unwrap()
        );
    }
    let (status, body) = crate::server::routes::cluster::list_nodes(State(state.clone()))
        .await
        .unwrap();
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body.0["session_owners"]["total_active_sessions"], 1);
    assert_eq!(body.0["session_owners"]["local"], 1);
    assert_eq!(body.0["session_owner_error"], Value::Null);
    assert_eq!(session_rows(&pool).await, before);
    assert!(tmux.take_calls().is_empty());
    drop(state);
    pool.close().await;
    database.drop().await;
    drop(tmux);
    drop(runtime_root);
}
