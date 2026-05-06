use super::*;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::Uri,
    response::IntoResponse,
    routing::{get, post, put},
};
use std::{
    collections::HashMap,
    ffi::OsString,
    sync::{Arc, Mutex},
};

fn test_db() -> crate::db::Db {
    crate::db::test_db()
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::services::discord::runtime_store::lock_test_env()
}

#[test]
fn review_delivery_channel_uses_target_provider_from_context() {
    let db = test_db();
    let conn = db.lock().expect("sqlite conn");
    conn.execute(
        "INSERT INTO agents (
                id, name, provider, discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
             ) VALUES ('agent-review-route', 'Agent', 'codex', '111', '222', '111', '222')",
        [],
    )
    .expect("seed agent");

    let channel = resolve_dispatch_delivery_channel_on_conn(
        &conn,
        "agent-review-route",
        "card-review-route",
        Some("review"),
        Some(r#"{"target_provider":"codex"}"#),
    )
    .expect("resolve delivery channel");

    assert_eq!(channel.as_deref(), Some("222"));
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }

    fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn write_announce_token(root: &std::path::Path) {
    let credential_dir = crate::runtime_layout::credential_dir(root);
    std::fs::create_dir_all(&credential_dir).unwrap();
    std::fs::write(
        crate::runtime_layout::credential_token_path(root, "announce"),
        "announce-token\n",
    )
    .unwrap();
}

fn write_command_bot_token(root: &std::path::Path, name: &str, value: &str) {
    let credential_dir = crate::runtime_layout::credential_dir(root);
    std::fs::create_dir_all(&credential_dir).unwrap();
    std::fs::write(
        crate::runtime_layout::credential_token_path(root, name),
        format!("{value}\n"),
    )
    .unwrap();
}

struct TestPostgresDb {
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl TestPostgresDb {
    async fn create() -> Self {
        let admin_url = postgres_admin_database_url();
        let database_name = format!(
            "agentdesk_dispatch_reaction_{}",
            uuid::Uuid::new_v4().simple()
        );
        let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "discord delivery tests",
        )
        .await
        .unwrap();

        Self {
            admin_url,
            database_name,
            database_url,
        }
    }

    async fn connect_and_migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(
            &self.database_url,
            "discord delivery tests",
        )
        .await
        .unwrap()
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "discord delivery tests",
        )
        .await
        .unwrap();
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

#[derive(Clone, Debug, Default)]
struct MockDiscordState {
    archived: bool,
    unarchive_failures_remaining: usize,
    message_length_failures_remaining: usize,
    message_length_failure_min_chars: Option<usize>,
    thread_create_status: Option<axum::http::StatusCode>,
    calls: Vec<String>,
    posted_messages: Vec<(String, String)>,
    thread_names: HashMap<String, String>,
    thread_parents: HashMap<String, String>,
}

async fn spawn_mock_discord_server(
    initial_archived: bool,
) -> (
    String,
    Arc<Mutex<MockDiscordState>>,
    tokio::task::JoinHandle<()>,
) {
    spawn_mock_discord_server_with_config(initial_archived, 0, 0, None, None).await
}

async fn spawn_mock_discord_server_with_failures(
    initial_archived: bool,
    unarchive_failures_remaining: usize,
) -> (
    String,
    Arc<Mutex<MockDiscordState>>,
    tokio::task::JoinHandle<()>,
) {
    spawn_mock_discord_server_with_config(
        initial_archived,
        unarchive_failures_remaining,
        0,
        None,
        None,
    )
    .await
}

async fn spawn_mock_discord_server_with_message_length_failures(
    initial_archived: bool,
    message_length_failures_remaining: usize,
    message_length_failure_min_chars: usize,
) -> (
    String,
    Arc<Mutex<MockDiscordState>>,
    tokio::task::JoinHandle<()>,
) {
    spawn_mock_discord_server_with_config(
        initial_archived,
        0,
        message_length_failures_remaining,
        Some(message_length_failure_min_chars),
        None,
    )
    .await
}

async fn spawn_mock_discord_server_with_thread_creation_failure(
    status: axum::http::StatusCode,
) -> (
    String,
    Arc<Mutex<MockDiscordState>>,
    tokio::task::JoinHandle<()>,
) {
    spawn_mock_discord_server_with_config(false, 0, 0, None, Some(status)).await
}

async fn spawn_mock_discord_server_with_config(
    initial_archived: bool,
    unarchive_failures_remaining: usize,
    message_length_failures_remaining: usize,
    message_length_failure_min_chars: Option<usize>,
    thread_create_status: Option<axum::http::StatusCode>,
) -> (
    String,
    Arc<Mutex<MockDiscordState>>,
    tokio::task::JoinHandle<()>,
) {
    async fn get_channel(
        State(state): State<Arc<Mutex<MockDiscordState>>>,
        Path(thread_id): Path<String>,
    ) -> axum::response::Response {
        if thread_id == "thread-invalid-json" {
            let mut state = state.lock().unwrap();
            state.calls.push(format!("GET /channels/{thread_id}"));
            return (
                axum::http::StatusCode::OK,
                [("content-type", "application/json")],
                "not-json",
            )
                .into_response();
        }

        let (archived, thread_name, parent_id, total_message_sent) = {
            let mut state = state.lock().unwrap();
            state.calls.push(format!("GET /channels/{thread_id}"));
            let total_message_sent = if thread_id == "thread-stale" { 501 } else { 0 };
            (
                state.archived,
                state
                    .thread_names
                    .get(&thread_id)
                    .cloned()
                    .unwrap_or_else(|| format!("seed-{thread_id}")),
                state
                    .thread_parents
                    .get(&thread_id)
                    .cloned()
                    .unwrap_or_else(|| "123".to_string()),
                total_message_sent,
            )
        };
        (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({
                "id": thread_id,
                "name": thread_name,
                "parent_id": parent_id,
                "total_message_sent": total_message_sent,
                "thread_metadata": {
                    "archived": archived,
                }
            })),
        )
            .into_response()
    }

    async fn patch_channel(
        State(state): State<Arc<Mutex<MockDiscordState>>>,
        Path(thread_id): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let mut state = state.lock().unwrap();
        state.calls.push(format!("PATCH /channels/{thread_id}"));
        if body.get("archived").and_then(|value| value.as_bool()) == Some(false)
            && state.unarchive_failures_remaining > 0
        {
            state.unarchive_failures_remaining -= 1;
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"id": thread_id, "ok": false})),
            );
        }
        if let Some(name) = body.get("name").and_then(|value| value.as_str()) {
            state
                .thread_names
                .insert(thread_id.clone(), name.to_string());
        }
        if let Some(archived) = body.get("archived").and_then(|value| value.as_bool()) {
            state.archived = archived;
        }
        (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({"id": thread_id, "ok": true})),
        )
    }

    async fn create_thread(
        State(state): State<Arc<Mutex<MockDiscordState>>>,
        Path(channel_id): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let mut state = state.lock().unwrap();
        state
            .calls
            .push(format!("POST /channels/{channel_id}/threads"));
        if let Some(status) = state.thread_create_status {
            return (
                status,
                Json(serde_json::json!({
                    "message": "mock thread creation failure"
                })),
            );
        }
        let thread_id = "456".to_string();
        state
            .thread_parents
            .insert(thread_id.clone(), channel_id.clone());
        if let Some(name) = body.get("name").and_then(|value| value.as_str()) {
            state
                .thread_names
                .insert(thread_id.clone(), name.to_string());
        }
        (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({"id": thread_id})),
        )
    }

    async fn post_message(
        State(state): State<Arc<Mutex<MockDiscordState>>>,
        Path(channel_id): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let mut state = state.lock().unwrap();
        state
            .calls
            .push(format!("POST /channels/{channel_id}/messages"));
        let content = body
            .get("content")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        state
            .posted_messages
            .push((channel_id.clone(), content.clone()));
        if state.message_length_failures_remaining > 0
            && state
                .message_length_failure_min_chars
                .map(|limit| content.chars().count() >= limit)
                .unwrap_or(false)
        {
            state.message_length_failures_remaining -= 1;
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "code": 50035,
                    "message": "Invalid Form Body",
                    "errors": {
                        "content": {
                            "_errors": [
                                {
                                    "code": "BASE_TYPE_MAX_LENGTH",
                                    "message": "Must be 2000 or fewer in length."
                                }
                            ]
                        }
                    }
                })),
            );
        }
        (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({"id": format!("message-{channel_id}")})),
        )
    }

    async fn add_thread_member(
        State(state): State<Arc<Mutex<MockDiscordState>>>,
        Path((thread_id, user_id)): Path<(String, String)>,
    ) -> impl IntoResponse {
        let mut state = state.lock().unwrap();
        state.calls.push(format!(
            "PUT /channels/{thread_id}/thread-members/{user_id}"
        ));
        axum::http::StatusCode::NO_CONTENT
    }

    async fn add_reaction(
        State(state): State<Arc<Mutex<MockDiscordState>>>,
        Path((_channel_id, _message_id, _emoji)): Path<(String, String, String)>,
        uri: Uri,
    ) -> impl IntoResponse {
        let mut state = state.lock().unwrap();
        state.calls.push(format!("PUT {}", uri.path()));
        axum::http::StatusCode::NO_CONTENT
    }

    async fn remove_reaction(
        State(state): State<Arc<Mutex<MockDiscordState>>>,
        Path((_channel_id, _message_id, _emoji)): Path<(String, String, String)>,
        uri: Uri,
    ) -> impl IntoResponse {
        let mut state = state.lock().unwrap();
        state.calls.push(format!("DELETE {}", uri.path()));
        axum::http::StatusCode::NO_CONTENT
    }

    let state = Arc::new(Mutex::new(MockDiscordState {
        archived: initial_archived,
        unarchive_failures_remaining,
        message_length_failures_remaining,
        message_length_failure_min_chars,
        thread_create_status,
        calls: Vec::new(),
        posted_messages: Vec::new(),
        thread_names: HashMap::new(),
        thread_parents: HashMap::new(),
    }));
    let app = Router::new()
        .route(
            "/channels/{thread_id}",
            get(get_channel).patch(patch_channel),
        )
        .route("/channels/{channel_id}/threads", post(create_thread))
        .route("/channels/{channel_id}/messages", post(post_message))
        .route(
            "/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me",
            put(add_reaction).delete(remove_reaction),
        )
        .route(
            "/channels/{thread_id}/thread-members/{user_id}",
            put(add_thread_member),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state, handle)
}

#[tokio::test]
async fn add_thread_member_unarchives_archived_thread_before_put() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(true).await;
    let client = reqwest::Client::new();

    add_thread_member_to_dispatch_thread(&client, "announce-token", &base_url, "thread-1", 42)
        .await
        .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-1",
            "PATCH /channels/thread-1",
            "PUT /channels/thread-1/thread-members/42",
        ]
    );
}

#[tokio::test]
async fn dispatch_outbox_direct_v3_envelope_posts_success_metadata() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let client = reqwest::Client::new();

    let outcome = post_dispatch_message_to_channel_with_delivery(
        &client,
        "announce-token",
        &base_url,
        "123",
        "direct v3 dispatch message",
        "minimal fallback message",
        Some("dispatch-v3-outbox"),
    )
    .await
    .unwrap();

    server_handle.abort();
    assert_eq!(outcome.message_id, "message-123");
    assert_eq!(outcome.delivery.status, "success");
    assert_eq!(
        outcome.delivery.correlation_id.as_deref(),
        Some("dispatch:dispatch-v3-outbox")
    );
    assert_eq!(
        outcome.delivery.semantic_event_id.as_deref(),
        Some("dispatch:dispatch-v3-outbox:notify")
    );
    assert_eq!(outcome.delivery.target_channel_id.as_deref(), Some("123"));
    assert_eq!(outcome.delivery.message_id.as_deref(), Some("message-123"));
    assert_eq!(outcome.delivery.fallback_kind, None);

    let state = state.lock().unwrap();
    assert_eq!(state.calls, vec!["POST /channels/123/messages"]);
    assert_eq!(
        state.posted_messages,
        vec![("123".to_string(), "direct v3 dispatch message".to_string())]
    );
}

#[tokio::test]
async fn post_dispatch_message_retries_with_minimal_fallback_after_length_error() {
    let (base_url, state, server_handle) =
        spawn_mock_discord_server_with_message_length_failures(false, 1, 120).await;
    let client = reqwest::Client::new();
    let primary_message = "A".repeat(180);
    let minimal_message = "minimal fallback message";

    let outcome = post_dispatch_message_to_channel_with_delivery(
        &client,
        "announce-token",
        &base_url,
        "123",
        &primary_message,
        minimal_message,
        Some("dispatch-length-fallback"),
    )
    .await
    .unwrap();

    server_handle.abort();
    assert_eq!(outcome.message_id, "message-123");
    assert_eq!(outcome.delivery.status, "fallback");
    assert_eq!(
        outcome.delivery.semantic_event_id.as_deref(),
        Some("dispatch:dispatch-length-fallback:notify")
    );
    assert_eq!(outcome.delivery.target_channel_id.as_deref(), Some("123"));

    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec!["POST /channels/123/messages", "POST /channels/123/messages",]
    );
    assert_eq!(
        state.posted_messages,
        vec![
            ("123".to_string(), primary_message),
            ("123".to_string(), minimal_message.to_string()),
        ]
    );
}

#[tokio::test]
async fn reused_thread_length_error_does_not_fall_back_to_creating_new_thread() {
    let (base_url, state, server_handle) =
        spawn_mock_discord_server_with_message_length_failures(false, 2, 10).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map, active_thread_id,
                    created_at, updated_at
                ) VALUES (
                    'card-length', 'Length card', 'requested', 'agent-1', 'dispatch-length',
                    '{\"123\":\"thread-existing\"}', 'thread-existing',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-length', 'card-length', 'agent-1', 'implementation', 'pending', 'Length card',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
    }

    let error = send_dispatch_to_discord_inner_with_context(
        &db,
        "agent-1",
        "Length card",
        "card-length",
        "dispatch-length",
        "announce-token",
        &base_url,
        None,
    )
    .await
    .expect_err("length error after minimal retry should fail closed");

    server_handle.abort();
    assert!(error.contains("BASE_TYPE_MAX_LENGTH"));

    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-existing",
            "PATCH /channels/thread-existing",
            "POST /channels/thread-existing/messages",
            "POST /channels/thread-existing/messages",
        ]
    );
    assert!(
        !state
            .calls
            .contains(&"POST /channels/123/threads".to_string()),
        "length errors on a reused thread must not trigger new thread fallback"
    );

    let conn = db.lock().unwrap();
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-length'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(thread_id, None);
}

#[tokio::test]
async fn reused_thread_probe_error_falls_back_to_creating_new_thread_after_phase_gate_dispatch() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map, active_thread_id,
                    created_at, updated_at
                ) VALUES (
                    'card-probe-error', 'Probe error card', 'requested', 'agent-1', 'dispatch-probe-error',
                    '{\"123\":\"thread-invalid-json\"}', 'thread-invalid-json',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-probe-error', 'card-probe-error', 'agent-1', 'implementation', 'pending', 'Probe error card',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
    }

    send_dispatch_to_discord_inner_with_context(
        &db,
        "agent-1",
        "Probe error card",
        "card-probe-error",
        "dispatch-probe-error",
        "announce-token",
        &base_url,
        None,
    )
    .await
    .expect("non-length reuse probe errors should fall back to new thread creation");

    server_handle.abort();

    let state = state.lock().unwrap();
    assert_eq!(
        state.calls.first().map(String::as_str),
        Some("GET /channels/thread-invalid-json")
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/123/threads".to_string())
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/456/messages".to_string())
    );
    // #750: announce bot no longer writes dispatch-lifecycle emoji
    // reactions — no PUT/DELETE reaction calls should have been issued.
    assert!(
        !state.calls.iter().any(|call| call.contains("/reactions/")),
        "#750: expected no emoji reaction HTTP calls, got {:?}",
        state
            .calls
            .iter()
            .filter(|c| c.contains("/reactions/"))
            .collect::<Vec<_>>()
    );

    let conn = db.lock().unwrap();
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-probe-error'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(thread_id.as_deref(), Some("456"));
}

#[tokio::test]
async fn thread_creation_failure_records_parent_channel_send_as_fallback_delivery() {
    let (base_url, state, server_handle) = spawn_mock_discord_server_with_thread_creation_failure(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-thread-fallback', 'Thread fallback', 'requested', 'agent-1',
                    'dispatch-thread-fallback', datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                ) VALUES (
                    'dispatch-thread-fallback', 'card-thread-fallback', 'agent-1', 'implementation',
                    'pending', 'Thread fallback', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
    }

    let delivery = send_dispatch_to_discord_inner_with_context(
        &db,
        "agent-1",
        "Thread fallback",
        "card-thread-fallback",
        "dispatch-thread-fallback",
        "announce-token",
        &base_url,
        None,
    )
    .await
    .expect("parent-channel fallback delivery succeeds");

    server_handle.abort();

    assert_eq!(delivery.status, "fallback");
    assert_eq!(
        delivery.fallback_kind.as_deref(),
        Some("ThreadCreationParentChannel")
    );
    assert!(
        delivery
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("thread creation failed")
    );
    assert_eq!(delivery.target_channel_id.as_deref(), Some("123"));
    let state = state.lock().unwrap();
    assert!(
        state
            .calls
            .contains(&"POST /channels/123/threads".to_string())
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/123/messages".to_string())
    );
}

#[tokio::test]
async fn send_dispatch_to_discord_adds_configured_owner_to_created_thread() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-1', 'Test card', 'requested', 'agent-1', 'dispatch-1', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                ) VALUES (
                    'dispatch-1', 'card-1', 'agent-1', 'implementation', 'pending', 'Test card', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
    }

    send_dispatch_to_discord_inner_with_context(
        &db,
        "agent-1",
        "Test card",
        "card-1",
        "dispatch-1",
        "announce-token",
        &base_url,
        Some(343742347365974026),
    )
    .await
    .unwrap();

    server_handle.abort();

    let state = state.lock().unwrap();
    assert!(
        state
            .calls
            .contains(&"POST /channels/123/threads".to_string())
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/456/messages".to_string())
    );
    // #750: no emoji reaction calls — see other tests in this file for rationale.
    assert!(
        !state.calls.iter().any(|call| call.contains("/reactions/")),
        "#750: no emoji reaction HTTP calls expected"
    );
    assert!(state.calls.contains(&"GET /channels/456".to_string()));
    assert!(
        state
            .calls
            .contains(&"PUT /channels/456/thread-members/343742347365974026".to_string())
    );

    let conn = db.lock().unwrap();
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(thread_id.as_deref(), Some("456"));
    let context: Option<String> = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = 'dispatch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let context = serde_json::from_str::<serde_json::Value>(&context.unwrap()).unwrap();
    assert_eq!(context["discord_message_channel_id"], "456");
    assert_eq!(context["discord_message_id"], "message-456");
}

#[tokio::test]
async fn send_phase_gate_dispatch_to_discord_creates_thread() {
    let _env_lock = env_lock();
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
    let temp = tempfile::tempdir().unwrap();
    write_announce_token(temp.path());
    let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
    )
    .bind("agent-1")
    .bind("Agent 1")
    .bind("123")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, github_issue_number,
                created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
    )
    .bind("card-phase")
    .bind("Phase gate")
    .bind("review")
    .bind("agent-1")
    .bind("dispatch-phase")
    .bind(999_i64)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())",
    )
    .bind("dispatch-phase")
    .bind("card-phase")
    .bind("agent-1")
    .bind("phase-gate")
    .bind("pending")
    .bind("[phase-gate P2] Final")
    .bind(r#"{"phase_gate":{"run_id":"run-1","batch_phase":1}}"#)
    .execute(&pool)
    .await
    .unwrap();

    send_dispatch_to_discord_with_pg(
        Some(&sqlite),
        Some(&pool),
        "agent-1",
        "[phase-gate P2] Final",
        "card-phase",
        "dispatch-phase",
    )
    .await
    .unwrap();

    server_handle.abort();

    let state = state.lock().unwrap();
    assert!(
        state
            .calls
            .contains(&"POST /channels/123/threads".to_string()),
        "phase-gate dispatch should create a dispatch thread"
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/456/messages".to_string()),
        "phase-gate dispatch message should be posted into the created thread"
    );
    assert!(
        !state.calls.iter().any(|call| call.contains("/reactions/")),
        "#750: phase-gate dispatch must not write lifecycle emoji reactions"
    );

    let thread_id: Option<String> =
        sqlx::query_scalar("SELECT thread_id FROM task_dispatches WHERE id = $1")
            .bind("dispatch-phase")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(thread_id.as_deref(), Some("456"));
    let context: Option<String> =
        sqlx::query_scalar("SELECT context::text FROM task_dispatches WHERE id = $1")
            .bind("dispatch-phase")
            .fetch_one(&pool)
            .await
            .unwrap();
    let context = serde_json::from_str::<serde_json::Value>(&context.unwrap()).unwrap();
    assert_eq!(context["discord_message_channel_id"], "456");
    assert_eq!(context["discord_message_id"], "message-456");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn reused_thread_probe_error_falls_back_to_creating_new_thread() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map, active_thread_id,
                    created_at, updated_at
                ) VALUES (
                    'card-probe-error', 'Probe error card', 'requested', 'agent-1', 'dispatch-probe-error',
                    '{\"123\":\"thread-invalid-json\"}', 'thread-invalid-json',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-probe-error', 'card-probe-error', 'agent-1', 'implementation', 'pending', 'Probe error card',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
    }

    send_dispatch_to_discord_inner_with_context(
        &db,
        "agent-1",
        "Probe error card",
        "card-probe-error",
        "dispatch-probe-error",
        "announce-token",
        &base_url,
        None,
    )
    .await
    .expect("reused thread probe errors should fall back to new thread creation");

    server_handle.abort();

    let state = state.lock().unwrap();
    assert_eq!(
        state.calls.first().map(String::as_str),
        Some("GET /channels/thread-invalid-json")
    );
    assert!(
        !state
            .calls
            .contains(&"POST /channels/thread-invalid-json/messages".to_string())
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/123/threads".to_string())
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/456/messages".to_string())
    );
    assert!(
        !state.calls.iter().any(|call| call.contains("/reactions/")),
        "#750: announce bot must not write dispatch-lifecycle emoji reactions, got {:?}",
        state.calls
    );

    let conn = db.lock().unwrap();
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-probe-error'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(thread_id.as_deref(), Some("456"));
}

#[tokio::test]
async fn send_dispatch_reuses_recent_slot_thread_history_when_slot_map_is_empty() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, github_issue_number,
                    created_at, updated_at
                ) VALUES (
                    'card-current', 'Reuse card', 'requested', 'agent-1', 'dispatch-current', 506,
                    datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-old', 'Old card', 'done', 'agent-1', 'dispatch-old',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-current', 'card-current', 'agent-1', 'implementation', 'pending',
                    'Reuse card', '{\"slot_index\":1}', datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, thread_id,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-old', 'card-old', 'agent-1', 'implementation', 'completed',
                    'Old card', '{\"slot_index\":1}', 'thread-history',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES ('agent-1', 1, '{}')",
            [],
        )
        .unwrap();
    }

    send_dispatch_to_discord_inner_with_context(
        &db,
        "agent-1",
        "Reuse card",
        "card-current",
        "dispatch-current",
        "announce-token",
        &base_url,
        None,
    )
    .await
    .unwrap();

    server_handle.abort();

    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-history",
            "PATCH /channels/thread-history",
            "POST /channels/thread-history/messages",
        ]
    );
    assert!(
        !state.calls.iter().any(|call| call.contains("/reactions/")),
        "#750: announce bot must not write dispatch-lifecycle emoji reactions, got {:?}",
        state.calls
    );
    assert_eq!(
        state.thread_names.get("thread-history").map(String::as_str),
        Some("[slot 1] #506 Reuse card")
    );

    let conn = db.lock().unwrap();
    let reused_thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-current'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reused_thread_id.as_deref(), Some("thread-history"));

    let (active_thread_id, channel_thread_map): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT active_thread_id, channel_thread_map
                 FROM kanban_cards
                 WHERE id = 'card-current'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(active_thread_id.as_deref(), Some("thread-history"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(channel_thread_map.as_deref().unwrap()).unwrap()
            ["123"],
        "thread-history"
    );

    let slot_map: String = conn
        .query_row(
            "SELECT thread_id_map
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-1' AND slot_index = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&slot_map).unwrap()["123"],
        "thread-history"
    );

    let dispatch_context: Option<String> = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = 'dispatch-current'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let dispatch_context =
        serde_json::from_str::<serde_json::Value>(&dispatch_context.unwrap()).unwrap();
    assert_eq!(
        dispatch_context["discord_message_channel_id"],
        "thread-history"
    );
    assert_eq!(
        dispatch_context["discord_message_id"],
        "message-thread-history"
    );
}

#[tokio::test]
async fn send_dispatch_skips_recent_slot_thread_history_when_context_requests_reset() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, github_issue_number,
                    created_at, updated_at
                ) VALUES (
                    'card-current', 'Reset card', 'requested', 'agent-1', 'dispatch-current', 507,
                    datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-old', 'Old card', 'done', 'agent-1', 'dispatch-old',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
            [],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-current', 'card-current', 'agent-1', 'implementation', 'pending',
                    'Reset card', '{\"slot_index\":1,\"reset_slot_thread_before_reuse\":true}', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, thread_id,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-old', 'card-old', 'agent-1', 'implementation', 'completed',
                    'Old card', '{\"slot_index\":1}', 'thread-history',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES ('agent-1', 1, '{}')",
            [],
        )
        .unwrap();
    }

    send_dispatch_to_discord_inner_with_context(
        &db,
        "agent-1",
        "Reset card",
        "card-current",
        "dispatch-current",
        "announce-token",
        &base_url,
        None,
    )
    .await
    .unwrap();

    server_handle.abort();

    let state = state.lock().unwrap();
    assert!(
        !state
            .calls
            .contains(&"GET /channels/thread-history".to_string()),
        "reset context must not probe old slot-thread history"
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/123/threads".to_string())
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/456/messages".to_string())
    );

    let conn = db.lock().unwrap();
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-current'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(thread_id.as_deref(), Some("456"));
}

#[tokio::test]
async fn stale_slot_thread_reset_failure_fails_closed() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-current', 'Stale reset card', 'requested', 'agent-1', 'dispatch-current',
                    datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-other', 'Conflicting card', 'in_progress', 'agent-1', 'dispatch-other',
                    datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-current', 'card-current', 'agent-1', 'implementation', 'pending',
                    'Stale reset card', '{\"slot_index\":1}', datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-other', 'card-other', 'agent-1', 'implementation', 'dispatched',
                    'Conflicting card', '{\"slot_index\":1}', datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES ('agent-1', 1, '{\"123\":\"thread-stale\"}')",
            [],
        )
        .unwrap();
    }

    let error = send_dispatch_to_discord_inner_with_context(
        &db,
        "agent-1",
        "Stale reset card",
        "card-current",
        "dispatch-current",
        "announce-token",
        &base_url,
        None,
    )
    .await
    .expect_err("stale slot thread reset failures must fail closed");

    server_handle.abort();

    assert!(error.contains("has active dispatch"));

    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec!["GET /channels/thread-stale".to_string()],
        "reset failure must not continue into new-thread creation or reuse writes"
    );
}

/// #750: completed dispatches reach sync_dispatch_status_reaction only
/// for non-live completion paths (api/recovery/supervisor — gated by
/// `transition_source_is_live_command_bot` in set_dispatch_status_on_conn).
/// For those, the announce bot's ✅ is the only terminal signal, so the
/// sync runs the full reconcile: DELETE ⏳/❌ (@me, 404-tolerant), PUT ✅.
#[tokio::test]
#[ignore = "obsolete SQLite-only reaction sync fixture; PG coverage lives in sync_dispatch_status_reaction_with_pg_marks_completed_dispatch_success"]
async fn sync_dispatch_status_reaction_writes_success_cycle_for_completed_dispatch() {
    let _env_lock = env_lock();
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
    let temp = tempfile::tempdir().unwrap();
    write_announce_token(temp.path());
    let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-1', 'Complete card', 'in_progress', 'agent-1', 'dispatch-complete',
                    datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches
                 (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (
                    'dispatch-complete', 'card-1', 'agent-1', 'implementation', 'completed', 'Complete me',
                    '{\"discord_message_channel_id\":\"123\",\"discord_message_id\":\"message-123\"}',
                    datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();
    }

    sync_dispatch_status_reaction(&db, "dispatch-complete")
        .await
        .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    let reaction_calls: Vec<String> = state
        .calls
        .iter()
        .filter(|call| call.contains("/reactions/"))
        .cloned()
        .collect();
    assert_eq!(
        reaction_calls,
        vec![
            "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me".to_string(),
            "DELETE /channels/123/messages/message-123/reactions/%E2%9D%8C/@me".to_string(),
            "PUT /channels/123/messages/message-123/reactions/%E2%9C%85/@me".to_string(),
        ],
        "#750: completed dispatch (non-live source) must DELETE announce-bot's own ⏳/❌ then PUT ✅"
    );
}

/// #750: failed dispatches get the full failure reconcile — DELETE
/// announce-bot's own ⏳/✅ (404-tolerant) then PUT ❌. Command bot's
/// own ✅ (if added via turn_bridge:1537) is untouched (@me-scoped
/// deletes), but ❌ is the authoritative failure signal.
#[tokio::test]
#[ignore = "obsolete SQLite-only reaction sync fixture; dispatch reaction sync is PG-only after #868"]
async fn sync_dispatch_status_reaction_writes_failure_cycle_for_failed_dispatch() {
    let _env_lock = env_lock();
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
    let temp = tempfile::tempdir().unwrap();
    write_announce_token(temp.path());
    let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-1', 'Failed card', 'in_progress', 'agent-1', 'dispatch-failed',
                    datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches
                 (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (
                    'dispatch-failed', 'card-1', 'agent-1', 'implementation', 'failed', 'Fail me',
                    '{\"discord_message_channel_id\":\"123\",\"discord_message_id\":\"message-123\"}',
                    datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();
    }

    sync_dispatch_status_reaction(&db, "dispatch-failed")
        .await
        .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    let reaction_calls: Vec<String> = state
        .calls
        .iter()
        .filter(|call| call.contains("/reactions/"))
        .cloned()
        .collect();
    assert_eq!(
        reaction_calls,
        vec![
            "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me".to_string(),
            "DELETE /channels/123/messages/message-123/reactions/%E2%9C%85/@me".to_string(),
            "PUT /channels/123/messages/message-123/reactions/%E2%9D%8C/@me".to_string(),
        ],
        "#750: failed dispatch must DELETE announce-bot's own ⏳/✅ then PUT ❌ (clean signal, not mixed state)"
    );
}

#[tokio::test]
async fn sync_dispatch_status_reaction_with_pg_marks_completed_dispatch_success() {
    let _env_lock = env_lock();
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
    let temp = tempfile::tempdir().unwrap();
    write_announce_token(temp.path());
    let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
    )
    .bind("agent-1")
    .bind("Agent 1")
    .bind("123")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, NOW(), NOW())",
    )
    .bind("card-1")
    .bind("Complete card")
    .bind("in_progress")
    .bind("agent-1")
    .bind("dispatch-complete")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())",
        )
        .bind("dispatch-complete")
        .bind("card-1")
        .bind("agent-1")
        .bind("implementation")
        .bind("completed")
        .bind("Complete me")
        .bind("{\"discord_message_channel_id\":\"123\",\"discord_message_id\":\"message-123\"}")
        .execute(&pool)
        .await
        .unwrap();

    sync_dispatch_status_reaction_with_pg(Some(&sqlite), Some(&pool), "dispatch-complete")
        .await
        .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    let reaction_calls: Vec<String> = state
        .calls
        .iter()
        .filter(|call| call.contains("/channels/123/messages/message-123/reactions/"))
        .cloned()
        .collect();
    assert_eq!(
        reaction_calls,
        vec![
            "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me",
            "DELETE /channels/123/messages/message-123/reactions/%E2%9D%8C/@me",
            "PUT /channels/123/messages/message-123/reactions/%E2%9C%85/@me",
        ]
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1445: simulates the canonical bug — command-bot has added `⏳` at
/// turn start, then a repair path (queue/API cancel) drives the dispatch
/// to `failed` and announce-bot runs `apply_dispatch_status_reaction_state`.
/// Before the fix the announce-bot's `/@me` DELETE skipped command-bot's
/// `⏳`, leaving the message rendered as `⏳ + ❌` (in-progress vs failed
/// ambiguity). The fix issues a 404-tolerant `/@me` DELETE on each
/// provider's command-bot token so whichever provider owns `⏳` cleans up.
#[tokio::test]
async fn apply_dispatch_status_reaction_state_failed_clears_command_bot_pending() {
    let _env_lock = env_lock();
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
    let temp = tempfile::tempdir().unwrap();
    write_announce_token(temp.path());
    write_command_bot_token(temp.path(), "claude", "claude-token");
    write_command_bot_token(temp.path(), "codex", "codex-token");
    let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

    let target = DispatchMessageTarget {
        channel_id: "123".to_string(),
        message_id: "message-123".to_string(),
    };
    let token = crate::credential::read_bot_token("announce").unwrap();
    apply_dispatch_status_reaction_state(
        shared_discord_http_client(),
        &token,
        &base_url,
        &target,
        DispatchStatusReactionState::Failed,
    )
    .await
    .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    let reaction_calls: Vec<String> = state
        .calls
        .iter()
        .filter(|call| call.contains("/channels/123/messages/message-123/reactions/"))
        .cloned()
        .collect();
    assert_eq!(
        reaction_calls,
        vec![
            // announce-bot drops its own stale ⏳ (404-tolerant).
            "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me",
            // #1445: each provider command-bot also drops its own ⏳
            // (the 404 case for whichever bot didn't own it is fine).
            "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me",
            "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me",
            "DELETE /channels/123/messages/message-123/reactions/%E2%9C%85/@me",
            "PUT /channels/123/messages/message-123/reactions/%E2%9D%8C/@me",
        ],
        "#1445: failed dispatch must DELETE command-bot ⏳ via each provider token before announce-bot adds ❌ — final reaction state is ❌ only, never ⏳ + ❌"
    );
    assert!(
        !reaction_calls
            .iter()
            .any(|call| call.starts_with("PUT") && call.contains("%E2%8F%B3")),
        "#1445: must never re-add ⏳ on the failure path"
    );
}

fn insert_review_followup_fixture(db: &crate::db::Db) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                created_at, updated_at
            ) VALUES (
                'card-review', 'Review Card', 'review', 'agent-1', 'dispatch-review',
                '{\"123\":\"thread-primary\"}', datetime('now'), datetime('now')
            )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
            ) VALUES (
                'dispatch-review', 'card-review', 'agent-1', 'review', 'completed',
                '[Review R1] card-review', '{\"from_provider\":\"claude\"}',
                datetime('now'), datetime('now')
            )",
        [],
    )
    .unwrap();
}

async fn insert_review_followup_fixture_pg(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
    )
    .bind("agent-1")
    .bind("Agent 1")
    .bind("123")
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6::jsonb, NOW(), NOW()
             )",
    )
    .bind("card-review")
    .bind("Review Card")
    .bind("review")
    .bind("agent-1")
    .bind("dispatch-review")
    .bind(r#"{"123":"thread-primary"}"#)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7,
                NOW(), NOW()
             )",
    )
    .bind("dispatch-review")
    .bind("card-review")
    .bind("agent-1")
    .bind("review")
    .bind("completed")
    .bind("[Review R1] card-review")
    .bind(r#"{"from_provider":"claude"}"#)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn review_pass_notification_unarchives_and_posts_to_thread() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(true).await;
    let db = test_db();
    insert_review_followup_fixture(&db);

    send_review_result_to_primary_with_context(
        &db,
        "card-review",
        "dispatch-review",
        "pass",
        Some("announce-token"),
        &base_url,
    )
    .await
    .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-primary",
            "PATCH /channels/thread-primary",
            "POST /channels/thread-primary/messages",
        ]
    );
}

#[tokio::test]
async fn review_notification_dedupes_same_dispatch_event() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    insert_review_followup_fixture(&db);

    for _ in 0..2 {
        send_review_result_message_via_http(
            Some(&db),
            None,
            "review-1166-dedup",
            "card-review",
            123,
            "✅ [리뷰 통과] Review Card — done으로 이동",
            ReviewFollowupKind::Pass,
            "announce-token",
            &base_url,
        )
        .await
        .unwrap();
    }

    server_handle.abort();
    let state = state.lock().unwrap();
    let post_count = state
        .calls
        .iter()
        .filter(|call| call.as_str() == "POST /channels/thread-primary/messages")
        .count();
    assert_eq!(post_count, 1, "duplicate review event must not post twice");
}

#[tokio::test]
async fn review_notification_truncates_over_2000_chars() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    insert_review_followup_fixture(&db);
    let message = format!("{}{}", "✅ [리뷰 통과] ", "A".repeat(2_100));

    send_review_result_message_via_http(
        Some(&db),
        None,
        "review-1166-overflow",
        "card-review",
        123,
        &message,
        ReviewFollowupKind::Pass,
        "announce-token",
        &base_url,
    )
    .await
    .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    assert_eq!(state.posted_messages.len(), 1);
    let posted = &state.posted_messages[0].1;
    assert!(posted.chars().count() <= 2_000);
    assert!(posted.contains("[… truncated]"));
}

#[tokio::test]
async fn review_pass_notification_uses_primary_thread_even_when_review_context_points_to_alt_channel()
 {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                    created_at, updated_at
                ) VALUES (
                    'card-review-alt', 'Review Card', 'review', 'agent-1', 'dispatch-review-alt',
                    '{\"123\":\"thread-impl\"}', datetime('now'), datetime('now')
                )",
            [],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-alt', 'card-review-alt', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-alt', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
    }

    send_review_result_to_primary_with_context(
        &db,
        "card-review-alt",
        "dispatch-review-alt",
        "pass",
        Some("announce-token"),
        &base_url,
    )
    .await
    .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-impl",
            "POST /channels/thread-impl/messages",
        ]
    );
    assert!(
        !state
            .calls
            .contains(&"POST /channels/456/messages".to_string()),
        "review followup must not fall back to the review channel"
    );
}

#[tokio::test]
async fn review_pass_notification_falls_back_to_primary_channel_when_no_implementation_thread_exists()
 {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-review-fallback', 'Review Card', 'review', 'agent-1', 'dispatch-review-fallback',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-fallback', 'card-review-fallback', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-fallback', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
    }

    send_review_result_to_primary_with_context(
        &db,
        "card-review-fallback",
        "dispatch-review-fallback",
        "pass",
        Some("announce-token"),
        &base_url,
    )
    .await
    .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    assert_eq!(state.calls, vec!["POST /channels/123/messages"]);
    assert!(
        !state
            .calls
            .contains(&"POST /channels/456/messages".to_string()),
        "review followup fallback must use the implementation channel"
    );
}

#[tokio::test]
async fn review_pass_notification_reuses_latest_work_dispatch_thread_when_channel_map_is_missing() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-review-history', 'Review Card', 'review', 'agent-1', 'dispatch-review-history',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-impl-history', 'card-review-history', 'agent-1', 'implementation', 'completed',
                    'Implementation', 'thread-history', datetime('now', '-1 minute'), datetime('now', '-1 minute')
                )",
                [],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-history', 'card-review-history', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-history', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
    }

    send_review_result_to_primary_with_context(
        &db,
        "card-review-history",
        "dispatch-review-history",
        "pass",
        Some("announce-token"),
        &base_url,
    )
    .await
    .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-history",
            "POST /channels/thread-history/messages",
        ]
    );
    assert!(
        !state
            .calls
            .contains(&"POST /channels/456/messages".to_string()),
        "latest work thread must win over the review channel"
    );
}

#[tokio::test]
async fn review_pass_notification_does_not_fallback_to_parent_when_unarchive_fails() {
    let (base_url, state, server_handle) = spawn_mock_discord_server_with_failures(true, 2).await;
    let db = test_db();
    insert_review_followup_fixture(&db);

    let err = send_review_result_to_primary_with_context(
        &db,
        "card-review",
        "dispatch-review",
        "pass",
        Some("announce-token"),
        &base_url,
    )
    .await
    .expect_err("review pass should fail closed when thread unarchive keeps failing");

    server_handle.abort();
    assert!(err.contains("failed to unarchive review followup thread thread-primary"));

    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-primary",
            "PATCH /channels/thread-primary",
            "PATCH /channels/thread-primary",
        ]
    );
    assert!(
        !state
            .calls
            .contains(&"POST /channels/123/messages".to_string()),
        "main channel fallback must not happen when the mapped thread still exists"
    );
}

#[tokio::test]
async fn review_pass_notification_uses_postgres_thread_map_and_channel_resolution() {
    let (base_url, state, server_handle) = spawn_mock_discord_server(true).await;
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    insert_review_followup_fixture_pg(&pool).await;

    let transport = HttpDispatchTransport {
        announce_bot_token: Some("announce-token".to_string()),
        discord_api_base: base_url.clone(),
        thread_owner_user_id: None,
        pg_pool: Some(pool.clone()),
    };
    send_review_result_to_primary_with_context_and_transport(
        Some(&db),
        "card-review",
        "dispatch-review",
        "pass",
        &transport,
    )
    .await
    .unwrap();

    server_handle.abort();
    let state = state.lock().unwrap();
    assert_eq!(
        state.calls,
        vec![
            "GET /channels/thread-primary",
            "PATCH /channels/thread-primary",
            "POST /channels/thread-primary/messages",
        ]
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn send_dispatch_to_discord_with_pg_creates_thread_and_persists_context() {
    let _env_lock = env_lock();
    let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
    let temp = tempfile::tempdir().unwrap();
    write_announce_token(temp.path());
    let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
    )
    .bind("agent-1")
    .bind("Agent 1")
    .bind("123")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, github_issue_number, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("card-1")
        .bind("PG card")
        .bind("requested")
        .bind("agent-1")
        .bind("dispatch-1")
        .bind(701_i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-1")
        .bind("card-1")
        .bind("agent-1")
        .bind("implementation")
        .bind("pending")
        .bind("PG card")
        .execute(&pool)
        .await
        .unwrap();

    send_dispatch_to_discord_with_pg(
        Some(&sqlite),
        Some(&pool),
        "agent-1",
        "PG card",
        "card-1",
        "dispatch-1",
    )
    .await
    .unwrap();

    server_handle.abort();

    let state = state.lock().unwrap();
    assert!(
        state
            .calls
            .contains(&"POST /channels/123/threads".to_string()),
        "pg delivery should create a dispatch thread"
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/456/messages".to_string()),
        "pg delivery should post the dispatch message into the created thread"
    );
    assert!(
        !state.calls.iter().any(|call| call.contains("/reactions/")),
        "#750: announce bot must not write dispatch-lifecycle emoji reactions, got {:?}",
        state.calls
    );

    let thread_id: Option<String> =
        sqlx::query_scalar("SELECT thread_id FROM task_dispatches WHERE id = $1")
            .bind("dispatch-1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(thread_id.as_deref(), Some("456"));

    let context: Option<String> =
        sqlx::query_scalar("SELECT context FROM task_dispatches WHERE id = $1")
            .bind("dispatch-1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let context = serde_json::from_str::<serde_json::Value>(&context.unwrap()).unwrap();
    assert_eq!(context["discord_message_channel_id"], "456");
    assert_eq!(context["discord_message_id"], "message-456");
    assert_eq!(context["slot_index"], 0);

    let channel_thread_map: Option<String> =
        sqlx::query_scalar("SELECT channel_thread_map::text FROM kanban_cards WHERE id = $1")
            .bind("card-1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&channel_thread_map.unwrap()).unwrap()["123"],
        "456"
    );

    let slot_map: Option<String> = sqlx::query_scalar(
        "SELECT thread_id_map::text
             FROM auto_queue_slots
             WHERE agent_id = $1 AND slot_index = 0",
    )
    .bind("agent-1")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&slot_map.unwrap()).unwrap()["123"],
        "456"
    );
}

#[tokio::test]
async fn review_decision_resolves_free_slot_and_skips_card_thread_candidate_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
    )
    .bind("agent-1")
    .bind("Agent 1")
    .bind("123")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6::jsonb, NOW(), NOW()
             )",
    )
    .bind("card-1")
    .bind("Review decision card")
    .bind("in_progress")
    .bind("agent-1")
    .bind("dispatch-work")
    .bind(r#"{"123":"thread-work"}"#)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES
                ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW()),
                ($8, $2, $3, $9, $10, $11, $12, NOW(), NOW())",
    )
    .bind("dispatch-work")
    .bind("card-1")
    .bind("agent-1")
    .bind("implementation")
    .bind("dispatched")
    .bind("Implementation")
    .bind(r#"{"slot_index":0}"#)
    .bind("dispatch-review-decision")
    .bind("review-decision")
    .bind("pending")
    .bind("Review decision")
    .bind("{}")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ($1, 0, $2::jsonb), ($1, 1, '{}'::jsonb)",
    )
    .bind("agent-1")
    .bind(r#"{"123":"thread-work"}"#)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, $2, $3)",
    )
    .bind("run-1")
    .bind("agent-1")
    .bind("active")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group
             ) VALUES ($1, $2, $3, $4, $5, $6, 0, 0)",
    )
    .bind("entry-work")
    .bind("run-1")
    .bind("card-1")
    .bind("agent-1")
    .bind("dispatched")
    .bind("dispatch-work")
    .execute(&pool)
    .await
    .unwrap();

    let dispatch_context = serde_json::json!({});
    let binding = resolve_slot_thread_binding_pg(
        &pool,
        "agent-1",
        "card-1",
        "dispatch-review-decision",
        Some(&dispatch_context),
        Some("review-decision"),
        123,
    )
    .await
    .unwrap()
    .expect("review-decision should claim a free slot");

    assert_eq!(binding.slot_index, 1);
    assert!(binding.thread_id.is_none());

    let candidates = collect_slot_thread_candidates_pg(
        &pool,
        "agent-1",
        "card-1",
        Some(&binding),
        123,
        false,
        true,
    )
    .await
    .unwrap();
    assert!(candidates.is_empty());
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate == "thread-work"),
        "review-decision must not reuse the active work card thread"
    );
    let card_candidates = collect_slot_thread_candidates_pg(
        &pool,
        "agent-1",
        "card-1",
        Some(&binding),
        123,
        true,
        true,
    )
    .await
    .unwrap();
    assert!(
        card_candidates
            .iter()
            .any(|candidate| candidate == "thread-work"),
        "test fixture must prove the card-thread candidate would be reused without the independent-slot guard"
    );

    let persisted_context: String =
        sqlx::query_scalar("SELECT context FROM task_dispatches WHERE id = $1")
            .bind("dispatch-review-decision")
            .fetch_one(&pool)
            .await
            .unwrap();
    let persisted_context = serde_json::from_str::<serde_json::Value>(&persisted_context).unwrap();
    assert_eq!(persisted_context["slot_index"], 1);

    sqlx::query(
        "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())",
    )
    .bind("dispatch-full")
    .bind("card-1")
    .bind("agent-1")
    .bind("review-decision")
    .bind("pending")
    .bind("Review decision with full pool")
    .bind("{}")
    .execute(&pool)
    .await
    .unwrap();
    for slot_index in 2..SLOT_THREAD_MAX_SLOTS {
        sqlx::query(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, 0)",
            )
            .bind(format!("entry-active-{slot_index}"))
            .bind("run-1")
            .bind("card-1")
            .bind("agent-1")
            .bind("dispatched")
            .bind(format!("dispatch-active-{slot_index}"))
            .bind(slot_index)
            .execute(&pool)
            .await
            .unwrap();
    }

    let err = resolve_slot_thread_binding_pg(
        &pool,
        "agent-1",
        "card-1",
        "dispatch-full",
        Some(&dispatch_context),
        Some("review-decision"),
        123,
    )
    .await
    .expect_err("review-decision should fail closed when every slot is active");
    assert!(err.contains("no free slot available"));

    pool.close().await;
    pg_db.drop().await;
}
