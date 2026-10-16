//! Actual sweep coverage with Legacy host admission over every obligation shape.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::host_defer_gate::tests::{
    Nameless, ScriptedTmux, map_channel, postgres,
};
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, seed, shared_on,
};
use axum::{Json, Router, http::Method, http::StatusCode, http::Uri, response::IntoResponse};

#[derive(Clone, Copy, Debug)]
enum Shape {
    Invalid,
    Stale,
    GenerationMismatch,
    MissingExpired,
    Complete,
    Deadline,
    RestError,
}

impl Shape {
    const ALL: [Self; 7] = [
        Self::Invalid,
        Self::Stale,
        Self::GenerationMismatch,
        Self::MissingExpired,
        Self::Complete,
        Self::Deadline,
        Self::RestError,
    ];
}

#[derive(Clone, Debug)]
struct Request {
    channel: u64,
    content: String,
}

struct MockRest {
    http: Arc<serenity::Http>,
    log: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockRest {
    async fn new() -> Self {
        let log = Arc::new(Mutex::new(Vec::new()));
        let recorded = log.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, Json(body): Json<serde_json::Value>| {
                let log = recorded.clone();
                async move {
                    assert_eq!(method, Method::PATCH, "the real sweep edits through PATCH");
                    let parts: Vec<_> = uri.path().split('/').collect();
                    let channel_at = parts.iter().position(|part| *part == "channels").unwrap();
                    let channel: u64 = parts[channel_at + 1].parse().unwrap();
                    let id = parts.last().unwrap().parse::<u64>().unwrap();
                    let content = body["content"].as_str().unwrap().to_string();
                    log.lock().unwrap().push(Request {
                        channel,
                        content: content.clone(),
                    });
                    if content == "rest-error" {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "code": 50035, "message": "fixture edit rejection"
                            })),
                        )
                            .into_response();
                    }
                    Json(serde_json::json!({
                    "id": id.to_string(), "channel_id": channel.to_string(), "content": content,
                    "author": {"id":"1", "username":"bot", "discriminator":"0001", "avatar":null},
                    "timestamp":"2026-10-01T00:00:00+00:00", "edited_timestamp":null,
                    "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                    "attachments":[], "embeds":[], "pinned":false, "type":0
                })).into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { http, log, task }
    }

    fn requests(&self, channel: u64) -> Vec<Request> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.channel == channel)
            .cloned()
            .collect()
    }
}

impl Drop for MockRest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn obligation_root() -> PathBuf {
    runtime_store::discord_terminal_ui_obligations_root().expect("isolated obligation root")
}

fn sample_obligation(channel: u64) -> TerminalUiObligation {
    TerminalUiObligation {
        channel_id: channel,
        provider: "claude".to_string(),
        status_message_id: 456,
        generation_mtime_ns: 789,
        pending_state_text: "pending".to_string(),
        completion_text: "complete".to_string(),
        deadline_text: "deadline".to_string(),
        created_unix: 1000,
        deadline_unix: 1600,
    }
}

async fn fixture(shared: &Arc<SharedData>, pool: &sqlx::PgPool, channel: u64, shape: Shape) {
    let name = ProviderKind::Claude.build_tmux_session_name(&format!("ui-input-fence-{channel}"));
    if matches!(shape, Shape::MissingExpired) {
        Nameless::Legacy
            .seed(
                pool,
                &shared.token_hash,
                "ui-fence-unused-bot",
                channel,
                &name,
            )
            .await;
    } else {
        seed(
            pool,
            &channel_key(shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
    }
    let now = terminal_ui_obligation_now_unix();
    let mut obligation = TerminalUiObligation {
        channel_id: channel,
        provider: "claude".to_string(),
        status_message_id: channel + 10,
        generation_mtime_ns: 789,
        pending_state_text: "pending".to_string(),
        completion_text: "complete".to_string(),
        deadline_text: "deadline".to_string(),
        created_unix: now - 120,
        deadline_unix: now + 3600,
    };
    let mut state = None;
    if !matches!(shape, Shape::MissingExpired) {
        map_channel(
            shared,
            ChannelId::new(channel),
            &format!("ui-input-fence-{channel}"),
        )
        .await;
        let output = obligation_root().join(format!("{channel}.jsonl"));
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        let text = if matches!(shape, Shape::Deadline) {
            "{\"type\":\"user\",\"message\":{\"content\":\"busy\"}}\n"
        } else {
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"ui-fixture\"}\n"
        };
        fs::write(&output, text).unwrap();
        let generation = crate::services::tmux_common::session_temp_path(&name, "generation");
        fs::create_dir_all(Path::new(&generation).parent().unwrap()).unwrap();
        fs::write(generation, "1").unwrap();
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            &name,
            RuntimeHandoffKind::ClaudeTui,
        )
        .unwrap();
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            channel,
            None,
            1,
            channel + 1,
            channel + 2,
            "terminal UI fixture".to_string(),
            None,
            Some(name),
            Some(output.to_string_lossy().into_owned()),
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.status_message_id = Some(if matches!(shape, Shape::Stale) {
            channel + 11
        } else {
            channel + 10
        });
        row.last_watcher_relayed_generation_mtime_ns =
            Some(if matches!(shape, Shape::GenerationMismatch) {
                790
            } else {
                789
            });
        inflight::save_inflight_state_create_new(&row).expect("fixture row");
        state = Some(row);
    }
    if matches!(shape, Shape::Invalid) {
        obligation.status_message_id = 0;
    }
    if matches!(
        shape,
        Shape::MissingExpired | Shape::Deadline | Shape::RestError
    ) {
        obligation.deadline_unix = now - 120;
    }
    if matches!(shape, Shape::RestError) {
        obligation.completion_text = "rest-error".to_string();
    }
    assert!(
        !terminal_ui_host_deferred(
            shared,
            &ProviderKind::Claude,
            ChannelId::new(channel),
            state.as_ref()
        )
        .await,
        "Legacy host evidence must admit {shape:?}"
    );
    write_obligation_in_root(&obligation_root(), &obligation).unwrap();
}

// Each shape follows the sweep's resolve, judge, edit and clear rules.
#[tokio::test]
async fn sweep_reconciles_every_obligation_shape_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let rest = MockRest::new().await;
    for (s, shape) in Shape::ALL.into_iter().enumerate() {
        fixture(&shared, &pool, 6_325_885_000 + s as u64, shape).await;
    }
    write_obligation_in_root(&obligation_root(), &sample_obligation(0)).unwrap();
    sweep_terminal_ui_obligations(&rest.http, &shared, &ProviderKind::Claude).await;
    assert!(!obligation_path_in_root(&obligation_root(), "claude", 0).exists());
    for (s, shape) in Shape::ALL.into_iter().enumerate() {
        let channel = 6_325_885_000 + s as u64;
        assert!(
            !obligation_path_in_root(&obligation_root(), "claude", channel).exists(),
            "{shape:?} clears"
        );
        let expected = match shape {
            Shape::Complete => Some("complete"),
            Shape::Deadline => Some("deadline"),
            Shape::RestError => Some("rest-error"),
            _ => None,
        };
        let requests = rest.requests(channel);
        assert_eq!(
            requests.len(),
            usize::from(expected.is_some()),
            "{shape:?} REST count"
        );
        if let Some(content) = expected {
            assert_eq!(requests[0].content, content);
        }
    }
    pool.close().await;
    db.drop().await;
}
