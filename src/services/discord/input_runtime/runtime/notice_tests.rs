use super::*;
use axum::{
    Json, Router,
    body::Bytes,
    http::{Method, StatusCode, Uri},
    response::IntoResponse,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

type Calls = Arc<Mutex<Vec<(Method, String, String)>>>;

struct Mock {
    http: Arc<Http>,
    calls: Calls,
    failures: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Mock {
    async fn new(channel: u64, failures: usize) -> Self {
        let calls = Calls::default();
        let fail = Arc::new(AtomicUsize::new(failures));
        let (recorded, pending) = (calls.clone(), fail.clone());
        let app = Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
            let (recorded, pending) = (recorded.clone(), pending.clone());
            async move {
                recorded.lock().unwrap().push((
                    method.clone(),
                    uri.path().into(),
                    String::from_utf8_lossy(&body).into_owned(),
                ));
                if method != Method::POST {
                    return StatusCode::NO_CONTENT.into_response();
                }
                if pending
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
                {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({"code":50013,"message":"missing permission"})),
                    )
                        .into_response();
                }
                Json(serde_json::json!({
                    "id":"63250801", "channel_id":channel.to_string(), "content":"notice",
                    "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                    "timestamp":"2026-10-12T00:00:00+00:00", "edited_timestamp":null,
                    "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                    "attachments":[],"embeds":[],"pinned":false,"type":0
                }))
                .into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            poise::serenity_prelude::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            http,
            calls,
            failures: fail,
            server,
        }
    }
}

#[tokio::test]
async fn b_failed_notice_remains_pending_http() {
    let channel = 6_325_801;
    let mock = Mock::new(channel, 1).await;
    let notice = DiscordNotice::new(mock.http.clone(), channel);
    let text = "source 63258002 remains pending".to_owned();
    assert!(
        !notice.send(text.clone()).await,
        "HTTP refusal must remain pending"
    );
    assert!(
        notice.send(text.clone()).await,
        "the pending notice is actually retried"
    );
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for (method, path, body) in calls.iter() {
        assert_eq!(method, Method::POST);
        assert!(
            path.ends_with(&format!("/channels/{channel}/messages")),
            "{path}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap()["content"],
            text
        );
    }
    assert_eq!(mock.failures.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn b_notice_reactions_use_original_source_and_http_failures() {
    let channel = 6_325_803;
    let mock = Mock::new(channel, 0).await;
    let notice = DiscordNotice::new(mock.http.clone(), channel);
    notice.set(channel, 6_325_804, '✅', true).await.unwrap();
    notice.set(channel, 6_325_804, '⏳', false).await.unwrap();
    assert!(
        notice
            .set(channel + 1, 6_325_804, '✅', true)
            .await
            .is_err()
    );
    assert!(notice.set(channel, 0, '✅', true).await.is_err());
    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "foreign and missing source produce no HTTP effects"
    );
    assert_eq!(calls[0].0, Method::PUT);
    assert_eq!(calls[1].0, Method::DELETE);
    assert!(calls.iter().all(|(_, path, _)| {
        path.contains(&format!("/channels/{channel}/messages/6325804/reactions/"))
    }));
}

#[tokio::test]
async fn b_notice_network_failure_is_not_delivery() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    );
    let notice = DiscordNotice::new(http, 6_325_805);
    assert!(!notice.send("retry after network recovery".into()).await);
    assert!(notice.set(6_325_805, 6_325_806, '✅', true).await.is_err());
}

#[tokio::test]
async fn b_notice_timeout_remains_pending() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted, receipt) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (_connection, _) = listener.accept().await.unwrap();
        accepted.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    );
    let task = tokio::spawn(async move {
        DiscordNotice::new(http, 6_325_807)
            .send("retain timeout notice".into())
            .await
    });
    receipt.await.unwrap();
    assert!(
        !task.await.unwrap(),
        "no HTTP completion is a failed delivery"
    );
    server.abort();
}
