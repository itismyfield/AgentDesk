//! Test-only Discord REST recorder for the O-owned body cuts: records every request and its
//! `content`, answers POST/PATCH with a message, DELETE with 204 and, if asked, GET with 404.

use std::sync::{Arc, Mutex};

use poise::serenity_prelude as serenity;

use crate::services::tui_o::channel_policy::BodyCheck;

use axum::{
    Json, Router,
    body::Bytes,
    http::{Method, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct Call {
    pub(in crate::services::discord) content: Option<String>,
}

pub(in crate::services::discord) struct DiscordRecorder {
    pub(in crate::services::discord) http: Arc<serenity::Http>,
    calls: Arc<Mutex<Vec<Call>>>,
    server: tokio::task::AbortHandle,
}

impl DiscordRecorder {
    pub(in crate::services::discord) fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Every body text the code under test tried to show.
    pub(in crate::services::discord) fn contents(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| call.content)
            .collect()
    }
}

impl Drop for DiscordRecorder {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub(in crate::services::discord) async fn start(channel_id: u64) -> DiscordRecorder {
    start_with(channel_id, false).await
}

/// Also hands each request's content to `check` as it arrives.
pub(in crate::services::discord) async fn start_watching(
    channel_id: u64,
    check: BodyCheck,
    gone_messages: bool,
) -> DiscordRecorder {
    serve(channel_id, gone_messages, Some(check)).await
}

/// `gone_messages` answers every GET with 404, so a probed anchor reads as deleted.
pub(in crate::services::discord) async fn start_with(
    channel_id: u64,
    gone_messages: bool,
) -> DiscordRecorder {
    serve(channel_id, gone_messages, None).await
}

async fn serve(
    channel_id: u64,
    gone_messages: bool,
    watched: Option<BodyCheck>,
) -> DiscordRecorder {
    let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
    let next_id = Arc::new(std::sync::atomic::AtomicU64::new(900_001));
    let recorded = calls.clone();
    let app = Router::new().fallback(any(move |method: Method, uri: Uri, body: Bytes| {
        let (recorded, next_id) = (recorded.clone(), next_id.clone());
        let check = watched.clone();
        async move {
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let content = payload["content"].as_str().map(str::to_owned);
            if let (Some(check), Some(content)) = (&check, &content) {
                check.sink_request(method.as_str(), uri.path(), content);
            }
            recorded.lock().unwrap().push(Call {
                content: content.clone(),
            });
            if method == Method::DELETE {
                return (StatusCode::NO_CONTENT, String::new()).into_response();
            }
            if gone_messages && method == Method::GET {
                let body = r#"{"message":"Unknown Message","code":10008}"#;
                return (StatusCode::NOT_FOUND, body).into_response();
            }
            let path_id = uri.path().rsplit('/').next().and_then(|id| id.parse().ok());
            let id = match method {
                Method::POST => next_id.fetch_add(1, std::sync::atomic::Ordering::AcqRel),
                _ => path_id.unwrap_or(900_000),
            };
            Json(serde_json::json!({
                "id": id.to_string(), "channel_id": channel_id.to_string(),
                "content": content.unwrap_or_default(),
                "author": {"id":"1","username":"t","discriminator":"0001","avatar":null},
                "timestamp":"2026-09-27T00:00:00+00:00", "edited_timestamp":null,
                "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                "attachments":[], "embeds":[], "pinned":false, "type":0
            }))
            .into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    DiscordRecorder {
        http,
        calls,
        server: server.abort_handle(),
    }
}
