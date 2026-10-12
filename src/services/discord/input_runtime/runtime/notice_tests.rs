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

#[derive(Clone, Copy)]
enum ReactionReply {
    Success,
    NotFound { kind: u8, parent: Option<u64> },
}

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
        Self::with_reactions(channel, failures, ReactionReply::Success).await
    }

    async fn with_reactions(channel: u64, failures: usize, reactions: ReactionReply) -> Self {
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
                    match reactions {
                        ReactionReply::Success => return StatusCode::NO_CONTENT.into_response(),
                        ReactionReply::NotFound { kind, parent } => {
                            if method == Method::GET
                                && uri.path().ends_with(&format!("/channels/{channel}"))
                            {
                                return Json(serde_json::json!({
                                    "id":channel.to_string(), "type":kind,
                                    "name":"notice-source", "guild_id":"42000", "position":0,
                                    "permission_overwrites":[], "nsfw":false,
                                    "parent_id":parent.map(|id| id.to_string()),
                                    "thread_metadata":{"archived":false, "auto_archive_duration":60,
                                        "archive_timestamp":"2026-10-12T00:00:00Z", "locked":false}
                                }))
                                .into_response();
                            }
                            if method == Method::PUT
                                && parent.is_some_and(|id| {
                                    uri.path().contains(&format!("/channels/{id}/messages/"))
                                })
                            {
                                return StatusCode::NO_CONTENT.into_response();
                            }
                            return (
                                StatusCode::NOT_FOUND,
                                Json(serde_json::json!({"code":10008,"message":"Unknown Message"})),
                            )
                                .into_response();
                        }
                    }
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
    let channel = 6_326_921;
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
    let channel = 6_326_923;
    let mock = Mock::new(channel, 0).await;
    let notice = DiscordNotice::new(mock.http.clone(), channel);
    notice
        .set(channel, 940_000_006_326_924, '✅', true)
        .await
        .unwrap();
    notice
        .set(channel, 940_000_006_326_924, '⏳', false)
        .await
        .unwrap();
    assert!(
        notice
            .set(channel + 1, 940_000_006_326_924, '✅', true)
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
        path.contains(&format!(
            "/channels/{channel}/messages/940000006326924/reactions/"
        ))
    }));
}

#[tokio::test]
async fn b_notice_non_thread_reaction_failure_gets_channel_without_retry() {
    let channel = 6_326_929;
    let message = 940_000_006_326_930;
    let mock = Mock::with_reactions(
        channel,
        0,
        ReactionReply::NotFound {
            kind: 0,
            parent: None,
        },
    )
    .await;
    let notice = DiscordNotice::new(mock.http.clone(), channel);

    assert!(notice.set(channel, message, '✅', true).await.is_err());

    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "non-thread failure must not send a second PUT"
    );
    assert_eq!(calls[0].0, Method::PUT);
    assert!(calls[0].1.contains(&format!(
        "/channels/{channel}/messages/{message}/reactions/"
    )));
    assert_eq!(calls[1].0, Method::GET);
    assert!(calls[1].1.ends_with(&format!("/channels/{channel}")));
}

#[tokio::test]
async fn b_notice_thread_reaction_failure_retries_parent_channel() {
    let channel = 6_326_931;
    let parent = 6_326_932;
    let message = 940_000_006_326_933;
    let mock = Mock::with_reactions(
        channel,
        0,
        ReactionReply::NotFound {
            kind: 11,
            parent: Some(parent),
        },
    )
    .await;
    let notice = DiscordNotice::new(mock.http.clone(), channel);

    assert!(
        notice.set(channel, message, '✅', true).await.is_ok(),
        "a thread reaction must recover through its parent after PUT 404"
    );

    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        3,
        "thread failure must send exactly one parent PUT"
    );
    assert_eq!(calls[0].0, Method::PUT);
    assert!(calls[0].1.contains(&format!(
        "/channels/{channel}/messages/{message}/reactions/"
    )));
    assert_eq!(calls[1].0, Method::GET);
    assert!(calls[1].1.ends_with(&format!("/channels/{channel}")));
    assert_eq!(calls[2].0, Method::PUT);
    assert!(
        calls[2]
            .1
            .contains(&format!("/channels/{parent}/messages/{message}/reactions/"))
    );
}

#[tokio::test]
async fn b_notice_category_parent_does_not_retry_non_thread_reaction() {
    let channel = 6_326_934;
    let category = 6_326_935;
    let message = 940_000_006_326_936;
    let mock = Mock::with_reactions(
        channel,
        0,
        ReactionReply::NotFound {
            kind: 0,
            parent: Some(category),
        },
    )
    .await;
    let notice = DiscordNotice::new(mock.http.clone(), channel);

    assert!(
        notice.set(channel, message, '✅', true).await.is_err(),
        "a category parent must not recover a non-thread reaction"
    );

    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "a category parent must not cause a retry");
    assert_eq!(calls[0].0, Method::PUT);
    assert!(calls[0].1.contains(&format!(
        "/channels/{channel}/messages/{message}/reactions/"
    )));
    assert_eq!(calls[1].0, Method::GET);
    assert!(calls[1].1.ends_with(&format!("/channels/{channel}")));
    assert_eq!(
        calls
            .iter()
            .filter(|(method, path, _)| {
                *method == Method::PUT && path.contains(&format!("/channels/{category}/messages/"))
            })
            .count(),
        0,
        "no category-target PUT is allowed"
    );
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
    let notice = DiscordNotice::new(http, 6_326_925);
    assert!(!notice.send("retry after network recovery".into()).await);
    assert!(
        notice
            .set(6_326_925, 940_000_006_326_926, '✅', true)
            .await
            .is_err()
    );
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
        DiscordNotice::new(http, 6_326_927)
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
