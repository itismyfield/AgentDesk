//! Shared Discord stand-in, durable-record fingerprints and private tmux fixtures.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use poise::serenity_prelude::{Http, HttpBuilder};

use crate::services::discord::inflight::InflightTurnState;

/// Status and JSON body for one request; `None` falls back to 204 for DELETE and an
/// empty-content message otherwise.
pub(in crate::services::discord) type Answer =
    Arc<dyn Fn(&Method, &str) -> Option<(u16, serde_json::Value)> + Send + Sync>;

/// Local Discord stand-in that logs each request as `METHOD path`.
pub(in crate::services::discord) struct MockDiscord {
    pub(in crate::services::discord) http: Arc<Http>,
    calls: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<String>>>,
    successful_requests: Arc<Mutex<Vec<String>>>,
    server: tokio::task::AbortHandle,
}

impl MockDiscord {
    pub(in crate::services::discord) async fn start() -> Self {
        Self::start_with(Arc::new(|_: &Method, _: &str| None)).await
    }

    pub(in crate::services::discord) async fn start_with(answer: Answer) -> Self {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let successful_requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded_successes = successful_requests.clone();
        let (recorded, recorded_requests) = (calls.clone(), requests.clone());
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, body: Bytes| {
                let (recorded, answer) = (recorded.clone(), answer.clone());
                let recorded_requests = recorded_requests.clone();
                let recorded_successes = recorded_successes.clone();
                async move {
                    recorded
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push(format!("{method} {}", uri.path()));
                    let request =
                        format!("{method} {} {}", uri.path(), String::from_utf8_lossy(&body));
                    recorded_requests
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push(request.clone());
                    let (status, body) = match answer(&method, uri.path()) {
                        Some(answer) => answer,
                        None if method == Method::DELETE => {
                            recorded_successes
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .push(request);
                            return StatusCode::NO_CONTENT.into_response();
                        }
                        None => (
                            200,
                            message_json(900_001, 1, 1, "", "2026-10-02T00:00:00+00:00"),
                        ),
                    };
                    let status = StatusCode::from_u16(status).unwrap();
                    if status.is_success() {
                        recorded_successes
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .push(request);
                    }
                    (status, axum::Json(body)).into_response()
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
            requests,
            successful_requests,
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

impl MockDiscord {
    /// Requests whose path names `channel_id`, in arrival order, each with its body.
    pub(in crate::services::discord) fn requests_for(&self, channel_id: u64) -> Vec<String> {
        let needle = format!("/channels/{channel_id}/");
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .filter(|r| r.contains(&needle))
            .cloned()
            .collect()
    }
}

impl MockDiscord {
    /// Requests answered with a successful HTTP status, each with its inbound body.
    pub(in crate::services::discord) fn successful_requests_for(
        &self,
        channel_id: u64,
    ) -> Vec<String> {
        let needle = format!("/channels/{channel_id}/");
        self.successful_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .filter(|request| request.contains(&needle))
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

/// A live private server makes a never-created session a measured negative.
#[cfg(unix)]
pub(in crate::services::discord) struct MissingSessionTmux {
    binary: PathBuf,
    socket: PathBuf,
    _env: [crate::config::TestEnvVarGuard; 3],
    _root: tempfile::TempDir,
}

#[cfg(unix)]
impl MissingSessionTmux {
    /// The caller must hold the shared environment lock until this fixture is dropped.
    pub(in crate::services::discord) fn new_after_shared_test_env_lock() -> Self {
        crate::config::test_env_lock::assert_shared_test_env_lock_held();
        use crate::config::TestEnvVarGuard as Guard;
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::Builder::new()
            .prefix("adk-missing-")
            .tempdir_in("/tmp")
            .expect("short private tmux root");
        let socket_dir = root
            .path()
            .join(format!("tmux-{}", unsafe { libc::getuid() }));
        std::fs::create_dir(&socket_dir).expect("private tmux socket directory");
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))
            .expect("private socket permissions");
        let env = [
            Guard::capture_after_shared_test_env_lock("TMUX"),
            Guard::capture_after_shared_test_env_lock("TMUX_PANE"),
            Guard::set_path_after_shared_test_env_lock("TMUX_TMPDIR", root.path()),
        ];
        unsafe {
            std::env::remove_var("TMUX");
            std::env::remove_var("TMUX_PANE");
        }
        let command = crate::services::platform::binary_resolver::runtime_command("tmux")
            .expect("real tmux is required by the measured-missing fixture");
        let fixture = Self {
            binary: PathBuf::from(command.get_program()),
            socket: socket_dir.join("default"),
            _env: env,
            _root: root,
        };
        let started = fixture
            .command()
            .args([
                "new-session",
                "-d",
                "-s",
                "fixture-keeper",
                "exec sleep 600",
            ])
            .output()
            .expect("start private tmux server");
        assert!(
            started.status.success(),
            "private tmux keeper must start: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        fixture
    }

    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.binary);
        command
            .args(["-f", "/dev/null", "-S"])
            .arg(&self.socket)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        command
    }
}

#[cfg(unix)]
impl Drop for MissingSessionTmux {
    fn drop(&mut self) {
        // The explicit socket keeps cleanup inside this fixture even during unwinding.
        let _ = self.command().arg("kill-server").output();
    }
}
