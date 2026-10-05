//! Shared fixtures for the retired-channel gate tests: a local Discord stand-in and
//! durable-record fingerprints (bytes, existence, mtime).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use poise::serenity_prelude::{Http, HttpBuilder};

use super::RetiredForTest;
use crate::services::discord::inflight::InflightTurnState;

/// Status and JSON body for one request; `None` falls back to 204 for DELETE and an
/// empty-content message otherwise.
pub(in crate::services::discord) type Answer =
    Arc<dyn Fn(&Method, &str) -> Option<(u16, serde_json::Value)> + Send + Sync>;

/// Local Discord stand-in that logs each request as `METHOD path`.
pub(in crate::services::discord) struct MockDiscord {
    pub(in crate::services::discord) http: Arc<Http>,
    calls: Arc<Mutex<Vec<String>>>,
    server: tokio::task::AbortHandle,
}

impl MockDiscord {
    pub(in crate::services::discord) async fn start() -> Self {
        Self::start_with(Arc::new(|_: &Method, _: &str| None)).await
    }

    pub(in crate::services::discord) async fn start_with(answer: Answer) -> Self {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = calls.clone();
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, _body: Bytes| {
                let (recorded, answer) = (recorded.clone(), answer.clone());
                async move {
                    recorded
                        .lock()
                        .unwrap()
                        .push(format!("{method} {}", uri.path()));
                    let (status, body) = match answer(&method, uri.path()) {
                        Some(answer) => answer,
                        None if method == Method::DELETE => {
                            return StatusCode::NO_CONTENT.into_response();
                        }
                        None => (
                            200,
                            message_json(900_001, 1, 1, "", "2026-10-02T00:00:00+00:00"),
                        ),
                    };
                    (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response()
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let http = HttpBuilder::new("test-token")
            .proxy(format!("http://127.0.0.1:{port}"))
            .ratelimiter_disabled(true)
            .build();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            http: Arc::new(http),
            calls,
            server: server.abort_handle(),
        }
    }

    /// Requests whose path names `channel_id`.
    pub(in crate::services::discord) fn calls_for(&self, channel_id: u64) -> Vec<String> {
        let needle = format!("/channels/{channel_id}/");
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.contains(&needle))
            .cloned()
            .collect()
    }
}

impl Drop for MockDiscord {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub(in crate::services::discord) fn message_json(
    id: u64,
    channel_id: u64,
    author_id: u64,
    content: &str,
    timestamp: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": id.to_string(), "channel_id": channel_id.to_string(), "content": content,
        "author": {"id": author_id.to_string(), "username": "t", "discriminator": "0001", "avatar": null},
        "timestamp": timestamp, "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "type": 0
    })
}

/// Retires a channel from inside a mock answer, modelling a mark that lands while
/// the gated pass awaits Discord.
#[derive(Clone, Default)]
pub(in crate::services::discord) struct RetireLater(Arc<Mutex<Option<RetiredForTest>>>);

impl RetireLater {
    pub(in crate::services::discord) fn retire(&self, provider: &str, channel_id: u64) {
        let mut slot = self.0.lock().unwrap();
        if slot.is_none() {
            *slot = Some(RetiredForTest::new(provider, channel_id));
        }
    }
}

/// Writes `state` as a row file without its finalizer id, so the writing loader rewrites it.
pub(in crate::services::discord) fn seed_backfill_row(state: &InflightTurnState) -> PathBuf {
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let provider =
        crate::services::provider::ProviderKind::from_str_or_unsupported(&state.provider);
    let path =
        crate::services::discord::inflight::inflight_state_path(&root, &provider, state.channel_id);
    let mut raw = serde_json::to_value(state).unwrap();
    raw.as_object_mut().unwrap().remove("finalizer_turn_id");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();
    age_file(&path, 3_600);
    path
}

/// Moves a file's mtime into the past so any rewrite is visible as an mtime change.
pub(in crate::services::discord) fn age_file(path: &Path, secs: i64) {
    let at = chrono::Utc::now().timestamp() - secs;
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(at, 0)).unwrap();
}

/// Bytes and mtime of a durable record; `None` when it does not exist.
pub(in crate::services::discord) fn fingerprint(
    path: &Path,
) -> Option<(Vec<u8>, std::time::SystemTime)> {
    let bytes = std::fs::read(path).ok()?;
    Some((bytes, std::fs::metadata(path).ok()?.modified().ok()?))
}

/// Fingerprints of every file under `dir`, keyed by path.
pub(in crate::services::discord) fn tree_fingerprint(
    dir: &Path,
) -> std::collections::BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(print) = fingerprint(&path) {
                out.insert(path, print);
            }
        }
    }
    out
}
