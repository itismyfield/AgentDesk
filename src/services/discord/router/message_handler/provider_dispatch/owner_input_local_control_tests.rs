//! A Codex control that completes locally, reached as the owner's idle front: it leaves the queue
//! and settles the drive, so the next input is the next front instead of waiting behind it.
use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
use crate::services::discord::queue_io::input_ingress as ingress;
use crate::services::discord::shared_state::test_rest;
use poise::serenity_prelude as serenity;
use std::sync::Arc;

const NOTICE: &str = "provider 턴을 만들지 않았습니다";

fn original(message: u64, author: u64, text: &str) -> crate::services::discord::Intervention {
    crate::services::discord::Intervention {
        author_id: serenity::UserId::new(author),
        author_is_bot: false,
        message_id: serenity::MessageId::new(message),
        queued_generation: 1,
        source_message_ids: vec![serenity::MessageId::new(message)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: true,
        pending_uploads: Vec::new(),
    }
}

/// Discord REST on this thread; keeps the content of every message the code posts.
async fn rest(channel: u64) -> (Arc<Mutex<Vec<String>>>, test_rest::Guard) {
    use axum::{body::Bytes, http::Method, response::IntoResponse};
    let posted = Arc::new(Mutex::new(Vec::new()));
    let kept = posted.clone();
    let app = axum::Router::new().fallback(move |method: Method, body: Bytes| {
        let kept = kept.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if method == Method::POST
                && let Some(content) = body["content"].as_str()
            {
                kept.lock().unwrap().push(content.to_string());
            }
            axum::Json(json!({
                "id":"7272370000000001", "channel_id":channel.to_string(), "content":"",
                "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                "timestamp":"2026-11-08T00:00:00+00:00", "edited_timestamp":null,
                "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                "attachments":[],"embeds":[],"pinned":false,"type":0
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("owner-local-control")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (posted, test_rest::install(http))
}

#[tokio::test(flavor = "current_thread")]
async fn a_codex_local_control_front_leaves_the_queue_and_settles_its_drive() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let disabled = std::ffi::OsStr::new("disabled");
    let _mode =
        TestEnvVarGuard::set_value_after_shared_test_env_lock("ADK_INTAKE_ROUTING_MODE", disabled);
    let root = tempfile::tempdir().unwrap();
    let _root =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let config = crate::runtime_layout::config_file_path(root.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
    let _config = TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &config);
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();

    let channel = ChannelId::new(7_272_370);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    shared.settings.write().await.provider = ProviderKind::Codex;
    shared
        .http
        .cached_bot_token
        .set("owner-local-control".into())
        .unwrap();
    let session =
        super::super::super::pre_admission_control::pre_admission_control_tests::session_with;
    let mut session = session(&[], false);
    session.current_path = Some(root.path().display().to_string());
    shared.core.lock().await.sessions.insert(channel, session);
    let (posted, _rest) = rest(channel.get()).await;
    let mailbox = shared.mailbox(channel);
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("codex")
        .join(&shared.token_hash)
        .join(format!("{channel}.json"));
    mailbox
        .v2_adopt(path, shared.token_hash.clone())
        .await
        .unwrap();
    let previous = set_owner_wake_for_tests(channel, Some(Arc::new(|| {})));
    for (message, author, text) in [
        (7_272_370_001, 11, "/model"),
        (
            7_272_370_002,
            12,
            "Reply with exactly [AFTER] and nothing else.",
        ),
    ] {
        let raw = original(message, author, text);
        let queued = ingress::enqueue(&shared, &ProviderKind::Codex, channel, &raw).await;
        assert!(queued.is_some_and(|outcome| outcome.enqueued));
    }
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    assert_eq!(front.1.message_id, 7_272_370_001);

    let settled = start_idle(shared.clone(), channel, front).await;
    let next = mailbox
        .v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.front()))
        .await
        .unwrap();
    set_owner_wake_for_tests(channel, previous);
    assert_eq!(
        settled,
        Ok(()),
        "a locally completed front settles its drive"
    );
    assert_eq!(
        next.map(|(_, item)| item.message_id),
        Some(7_272_370_002),
        "the ordinary message is the next front, not behind the control"
    );
    let notices = posted.lock().unwrap().clone();
    assert_eq!(
        notices.iter().filter(|text| text.contains(NOTICE)).count(),
        1,
        "{notices:?}"
    );
}
