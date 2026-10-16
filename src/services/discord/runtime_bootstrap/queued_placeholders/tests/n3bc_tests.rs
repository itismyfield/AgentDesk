use super::*;
use crate::services::discord::placeholder_controller::PlaceholderKey;
use std::sync::Mutex;

fn controller(shared: &SharedData, channel: ChannelId, card: MessageId) -> PlaceholderKey {
    let key = PlaceholderKey {
        provider: shared.provider.clone(),
        channel_id: channel,
        message_id: card,
    };
    shared.ui.placeholder_controller.incarnation(&key);
    assert!(queued_card_gate::boot_controller_attached(shared, &key));
    key
}

async fn response_case(status: u16, index: u64) {
    use axum::{Router, http::StatusCode, response::IntoResponse, routing::any};
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_325_710_050 + index);
    let (owner, card) = (MessageId::new(1), MessageId::new(91));
    let key = controller(&shared, channel, card);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (seen, entered, latch) = (requests.clone(), started.clone(), release.clone());
    let app = Router::new().fallback(any(
        move |method: axum::http::Method, uri: axum::http::Uri| {
            let (seen, entered, latch) = (seen.clone(), entered.clone(), latch.clone());
            async move {
                seen.lock().unwrap().push((method, uri.path().to_owned()));
                entered.notify_one();
                latch.acquire().await.unwrap().forget();
                (
                    StatusCode::from_u16(status).unwrap(),
                    axum::Json(serde_json::json!({"code": 50013, "message": "fixture failure"})),
                )
                    .into_response()
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let deleter = SerenityStalePlaceholderDeleter { http };
    let cards = [(channel, owner, card)];
    let cleanup = delete_stale_queued_placeholder_cards_with(&deleter, &shared, &cards);
    tokio::pin!(cleanup);
    tokio::select! {
        _ = started.notified() => {},
        _ = &mut cleanup => panic!("DELETE must await its mock response"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => panic!("HTTP request never started"),
    }
    assert!(queued_card_gate::boot_controller_attached(&shared, &key));
    release.add_permits(1);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), cleanup).await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    result.expect("bounded mock HTTP completion");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "exactly one real HTTP DELETE");
    assert_eq!(requests[0].0, axum::http::Method::DELETE);
    assert!(
        requests[0]
            .1
            .ends_with(&format!("/channels/{channel}/messages/{card}"))
    );
    assert!(
        !queued_card_gate::boot_controller_attached(&shared, &key),
        "a finished DELETE detaches the controller; status={status}"
    );
}

#[tokio::test]
async fn stale_cleanup_unprotected_success_and_error_keep_existing_detach_policy() {
    response_case(204, 2).await;
    response_case(403, 3).await;
}
