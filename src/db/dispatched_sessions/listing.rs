use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

#[derive(Debug, Clone)]
pub(crate) struct DispatchedSessionSnapshot {
    pub id: i64,
    pub session_key: Option<String>,
    pub instance_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub status: Option<String>,
    pub active_dispatch_id: Option<String>,
    pub model: Option<String>,
    pub tokens: i64,
    pub cwd: Option<String>,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub session_info: Option<String>,
    pub department_id: Option<String>,
    pub sprite_number: Option<i64>,
    pub avatar_emoji: Option<String>,
    pub department_name: Option<String>,
    pub department_name_ko: Option<String>,
    pub department_color: Option<String>,
    pub thread_channel_id: Option<String>,
    pub dispatch_thread_id: Option<String>,
    pub dispatch_type: Option<String>,
    pub dispatch_row_status: Option<String>,
    pub dispatch_created_at: Option<DateTime<Utc>>,
    pub dispatch_sent_at: Option<DateTime<Utc>>,
    pub auto_queue_entry_id: Option<String>,
    pub auto_queue_run_id: Option<String>,
    pub auto_queue_slot_index: Option<i64>,
    pub auto_queue_thread_group: Option<i64>,
}

pub(crate) async fn list_dispatched_session_snapshots_pg(
    pool: &PgPool,
    include_all: bool,
) -> Result<Vec<DispatchedSessionSnapshot>, String> {
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
        sessions.push(DispatchedSessionSnapshot {
            id,
            session_key,
            instance_id,
            agent_id,
            provider,
            status,
            active_dispatch_id,
            model,
            tokens,
            cwd,
            last_heartbeat,
            session_info,
            department_id,
            sprite_number,
            avatar_emoji,
            department_name,
            department_name_ko,
            department_color,
            thread_channel_id,
            dispatch_thread_id,
            dispatch_type,
            dispatch_row_status,
            dispatch_created_at,
            dispatch_sent_at,
            auto_queue_entry_id,
            auto_queue_run_id,
            auto_queue_slot_index,
            auto_queue_thread_group,
        });
    }

    Ok(sessions)
}
