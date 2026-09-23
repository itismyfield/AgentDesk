//! #138: Channel queue management + dispatch cancel API.
//!
//! Provides operational endpoints for pipeline incident recovery.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};

// ── GET /api/channels/:id/queue ─────────────────────────────────

/// List intervention queue for a channel.
pub async fn list_channel_queue(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> Json<serde_json::Value> {
    if channel_id.parse::<u64>().is_err() {
        return Json(json!({"error": "invalid channel_id", "queue": []}));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Json(json!({"error": "postgres pool unavailable", "queue": []}));
    };
    let dispatches = sqlx::query(
        "SELECT
            td.id,
            td.dispatch_type,
            td.status,
            td.title,
            td.created_at::TEXT AS created_at,
            kc.github_issue_number::BIGINT AS github_issue_number
         FROM task_dispatches td
         JOIN kanban_cards kc ON td.kanban_card_id = kc.id
         JOIN agents a ON td.to_agent_id = a.id
         WHERE (
             a.discord_channel_id = $1 OR a.discord_channel_alt = $1 OR
             a.discord_channel_cc = $1 OR a.discord_channel_cdx = $1
         )
           AND td.status IN ('pending', 'dispatched')
         ORDER BY td.created_at DESC",
    )
    .bind(&channel_id)
    .fetch_all(pool)
    .await
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| queue_channel_dispatch_row_to_json_pg(&row).ok())
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    Json(json!({"channel_id": channel_id, "dispatches": dispatches}))
}

// ── GET /api/dispatches/pending ─────────────────────────────────

/// List all pending dispatches across all agents.
pub async fn list_pending_dispatches(State(state): State<AppState>) -> Json<serde_json::Value> {
    let Some(pool) = state.pg_pool_ref() else {
        return Json(json!({"error": "postgres pool unavailable", "dispatches": [], "count": 0}));
    };
    let dispatches = sqlx::query(
        "SELECT
            td.id,
            td.kanban_card_id,
            td.to_agent_id,
            td.dispatch_type,
            td.status,
            td.title,
            td.thread_id,
            td.created_at::TEXT AS created_at,
            td.retry_count::BIGINT AS retry_count,
            kc.github_issue_number::BIGINT AS github_issue_number,
            kc.status AS card_status
         FROM task_dispatches td
         JOIN kanban_cards kc ON td.kanban_card_id = kc.id
         WHERE td.status IN ('pending', 'dispatched')
         ORDER BY td.created_at DESC",
    )
    .fetch_all(pool)
    .await
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| pending_dispatch_row_to_json_pg(&row).ok())
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    Json(json!({"dispatches": dispatches, "count": dispatches.len()}))
}

// ── POST /api/dispatches/:id/cancel ─────────────────────────────

/// Cancel a specific dispatch.
pub async fn cancel_dispatch(
    State(state): State<AppState>,
    Path(dispatch_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let body = state
        .queue_service()
        .cancel_dispatch(state.health_registry.as_ref(), &dispatch_id)
        .await?;
    Ok((StatusCode::OK, Json(body)))
}

// ── POST /api/dispatches/cancel-all ─────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CancelAllBody {
    pub kanban_card_id: Option<String>,
    pub agent_id: Option<String>,
}

/// Cancel all pending/dispatched dispatches matching filters.
pub async fn cancel_all_dispatches(
    State(state): State<AppState>,
    Json(body): Json<CancelAllBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let response = state
        .queue_service()
        .cancel_all_dispatches(body.kanban_card_id.as_deref(), body.agent_id.as_deref())
        .await?;
    Ok((StatusCode::OK, Json(response)))
}

// ── POST /api/turns/:channel_id/cancel ──────────────────────────

/// Query parameters for `POST /api/turns/:channel_id/cancel`.
///
/// `force=true` requests the historical hard-kill path: the live turn's tmux
/// session and the entire child PID tree (cargo, claude CLI, …) get SIGKILLed
/// via `kill_pid_tree`. Reserve for explicit recovery — operators reaching
/// for "remove queued message" almost never want this (#1196).
///
/// Default (`force=false`): preserve the live tmux session and watcher; only
/// drain the channel mailbox. Tool subprocesses keep running.
#[derive(Debug, Default, Deserialize)]
pub struct CancelTurnQuery {
    #[serde(default)]
    pub force: bool,
}

/// #3029(C): `force` carried in the request *body*, mirroring the JSON-body
/// shape of `cancel_all_dispatches`. Clients that POST
/// `{"force": true}` previously had it silently dropped because the handler
/// only read `Query<CancelTurnQuery>`, downgrading an intended hard-kill to a
/// soft cancel.
#[derive(Debug, Default, Deserialize)]
pub struct CancelTurnBody {
    #[serde(default)]
    pub force: bool,
}

/// #3029(C): resolve the effective `force` intent from query + body. The body
/// wins when present and parseable (it's the canonical JSON shape); the query
/// param remains an honored fallback for existing `?force=true` clients. An
/// empty or non-JSON body falls through to the query value, so callers that
/// never sent a body keep working unchanged.
fn resolve_cancel_force(query_force: bool, body: &Bytes) -> bool {
    if body.is_empty() {
        return query_force;
    }
    match serde_json::from_slice::<CancelTurnBody>(body) {
        Ok(parsed) => parsed.force || query_force,
        // Unparseable body: do not silently swallow the request; honor the
        // query fallback so a `?force=true` with a junk body still forces.
        Err(_) => query_force,
    }
}

/// Cancel the active turn in a channel.
///
/// Default (`force=false`): preserves the live provider session and watcher;
/// drains the channel mailbox. The currently running tool subtree is NOT
/// SIGKILLed — what an operator usually means by "queue 정리".
///
/// `force=true`: tear the tmux session down, SIGKILL the PID tree, clear
/// inflight state. The turn will not complete gracefully; in-flight
/// `cargo`/`claude` subprocesses get terminated. `force` may be supplied either
/// as a query param (`?force=true`) or in the JSON body (`{"force": true}`,
/// #3029); the body wins when both are present.
pub async fn cancel_turn(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel_id): Path<String>,
    Query(query): Query<CancelTurnQuery>,
    body: Bytes,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let force = resolve_cancel_force(query.force, &body);
    let forward_context = crate::services::session_forwarding::ForwardCallerContext::from(&state);
    let response = state
        .queue_service()
        .cancel_turn(
            state.health_registry.as_ref(),
            &channel_id,
            force,
            &headers,
            &forward_context,
        )
        .await?;
    Ok((StatusCode::OK, Json(response)))
}

// ── GET /api/channels/:id/watcher-state ─────────────────────────

/// #964 / #1133: snapshot the tmux-watcher lifecycle state for a channel.
///
/// Core fields (#964): `{ provider, attached, tmux_session,
/// last_relay_offset, inflight_state_present, last_relay_ts_ms,
/// last_capture_offset, unread_bytes, desynced, reconnect_count,
/// has_pending_queue }`.
///
/// #5071 relay-tail S2: `unread_bytes` is nullable and `null` means UNMEASURED,
/// not `0`. The tail could not be counted against this row's relay frontier (no
/// row `output_path`, a failed stat on it, or a row/watcher tmux mismatch), so
/// an out-of-band consumer that folds `null` to `0` reads an unknown tail as a
/// drained one.
///
/// #1133 enriched read-only diagnostics (omitted when their source is
/// absent): `inflight_started_at`, `inflight_updated_at`,
/// `inflight_user_msg_id`, `inflight_current_msg_id`,
/// `watcher_owner_channel_id`, `tmux_session_alive` (PID check via
/// `tmux has-session`), and `mailbox_active_user_msg_id`. All fields are
/// PII-free scalars so the response is safe for non-privileged operator
/// dashboards.
///
/// Used by operators to diagnose "watcher detached silently while tmux
/// still producing output" incidents and pre-watcher mailbox queueing.
/// `desynced=true` means a live tmux-backed inflight appears orphaned,
/// is owned by another channel, or its capture file diverges from relay
/// telemetry while stale for at least 30 seconds.
///
/// 404 is returned when no watcher, no inflight state, and no mailbox
/// engagement (active turn or queued intervention) is known for the
/// channel across all registered providers.
pub async fn get_watcher_state(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let channel_num: u64 = channel_id
        .parse()
        .map_err(|_| AppError::bad_request("channel_id must be a numeric Discord channel ID"))?;

    let registry = state.health_registry.as_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "health registry unavailable in this runtime",
        )
    })?;

    match registry.snapshot_watcher_state(channel_num).await {
        Some(snapshot) => {
            let body = serde_json::to_value(&snapshot)
                .unwrap_or_else(|_| json!({"error": "failed to serialize watcher snapshot"}));
            Ok((StatusCode::OK, Json(body)))
        }
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "no watcher, relay-coord, or inflight state for this channel",
                "channel_id": channel_id,
            })),
        )),
    }
}

fn queue_channel_dispatch_row_to_json_pg(
    row: &sqlx::postgres::PgRow,
) -> Result<serde_json::Value, String> {
    Ok(json!({
        "dispatch_id": row.try_get::<String, _>("id").map_err(|error| format!("decode queue dispatch id: {error}"))?,
        "dispatch_type": row.try_get::<String, _>("dispatch_type").map_err(|error| format!("decode queue dispatch_type: {error}"))?,
        "status": row.try_get::<String, _>("status").map_err(|error| format!("decode queue status: {error}"))?,
        "title": row.try_get::<Option<String>, _>("title").map_err(|error| format!("decode queue title: {error}"))?,
        "created_at": row.try_get::<Option<String>, _>("created_at").map_err(|error| format!("decode queue created_at: {error}"))?,
        "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number").map_err(|error| format!("decode queue github_issue_number: {error}"))?,
    }))
}

fn pending_dispatch_row_to_json_pg(
    row: &sqlx::postgres::PgRow,
) -> Result<serde_json::Value, String> {
    Ok(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode pending dispatch id: {error}"))?,
        "kanban_card_id": row.try_get::<String, _>("kanban_card_id").map_err(|error| format!("decode pending kanban_card_id: {error}"))?,
        "to_agent_id": row.try_get::<String, _>("to_agent_id").map_err(|error| format!("decode pending to_agent_id: {error}"))?,
        "dispatch_type": row.try_get::<String, _>("dispatch_type").map_err(|error| format!("decode pending dispatch_type: {error}"))?,
        "status": row.try_get::<String, _>("status").map_err(|error| format!("decode pending status: {error}"))?,
        "title": row.try_get::<Option<String>, _>("title").map_err(|error| format!("decode pending title: {error}"))?,
        "thread_id": row.try_get::<Option<String>, _>("thread_id").map_err(|error| format!("decode pending thread_id: {error}"))?,
        "created_at": row.try_get::<Option<String>, _>("created_at").map_err(|error| format!("decode pending created_at: {error}"))?,
        "retry_count": row.try_get::<i64, _>("retry_count").map_err(|error| format!("decode pending retry_count: {error}"))?,
        "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number").map_err(|error| format!("decode pending github_issue_number: {error}"))?,
        "card_status": row.try_get::<String, _>("card_status").map_err(|error| format!("decode pending card_status: {error}"))?,
    }))
}

// #3029(C): `resolve_cancel_force` is a pure parser with no DB/runtime
// dependency, so its coverage lives in a plain `#[cfg(test)]` module that runs
// under the default `cargo test` invocation; the older SQLite-only suite was
// removed from the supported Cargo feature set.
#[cfg(test)]
mod cancel_force_tests {
    use super::*;
    use axum::body::Bytes;

    #[test]
    fn body_force_true_is_honored_even_when_query_false() {
        let body = Bytes::from_static(b"{\"force\": true}");
        assert!(
            resolve_cancel_force(false, &body),
            "force in body must be honored (#3029 C): previously dropped to default=false"
        );
    }

    #[test]
    fn body_force_false_is_respected() {
        let body = Bytes::from_static(b"{\"force\": false}");
        assert!(!resolve_cancel_force(false, &body));
    }

    #[test]
    fn empty_body_falls_back_to_query() {
        let empty = Bytes::new();
        assert!(
            resolve_cancel_force(true, &empty),
            "existing ?force=true clients (no body) must keep forcing"
        );
        assert!(!resolve_cancel_force(false, &empty));
    }

    #[test]
    fn query_force_remains_fallback_when_body_omits_force() {
        // Body is valid JSON but omits `force` → serde default false; the query
        // value still wins as a fallback.
        let body = Bytes::from_static(b"{}");
        assert!(resolve_cancel_force(true, &body));
        assert!(!resolve_cancel_force(false, &body));
    }

    #[test]
    fn unparseable_body_falls_back_to_query() {
        let junk = Bytes::from_static(b"not json");
        assert!(
            resolve_cancel_force(true, &junk),
            "a junk body must not silently swallow a ?force=true intent"
        );
        assert!(!resolve_cancel_force(false, &junk));
    }
}

/// #5176 R3 regression: `POST /api/turns/{channel_id}/cancel` must not destroy
/// queued user messages in silence. Driven through the HTTP handler so the
/// capture/preserve wiring is under test, not just the guard helper.
#[cfg(test)]
mod cancel_queue_preserve_pg_tests {

    use std::sync::Arc;
    use std::time::Instant;

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::super::{AppState, domains};
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::provider::ProviderKind;
    use crate::services::turn_orchestrator::{
        ChannelMailboxRegistry, Intervention, InterventionMode, QueuePersistenceContext,
    };

    const AUTH_TOKEN: &str = "cancel-queue-preserve-test-token";
    const QUEUED_TEXT: &str = "the instruction #5176 threw away";

    fn test_state(pool: sqlx::PgPool) -> AppState {
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH_TOKEN.to_string());
        let engine = crate::engine::PolicyEngine::new(&config).expect("construct policy engine");
        let broadcast_tx = crate::eventbus::new_broadcast();
        let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
        AppState {
            pg_pool: Some(pool),
            engine,
            config: Arc::new(config),
            broadcast_tx,
            batch_buffer,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    fn test_router(pool: sqlx::PgPool) -> Router {
        let state = test_state(pool);
        domains::runtime::router(state.clone()).with_state(state)
    }

    async fn post_cancel(app: &Router, channel_id: u64, force: bool) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/turns/{channel_id}/cancel?force={force}"))
            .header(header::AUTHORIZATION, format!("Bearer {AUTH_TOKEN}"))
            .body(Body::empty())
            .expect("build cancel request");
        let response = app
            .clone()
            .oneshot(request)
            .await
            .expect("cancel request completes");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("read cancel body");
        let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    /// An agent that owns the channel plus an active session, so the handler
    /// resolves a cancel target instead of 404-ing.
    async fn seed_cancel_target(pool: &sqlx::PgPool, channel_id: u64) {
        let channel = channel_id.to_string();
        sqlx::query("INSERT INTO agents (id, name, discord_channel_cc) VALUES ($1, $1, $2)")
            .bind(format!("agent-{channel_id}"))
            .bind(&channel)
            .execute(pool)
            .await
            .expect("seed agent row");
        sqlx::query(
            "INSERT INTO sessions (
                 channel_id, session_key, agent_id, provider, status, last_heartbeat
             ) VALUES ($1, $2, $3, 'claude', 'turn_active', NOW())",
        )
        .bind(&channel)
        .bind(format!("host:AgentDesk-claude-{channel_id}"))
        .bind(format!("agent-{channel_id}"))
        .execute(pool)
        .await
        .expect("seed session row");
    }

    fn queued_user_message(message_id: u64) -> Intervention {
        Intervention {
            author_id: UserId::new(4_242),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: QUEUED_TEXT.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    /// Publish a live mailbox for `channel_id` holding exactly one queued user
    /// message. Returned handle keeps the actor reachable for the assertions.
    async fn seed_queued_message(
        channel_id: ChannelId,
        message_id: u64,
    ) -> crate::services::turn_orchestrator::ChannelMailboxHandle {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        handle
            .replace_queue(
                vec![queued_user_message(message_id)],
                QueuePersistenceContext::new(&ProviderKind::Claude, "", None),
            )
            .await;
        assert_eq!(
            handle.snapshot().await.intervention_queue.len(),
            1,
            "fixture must really enqueue one user message before the cancel"
        );
        handle
    }

    /// No polling: the response only reports a message as dead-lettered once the
    /// row is committed, so a missing row here is a claim the cancel never kept.
    async fn dead_letter_row(pool: &sqlx::PgPool, channel_id: u64) -> Option<(String, String)> {
        sqlx::query_as(
            "SELECT content, message_id FROM relay_dead_letter
              WHERE kind = 'cancel_queue_discard' AND channel_id = $1",
        )
        .bind(channel_id.to_string())
        .fetch_optional(pool)
        .await
        .expect("query relay_dead_letter")
    }

    /// `force=true` is a deliberate purge, so the queued instruction does not
    /// come back — but it must leave a durable record instead of vanishing.
    #[tokio::test(flavor = "current_thread")]
    async fn force_cancel_dead_letters_the_queued_message_it_purges_pg() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let channel_id = 5_176_401_u64;
        seed_cancel_target(&pool, channel_id).await;
        let handle = seed_queued_message(ChannelId::new(channel_id), 9_101).await;

        let app = test_router(pool.clone());
        let (status, body) = post_cancel(&app, channel_id, true).await;
        assert_eq!(status, StatusCode::OK, "cancel response: {body}");

        assert_eq!(
            body["queue_dead_lettered_message_ids"],
            serde_json::json!(["9101".parse::<u64>().unwrap()]),
            "force cancel must name the purged instruction: {body}"
        );
        assert_eq!(
            body["queue_loss_recorded"], true,
            "a purge with a durable record is not a silent loss: {body}"
        );
        assert!(
            handle.snapshot().await.intervention_queue.is_empty(),
            "force cancel must still purge the mailbox"
        );

        let (content, message_id) = dead_letter_row(&pool, channel_id)
            .await
            .expect("the purged user message must be recoverable from relay_dead_letter");
        assert_eq!(content, QUEUED_TEXT);
        assert_eq!(message_id, "9101");
    }

    /// The preserve path must leave a queue it did not touch exactly as it was:
    /// no purge, no duplicate, and no dead-letter noise.
    #[tokio::test(flavor = "current_thread")]
    async fn preserve_cancel_keeps_the_queued_message_and_records_nothing_pg() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let channel_id = 5_176_402_u64;
        seed_cancel_target(&pool, channel_id).await;
        let handle = seed_queued_message(ChannelId::new(channel_id), 9_102).await;

        let app = test_router(pool.clone());
        let (status, body) = post_cancel(&app, channel_id, false).await;
        assert_eq!(status, StatusCode::OK, "cancel response: {body}");

        assert_eq!(
            body["queue_loss_recorded"], true,
            "nothing was removed, so nothing was lost: {body}"
        );
        assert_eq!(
            body["queue_dead_lettered_message_ids"],
            serde_json::json!([])
        );
        assert_eq!(
            body["queued_remaining"], 1,
            "the surviving instruction must be reported, not zeroed: {body}"
        );

        let survivors = handle.snapshot().await.intervention_queue;
        assert_eq!(
            survivors.len(),
            1,
            "a preserve cancel must not drop or duplicate the queued instruction"
        );
        assert_eq!(survivors[0].message_id.get(), 9_102);
        assert_eq!(survivors[0].text, QUEUED_TEXT);

        assert!(
            dead_letter_row(&pool, channel_id).await.is_none(),
            "a preserved queue must not be reported as lost"
        );
    }
}
