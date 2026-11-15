//! A pending channel's long replace is held before its edit and fallback post reach Discord.

use super::*;

/// A pending adoption holds the long replace: neither its edit nor the fallback post a refused
/// edit would take reaches Discord, nothing is reported sent, and no Legacy send is counted.
#[tokio::test(flavor = "current_thread")]
async fn a_held_long_replace_sends_neither_its_edit_nor_its_fallback_post() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::test_override;
    use axum::{Json, Router, http::Method, http::StatusCode, response::IntoResponse};
    const CHANNEL: u64 = 6_737_151;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _candidates = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let candidate = test_override::with_channels(|boot| boot?.candidate(CHANNEL).cloned());
    let candidate = candidate.unwrap();
    // Every request that reached Discord; every edit is refused.
    let requests: Arc<std::sync::Mutex<Vec<Method>>> = Arc::default();
    let seen = requests.clone();
    let app = Router::new().fallback(axum::routing::any(move |method: Method| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(method.clone());
            if method == Method::PATCH {
                let refused = r#"{"message":"Missing Permissions","code":50013}"#;
                return (StatusCode::FORBIDDEN, refused).into_response();
            }
            Json(serde_json::json!({
                "id": "6737153", "channel_id": CHANNEL.to_string(), "content": "",
                "author": {"id": "1", "username": "t", "discriminator": "0001", "avatar": null},
                "timestamp": "2026-10-09T00:00:00+00:00", "edited_timestamp": null,
                "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                "attachments": [], "embeds": [], "pinned": false, "type": 0
            }))
            .into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = serenity::HttpBuilder::new("test-token")
        .proxy(format!("http://{}", listener.local_addr().unwrap()))
        .ratelimiter_disabled(true)
        .build();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared = crate::services::discord::make_shared_data_for_tests();
    // No pinned transcript identity: the failed edit's range is never read as committed.
    let recheck = EditFailureRecheck {
        provider: &ProviderKind::Claude,
        tmux_session_name: "AgentDesk-claude-s1-fallback",
        expected: None,
        range_end: 1,
    };
    let mut anchor = None;
    let sent = replace_or_post_after_edit_failure(
        &http,
        &shared,
        (ChannelId::new(CHANNEL), MessageId::new(6_737_152)),
        "the fallback body",
        recheck,
        Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
        &mut anchor,
    )
    .await;
    server.abort();
    assert!(sent.is_none(), "nothing was sent");
    assert_eq!(*requests.lock().unwrap(), Vec::<Method>::new());
    assert_eq!(candidate.sends(), (0, 0), "no Legacy send is counted");
}
