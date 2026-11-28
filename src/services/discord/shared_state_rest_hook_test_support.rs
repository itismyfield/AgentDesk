use super::serenity;
use std::cell::RefCell;
use std::sync::Arc;

thread_local! {
    static HTTP: RefCell<Option<Arc<serenity::http::Http>>> = const { RefCell::new(None) };
}

pub(super) fn current() -> Option<Arc<serenity::http::Http>> {
    HTTP.with(|cell| cell.borrow().clone())
}

pub(in crate::services::discord) struct Guard(Option<Arc<serenity::http::Http>>);

pub(in crate::services::discord) fn install(http: Arc<serenity::http::Http>) -> Guard {
    Guard(HTTP.with(|cell| cell.replace(Some(http))))
}

impl Drop for Guard {
    fn drop(&mut self) {
        HTTP.with(|cell| *cell.borrow_mut() = self.0.take());
    }
}

/// REST requests as `(method, message id)`; a POST records the id the mock created.
pub(in crate::services::discord) type RestLog = Arc<std::sync::Mutex<Vec<(String, u64)>>>;

/// A recording REST mock installed for this thread; created messages count up from `base`.
pub(in crate::services::discord) async fn recording_mock(
    base: u64,
    channel: u64,
) -> (RestLog, Guard) {
    use axum::{Json, Router, http::Method, http::StatusCode, http::Uri, response::IntoResponse};
    let (log, next) = (RestLog::default(), Arc::new(std::sync::Mutex::new(base)));
    let recorded = Arc::clone(&log);
    let app = Router::new().fallback(move |method: Method, uri: Uri| {
        let (log, next) = (Arc::clone(&recorded), Arc::clone(&next));
        async move {
            let tail = uri.path().rsplit('/').next().unwrap_or_default();
            let id = tail.parse::<u64>().unwrap_or_else(|_| {
                let mut next = next.lock().unwrap();
                *next += 1;
                *next - 1
            });
            log.lock().unwrap().push((method.to_string(), id));
            if method == Method::DELETE {
                return StatusCode::NO_CONTENT.into_response();
            }
            Json(serde_json::json!({
                "id": id.to_string(), "channel_id": channel.to_string(), "content": "",
                "author": {"id":"1", "username":"bot", "discriminator":"0001", "avatar":null},
                "timestamp":"2026-10-01T00:00:00+00:00", "edited_timestamp":null,
                "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                "attachments":[], "embeds":[], "pinned":false, "type":0
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (log, install(http))
}
