//! The legacy long replace's claimed send stays counted through the fallback post at Discord.

use super::*;

/// A legacy long replace whose edit fails keeps its claimed send counted through the fallback
/// post, and lets it go once that post is done.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_edit_keeps_the_claimed_send_counted_through_its_fallback_post() {
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
    // The channel's Legacy sends as each post reached Discord; every edit is refused.
    let posts: Arc<std::sync::Mutex<Vec<(u64, u64)>>> = Arc::default();
    let (seen, adoption) = (posts.clone(), candidate.clone());
    let app = Router::new().fallback(axum::routing::any(move |method: Method| {
        let (seen, adoption) = (seen.clone(), adoption.clone());
        async move {
            if method == Method::PATCH {
                let refused = r#"{"message":"Missing Permissions","code":50013}"#;
                return (StatusCode::FORBIDDEN, refused).into_response();
            }
            seen.lock().unwrap().push(adoption.sends());
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
    let (mut anchor, mut receipt) = (None, None);
    let sent = replace_or_post_after_edit_failure(
        &http,
        &shared,
        (ChannelId::new(CHANNEL), MessageId::new(6_737_152)),
        "the fallback body",
        recheck,
        Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
        (&mut anchor, &mut receipt),
    )
    .await;
    server.abort();
    assert!(matches!(
        sent,
        Some(Ok(WatcherDeferredReplaceOutcome::Replace(
            ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. }
        )))
    ));
    assert_eq!(
        *posts.lock().unwrap(),
        [(1, 0)],
        "counted at the fallback post"
    );
    assert_eq!(candidate.sends(), (1, 1), "done once the post is");
}
