use super::*;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::IntoResponse;
use std::sync::Mutex as CaptureMutex;

type Calls = Arc<CaptureMutex<Vec<(Method, String, String, serde_json::Value)>>>;

struct Recorder(tokio::task::AbortHandle);

impl Drop for Recorder {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn monitoring_store_render_posts_and_edits_one_inline_provider_surface() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(72_281_701);
    let legacy = 72_281_700;
    let replacement = 72_281_702;
    let calls = Calls::default();
    let captured = calls.clone();
    let app =
        axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let captured = captured.clone();
                async move {
                    let content = serde_json::from_slice::<serde_json::Value>(&body)
                        .unwrap_or(serde_json::Value::Null);
                    let authorization = headers
                        .get("authorization")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string();
                    captured.lock().unwrap().push((
                        method.clone(),
                        uri.path().into(),
                        authorization,
                        content,
                    ));
                    if method == Method::DELETE {
                        return axum::http::StatusCode::NO_CONTENT.into_response();
                    }
                    if method == Method::PATCH
                        && uri.path().ends_with(&format!("/messages/{legacy}"))
                    {
                        return (axum::http::StatusCode::FORBIDDEN, axum::Json(serde_json::json!({
                        "code":50005, "message":"Cannot edit a message authored by another user"
                    }))).into_response();
                    }
                    axum::Json(serde_json::json!({
                    "id":replacement.to_string(), "channel_id":channel.get().to_string(),
                    "content":"inline test response",
                    "author":{"id":"1","username":"provider","discriminator":"0001","avatar":null},
                    "timestamp":"2026-10-23T00:00:00+00:00", "edited_timestamp":null,
                    "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                    "attachments":[],"embeds":[],"pinned":false,"type":0
                })).into_response()
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let http = |token| {
        Arc::new(
            serenity::HttpBuilder::new(token)
                .proxy(endpoint.clone())
                .ratelimiter_disabled(true)
                .build(),
        )
    };
    let provider_http = http("offline-provider-monitoring");
    let notify_http = http("offline-notify-monitoring");
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _recorder = Recorder(server.abort_handle());
    let registry = Arc::new(health::HealthRegistry::new());
    let shared = super::super::make_shared_data_for_tests_with_storage(None);
    shared.settings.write().await.allowed_channel_ids = vec![channel.get()];
    registry.register("claude".into(), shared).await;
    registry
        .set_utility_bot_http_for_tests(super::super::bot_role::UtilityBotRole::Notify, notify_http)
        .await;
    let _rest = super::super::shared_state::test_rest::install(provider_http);
    let monitoring = Arc::new(Mutex::new(MonitoringStore::default()));
    {
        let mut store = monitoring.lock().await;
        store.upsert(
            channel.get(),
            "resident".into(),
            "coord `events`\nnext event".into(),
        );
        store.set_rendered_msg(channel.get(), Some(legacy));
    }

    render_channel_monitoring_from_registry(&monitoring, Some(&registry), channel)
        .await
        .unwrap();
    assert_eq!(
        monitoring.lock().await.get_rendered_msg(channel.get()),
        Some(replacement)
    );
    monitoring.lock().await.upsert(
        channel.get(),
        "resident".into(),
        "latest\nsecond\nthird".into(),
    );
    render_channel_monitoring_from_registry(&monitoring, Some(&registry), channel)
        .await
        .unwrap();
    monitoring.lock().await.remove(channel.get(), "resident");
    render_channel_monitoring_from_registry(&monitoring, Some(&registry), channel)
        .await
        .unwrap();
    assert_eq!(
        monitoring.lock().await.get_rendered_msg(channel.get()),
        None
    );

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        4,
        "legacy PATCH rejection, replacement POST, provider PATCH and DELETE"
    );
    assert_eq!(calls[0].0, Method::PATCH);
    assert!(calls[0].1.ends_with(&format!("/messages/{legacy}")));
    assert_eq!(calls[1].0, Method::POST);
    assert_eq!(calls[2].0, Method::PATCH);
    assert_eq!(calls[3].0, Method::DELETE);
    assert!(calls[2].1.ends_with(&format!("/messages/{replacement}")));
    assert!(calls[3].1.ends_with(&format!("/messages/{replacement}")));
    assert!(
        calls
            .iter()
            .all(|(_, _, auth, _)| auth == "Bot offline-provider-monitoring"),
        "the registry render entrypoint must select the channel provider, never notify: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|(method, path, _, _)| *method == Method::DELETE
                && path.ends_with(&format!("/messages/{legacy}"))),
        "provider transition must leave the old notify message untouched"
    );
    for (_, _, _, body) in &calls[..3] {
        let content = body["content"].as_str().unwrap();
        assert!(
            content.starts_with("-# 👀 ") && super::super::session_notice::is_inline(content),
            "the monitoring registry entrypoint must emit one subtext line: {content}"
        );
    }
    assert!(
        calls[1].3["content"]
            .as_str()
            .unwrap()
            .contains("coord \\`events\\` (+1줄)")
    );
    assert!(
        calls[2].3["content"]
            .as_str()
            .unwrap()
            .contains("latest (+2줄)")
    );
}
