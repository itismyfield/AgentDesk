//! Stale-leak recovery keeps the Legacy send it claimed counted through its edit and every
//! continuation post, and lets it go only when the pass returns.

use std::sync::{Arc, Mutex};

use axum::{Json, Router, http::Method, http::Uri, routing::any};
use poise::serenity_prelude::{self as serenity, ChannelId};

use super::super::HealthRegistry;
use super::maybe_recover_completed_stale_leak;
use crate::config::TestEnvVarGuard;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::shared_state::test_rest;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::cutover::test_override;

const CHANNEL: u64 = 6_737_101;
const PLACEHOLDER: u64 = 6_737_102;

/// Each write the pass makes, with the channel's Legacy sends as it reached Discord.
type Writes = Arc<Mutex<Vec<(Method, (u64, u64))>>>;

#[tokio::test(flavor = "current_thread")]
async fn a_stale_leak_resend_stays_counted_through_its_edit_and_continuations() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let _candidates = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let candidate = test_override::with_channels(|boot| boot?.candidate(CHANNEL).cloned());
    let candidate = candidate.unwrap();
    let writes = Writes::default();
    let (seen, adoption) = (writes.clone(), candidate.clone());
    let app = Router::new().fallback(any(move |method: Method, uri: Uri| {
        let (seen, adoption) = (seen.clone(), adoption.clone());
        async move {
            let author = serde_json::json!({"id": "1", "username": "bot", "discriminator": "0001", "avatar": null, "bot": true});
            if uri.path().ends_with("/users/@me") {
                return Json(author);
            }
            let mut seen = seen.lock().unwrap();
            if method != Method::GET {
                seen.push((method.clone(), adoption.sends()));
            }
            let id = match method {
                Method::POST => PLACEHOLDER + seen.len() as u64,
                _ => PLACEHOLDER,
            };
            Json(serde_json::json!({
                "id": id.to_string(), "channel_id": CHANNEL.to_string(), "content": "",
                "author": author, "timestamp": "2026-10-09T00:00:00+00:00",
                "edited_timestamp": null, "tts": false, "mention_everyone": false,
                "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
                "pinned": false, "type": 0
            }))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = serenity::HttpBuilder::new("test-token")
        .proxy(format!("http://{}", listener.local_addr().unwrap()))
        .ratelimiter_disabled(true)
        .build();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _rest = test_rest::install(Arc::new(http));

    let state: InflightTurnState = serde_json::from_value(serde_json::json!({
        "version": 9, "provider": "claude", "channel_id": CHANNEL, "channel_name": "adk-cc",
        "request_owner_user_id": 7, "user_msg_id": 6_737_103, "current_msg_id": PLACEHOLDER,
        "current_msg_len": 0, "user_text": "prompt", "source": "text", "session_id": "session",
        "tmux_session_name": "AgentDesk-claude-adk-cc", "output_path": "/tmp/o-leak.jsonl",
        "input_fifo_path": null, "last_offset": 0,
        "full_response": "leaked answer line\n".repeat(200),
        "response_sent_offset": 0, "relay_owner_kind": "watcher", "runtime_kind": "claude_tui",
        "started_at": "2026-01-01 00:00:00", "updated_at": "2026-01-01 00:00:00"
    }))
    .unwrap();
    crate::services::discord::inflight::save_inflight_state(&state).unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let registry = HealthRegistry::new();
    registry.register("claude".into(), shared.clone()).await;

    let recovered = maybe_recover_completed_stale_leak(
        &registry,
        &ProviderKind::Claude,
        &shared,
        ChannelId::new(CHANNEL),
    )
    .await;
    server.abort();
    let writes = writes.lock().unwrap().clone();
    assert!(recovered, "the pass delivered the answer: {writes:?}");
    assert_eq!(
        writes.first().map(|(method, _)| method),
        Some(&Method::PATCH)
    );
    assert!(
        writes.len() > 1,
        "an edit and its continuations: {writes:?}"
    );
    assert!(
        writes.iter().all(|(_, sends)| *sends == (1, 0)),
        "one Legacy send counted at every write: {writes:?}"
    );
    assert_eq!(candidate.sends(), (1, 1), "done once the pass returns");
}
