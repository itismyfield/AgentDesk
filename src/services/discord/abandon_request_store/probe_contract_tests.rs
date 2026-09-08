//! #5305: actual renderer -> HTTP probe -> durable drain -> gateway PATCH.
use super::*;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::any};
use serde_json::{Value, json};
use std::time::Duration;

use crate::services::discord::{
    inflight::{InflightTurnState, save_inflight_state},
    make_shared_data_for_tests,
    placeholder_live_events::{rendered_answers_for_probe_tests, rendered_panels_for_probe_tests},
};

struct RuntimeRootGuard {
    _env: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
}

fn isolated_root() -> (std::sync::MutexGuard<'static, ()>, RuntimeRootGuard) {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("isolated drain root");
    let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    (
        lock,
        RuntimeRootGuard {
            _env: env,
            _root: root,
        },
    )
}

#[derive(Clone)]
struct WireState {
    channel: u64,
    message: u64,
    content: String,
    fail: Option<Method>,
    requests: Arc<Mutex<Vec<(Method, String, Option<Value>)>>>,
}

impl WireState {
    fn path(&self) -> String {
        format!(
            "/api/v10/channels/{}/messages/{}",
            self.channel, self.message
        )
    }

    fn message_json(&self, content: &str) -> Value {
        json!({
            "id": self.message.to_string(), "channel_id": self.channel.to_string(),
            "author": { "id": "530500", "username": "drain-test", "discriminator": "0001",
                "avatar": null, "bot": true, "public_flags": 0 },
            "content": content, "timestamp": "2026-09-08T00:00:00.000000+00:00",
            "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "mention_channels": [], "attachments": [],
            "embeds": [], "reactions": [], "nonce": null, "pinned": false, "type": 0,
            "flags": 0, "components": [], "sticker_items": [], "message_snapshots": []
        })
    }
}

async fn discord_rest(State(state): State<WireState>, request: Request<Body>) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 16_384)
        .await
        .unwrap();
    let payload: Option<Value> =
        (!bytes.is_empty()).then(|| serde_json::from_slice(&bytes).expect("Discord JSON body"));
    state
        .requests
        .lock()
        .unwrap()
        .push((method.clone(), path.clone(), payload.clone()));
    if path != state.path() || !matches!(method, Method::GET | Method::PATCH) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if state.fail.as_ref() == Some(&method) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "code": 0, "message": "transient fixture failure"
            })),
        )
            .into_response();
    }
    let content = payload
        .as_ref()
        .and_then(|body| body["content"].as_str())
        .unwrap_or(&state.content);
    Json(state.message_json(content)).into_response()
}

struct Case {
    content: String,
    terminal: TerminalCardStatus,
    owner: Option<(u64, u64)>, // user-message delta, save-revision delta
    fail: Option<Method>,
    gets: usize,
    patches: usize,
    cleared: usize,
}

impl Case {
    fn delivered(content: String) -> Self {
        Self {
            content,
            terminal: TerminalCardStatus::Completed,
            owner: None,
            fail: None,
            gets: 1,
            patches: 0,
            cleared: 1,
        }
    }
}

async fn assert_drain_case(index: usize, case: Case, shared: &Arc<super::super::SharedData>) {
    let channel = 530_510 + index as u64;
    let token = format!("probe-contract-{index}");
    let provider = ProviderKind::Claude;
    let record = AbandonRecord {
        msg_id: channel + 100,
        started_at: "2026-09-08 00:00:00".into(),
        current_tool_line: Some("fixture tool".into()),
        terminal_status: case.terminal,
        episode: AbandonEpisodeIdentity {
            user_msg_id: channel + 200,
            started_at: format!("2026-09-08 00:00:{index:02}"),
            status_panel_generation: 1,
            save_generation: 1,
        },
    };
    if let Some((user_delta, revision_delta)) = case.owner {
        let mut owner = InflightTurnState::new(
            provider.clone(),
            channel,
            None,
            1,
            record.episode.user_msg_id + user_delta,
            record.msg_id,
            "fixture".into(),
            None,
            None,
            None,
            None,
            0,
        );
        owner.started_at = record.episode.started_at.clone();
        owner.status_message_id = Some(record.msg_id);
        owner.status_panel_generation = record.episode.status_panel_generation;
        let expected_generation = record.episode.save_generation + revision_delta;
        // The canonical seed writer increments the supplied revision on disk.
        // Seed its predecessor so same/new-revision fences retain their meaning.
        owner.save_generation = expected_generation - 1;
        save_inflight_state(&owner).expect("seed actual owner under isolated root");
        let loaded = super::super::inflight::load_inflight_state(&provider, channel).unwrap();
        assert_eq!(
            (loaded.user_msg_id, loaded.save_generation),
            (owner.user_msg_id, expected_generation)
        );
    }
    enqueue(&provider, &token, channel, record.clone()).expect("durable enqueue");
    let before = vec![(channel, record.clone())];
    assert_eq!(
        load_pending(&provider, &token),
        before,
        "case {index}: precondition"
    );

    let wire = WireState {
        channel,
        message: record.msg_id,
        content: case.content,
        fail: case.fail,
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    // The same Http drives probe GET and the actual gateway's PATCH.
    let app = Router::new()
        .fallback(any(discord_rest))
        .with_state(wire.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build(),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        drain(&http, shared, &provider, &token),
    )
    .await;
    server.abort();
    let stopped = server.await.expect_err("fixture should stop on abort");
    assert!(stopped.is_cancelled(), "fixture must not panic");
    let cleared = outcome.expect("bounded real HTTP drain");
    let requests = wire.requests.lock().unwrap();
    let get_paths: Vec<_> = requests
        .iter()
        .filter(|(method, _, _)| *method == Method::GET)
        .map(|(_, path, body)| {
            assert!(body.is_none());
            path.clone()
        })
        .collect();
    let patches: Vec<_> = requests
        .iter()
        .filter(|(method, _, _)| *method == Method::PATCH)
        .map(|(_, path, body)| {
            (
                path.clone(),
                body.as_ref().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect();
    let card = build_terminal_card(&record);
    assert!(card.contains(match case.terminal {
        TerminalCardStatus::Completed => "응답 완료",
        TerminalCardStatus::Aborted => "응답 중단",
    }));
    let expected_patches = vec![(wire.path(), card); case.patches];
    let after = load_pending(&provider, &token);
    assert_eq!(
        (requests.len(), get_paths, patches, cleared, after),
        (
            case.gets + case.patches,
            vec![wire.path(); case.gets],
            expected_patches,
            case.cleared,
            if case.cleared == 0 { before } else { vec![] }
        ),
        "case {index}: request identity/count/content and durable outcome must agree"
    );
}

#[test]
fn drain_http_contract_covers_live_terminal_prose_fences_and_transient_failures() {
    // Bind separately: runtime and all fixture/finalizer tasks drop before the
    // root restores the environment, and root drops before releasing the lock.
    let (_lock, _root) = isolated_root();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let shared = make_shared_data_for_tests();
        assert!(shared.pg_pool.is_none());
        let panels = rendered_panels_for_probe_tests();
        let live = panels[0].0.clone();
        let mut cases = Vec::new();
        for terminal in [TerminalCardStatus::Completed, TerminalCardStatus::Aborted] {
            cases.push(Case {
                terminal,
                patches: 1,
                ..Case::delivered(live.clone())
            });
        }
        for prose in rendered_answers_for_probe_tests() {
            cases.push(Case::delivered(prose));
        }
        let terminal_panels: Vec<_> = panels.into_iter().filter(|(_, live)| !live).collect();
        assert_eq!(terminal_panels.len(), 2);
        for (terminal, _) in terminal_panels {
            cases.push(Case::delivered(terminal));
        }
        // Real inflight snapshots force each of the three pre-probe fences.
        for (owner, cleared) in [((0, 0), 0), ((0, 1), 0), ((1, 0), 1)] {
            cases.push(Case {
                owner: Some(owner),
                gets: 0,
                cleared,
                ..Case::delivered(live.clone())
            });
        }
        // Disabled SDK ratelimiter performs one request without 5xx retries;
        // pin R=1 instead of accepting an unbounded number of observed calls.
        cases.push(Case {
            fail: Some(Method::GET),
            cleared: 0,
            ..Case::delivered(live.clone())
        });
        cases.push(Case {
            fail: Some(Method::PATCH),
            patches: 1,
            cleared: 0,
            ..Case::delivered(live)
        });
        assert_eq!(cases.len(), 12);
        for (index, case) in cases.into_iter().enumerate() {
            assert_drain_case(index, case, &shared).await;
        }
    });
    // Dropping this owned runtime cancels/joins its finalizer before env restore,
    // including on assertion unwind. No background task escapes this test.
    drop(runtime);
}
