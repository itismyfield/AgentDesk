//! Runs the real `tmux_output_watcher_with_restore` loop over a fake `tmux`, an
//! appended transcript and a recording Discord, one child process per test.

use super::super::*;
use crate::services::discord::inflight::{load_inflight_state, save_inflight_state};
use crate::services::discord::outbound::delivery_frontier_probe::delivered_frontier_current_generation;
use crate::services::discord::outbound::delivery_record as dr;
use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "streaming_baseline_tests.rs"]
mod streaming_baseline_tests;

const CHILD: &str = "ADK_STREAMING_HARNESS_CHILD";
const CLAUDE: ProviderKind = ProviderKind::Claude;
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

// `$1` after global flags; the pane file holds busy, idle or dead.
const FAKE_TMUX: &str = r#"#!/bin/sh
while [ "${1#-}" != "$1" ]; do shift; done
state=$(cat "PANE" 2>/dev/null)
case "$1" in
  has-session) [ "$state" = dead ] && { echo "can't find session" >&2; exit 1; }; exit 0 ;;
  list-panes) [ "$state" = dead ] && echo 1 || echo 0; exit 0 ;;
  capture-pane) [ "$state" = busy ] && printf '%s\n' '⏺ Running 1 shell command…' '· Actioning… (4m 7s · esc to interrupt)'; exit 0 ;;
esac
exit 0
"#;

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LOG.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// True inside the isolated child. The parent re-runs `test` there with its own
/// runtime root, fake `tmux` on `PATH` and a zero streaming interval, then checks it passed.
pub(super) fn isolated(test: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_env_filter(
                "agentdesk::relay_flight_recorder=info,agentdesk::inflight_remove=warn,\
                 agentdesk::services::discord::tmux::tmux_watcher::cancel_handoff=info",
            )
            .with_writer(|| Capture)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("child log capture");
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let tmux = root.path().join("tmux");
    let pane = root.path().join("pane").display().to_string();
    std::fs::write(&tmux, FAKE_TMUX.replace("PANE", &pane)).unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let module = module_path!().split_once("::").unwrap().1;
    let exact = format!("{module}::streaming_baseline_tests::{test}");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("PATH", root.path())
        .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child {test}:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed; 0 failed"),
        "child {test}:\n{stdout}"
    );
    false
}

/// The value of `name=` in a captured log line, unquoted.
pub(super) fn field(line: &str, name: &str) -> Option<String> {
    let at = line.find(&format!(" {name}="))? + name.len() + 2;
    let value = line[at..].split(' ').next()?;
    Some(value.trim_matches('"').to_owned())
}

pub(super) fn user(text: &str) -> String {
    let line = serde_json::json!({"type": "user", "message": {"role": "user", "content": text}});
    format!("{line}\n")
}

pub(super) fn said(text: &str) -> String {
    let line = serde_json::json!({"type": "assistant", "message": {"role": "assistant",
        "content": [{"type": "text", "text": text}]}});
    format!("{line}\n")
}

pub(super) fn stop() -> String {
    let line = serde_json::json!({"type": "system", "subtype": "stop_hook_summary"});
    format!("{line}\n")
}

/// Discord as the watcher sees it: posts get ordinal ids, edits replace, deletes remove.
#[derive(Default)]
struct Discord {
    posts: u64,
    visible: BTreeMap<u64, String>,
    shown: Vec<String>,
    calls: usize,
}

impl Discord {
    fn respond(&mut self, channel: u64, method: Method, uri: Uri, body: Bytes) -> Response {
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let path = uri.path();
        let tail: Option<u64> = path.rsplit('/').next().and_then(|id| id.parse().ok());
        self.calls += 1;
        let id = match (method, tail) {
            (Method::POST, _) if path.ends_with("/messages") => {
                self.posts += 1;
                self.posts
            }
            (Method::PATCH, Some(id)) if path.contains("/messages/") => id,
            (Method::DELETE, Some(id)) if path.contains("/messages/") => {
                self.visible.remove(&id);
                return StatusCode::NO_CONTENT.into_response();
            }
            (Method::GET, Some(id)) if self.visible.contains_key(&id) => id,
            (Method::GET, _) if path.ends_with("/messages") => {
                return axum::Json(serde_json::json!([])).into_response();
            }
            (Method::GET, _) => {
                let missing = serde_json::json!({"message": "Unknown Message", "code": 10008});
                return (StatusCode::NOT_FOUND, axum::Json(missing)).into_response();
            }
            _ => return StatusCode::NO_CONTENT.into_response(),
        };
        if let Some(content) = payload["content"].as_str() {
            self.visible.insert(id, content.to_owned());
            self.shown.push(content.to_owned());
        }
        let content = self.visible.get(&id).cloned().unwrap_or_default();
        axum::Json(serde_json::json!({
            "id": id.to_string(), "channel_id": channel.to_string(), "content": content,
            "author": {"id": "1", "username": "t", "discriminator": "0001", "avatar": null},
            "timestamp": "2026-09-27T00:00:00+00:00", "edited_timestamp": null, "tts": false,
            "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [],
            "embeds": [], "pinned": false, "type": 0
        }))
        .into_response()
    }
}

struct Controls {
    cancel: Arc<AtomicBool>,
    resume: Arc<Mutex<Option<u64>>>,
    task: tokio::task::JoinHandle<()>,
}

/// What a scenario leaves behind, recorded as the main baseline a fix compares against.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Observed {
    /// `(data_start_offset, current_offset, route)` of every terminal relay frame.
    pub(super) frames: Vec<(u64, u64, String)>,
    /// Post ordinals of the messages that end up showing each body.
    pub(super) copies: Vec<Vec<u64>>,
    /// Bytes of bodies no message shows at the end.
    pub(super) missing_bytes: usize,
    /// Bodies some message showed that none shows at the end.
    pub(super) overwritten: usize,
    /// Durable delivered frontier range.
    pub(super) frontier: Option<(u64, u64)>,
    /// Surviving row: `(turn_start_offset, terminal_delivery_committed)`.
    pub(super) row: Option<(u64, bool)>,
}

pub(super) struct Harness {
    pub(super) shared: Arc<SharedData>,
    pub(super) channel: ChannelId,
    pub(super) tmux: String,
    pub(super) path: String,
    http: Arc<serenity::Http>,
    discord: Arc<Mutex<Discord>>,
    watcher: Option<Controls>,
}

impl Harness {
    /// A watcher-less channel over `seed`, with the pane busy.
    pub(super) async fn new(case: u64, seed: &str) -> Self {
        let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
        let channel = ChannelId::new(6_284_100 + case);
        let tmux = CLAUDE.build_tmux_session_name(&format!("i6284-harness-{case}"));
        let path = format!("{root}/transcript-{case}.jsonl");
        let generation = crate::services::tmux_common::session_temp_path(&tmux, "generation");
        std::fs::create_dir_all(std::path::Path::new(&generation).parent().unwrap()).unwrap();
        std::fs::write(&generation, "harness-generation").unwrap();
        std::fs::write(&path, seed).unwrap();
        let discord = Arc::new(Mutex::new(Discord::default()));
        let app = axum::Router::new().fallback(axum::routing::any({
            let discord = discord.clone();
            move |method: Method, uri: Uri, body: Bytes| {
                let discord = discord.clone();
                async move {
                    discord
                        .lock()
                        .unwrap()
                        .respond(channel.get(), method, uri, body)
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let harness = Self {
            shared: crate::services::discord::make_shared_data_for_tests(),
            channel,
            tmux,
            path,
            http,
            discord,
            watcher: None,
        };
        harness.pane("busy");
        harness
    }

    pub(super) fn pane(&self, state: &str) {
        let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
        std::fs::write(format!("{root}/pane"), state).unwrap();
        if state == "dead" {
            let marker = crate::services::tmux_common::session_dead_marker_path(&self.tmux);
            std::fs::write(marker, "dead").unwrap();
        }
    }

    /// Records `[start, end)` as durably delivered, as a committing relay would.
    pub(super) fn commit(&self, start: u64, end: u64) {
        let commit = dr::DeliveredCommit {
            range: (start, end),
            generation_mtime_ns: dr::current_generation_mtime_ns(&self.tmux),
            attempts: 1,
            panel_msg_id: None,
            panel_channel_id: None,
        };
        dr::write_delivered_frontier(&CLAUDE, self.channel.get(), &self.tmux, commit).unwrap();
        let coord = self.shared.tmux_relay_coord(self.channel);
        coord.confirmed_end_offset.store(end, Ordering::Release);
    }

    /// Re-acquires a watcher-owned row starting at `start`, as the watcher itself does.
    pub(super) fn row_at(&self, start: u64) -> InflightTurnState {
        let (channel, tmux, path) = (self.channel, &self.tmux, &self.path);
        let saved = reacquire_watcher_inflight_for_active_stream(
            &CLAUDE, channel, tmux, path, start, None, None, None,
        );
        assert!(saved, "a row already exists");
        self.row().unwrap()
    }

    pub(super) fn save(&self, row: &InflightTurnState) {
        save_inflight_state(row).unwrap();
    }

    pub(super) fn row(&self) -> Option<InflightTurnState> {
        load_inflight_state(&CLAUDE, self.channel.get())
    }

    /// Registers and starts a watcher at `offset`; a running one is cancelled first,
    /// which hands its turn over through cancellation custody.
    pub(super) fn spawn(&mut self, offset: u64) {
        if let Some(old) = self.watcher.take() {
            old.cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let resume = Arc::new(Mutex::new(None));
        let paused = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let delivered = Arc::new(AtomicBool::new(false));
        let beat = Arc::new(AtomicI64::new(
            crate::services::discord::tmux_watcher_now_ms(),
        ));
        self.shared.tmux_watchers.insert(
            self.channel,
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: self.tmux.clone(),
                output_path: self.path.clone(),
                paused: paused.clone(),
                resume_offset: resume.clone(),
                cancel: cancel.clone(),
                pause_epoch: epoch.clone(),
                turn_delivered: delivered.clone(),
                last_heartbeat_ts_ms: beat.clone(),
            },
        );
        let task = tokio::spawn(tmux_output_watcher_with_restore(
            self.channel,
            self.http.clone(),
            self.shared.clone(),
            self.path.clone(),
            self.tmux.clone(),
            offset,
            cancel.clone(),
            paused,
            resume.clone(),
            epoch,
            delivered,
            beat,
            None,
        ));
        self.watcher = Some(Controls {
            cancel,
            resume,
            task,
        });
    }

    /// Enqueues a resume the way a redrive nudge does; the watcher consumes it at its next poll.
    pub(super) fn resume(&self, offset: u64) {
        *self.watcher.as_ref().unwrap().resume.lock().unwrap() = Some(offset);
    }

    pub(super) fn append(&self, bytes: &[u8]) {
        use std::io::Write;
        let mut file = std::fs::File::options()
            .append(true)
            .open(&self.path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    pub(super) fn len(&self) -> u64 {
        std::fs::metadata(&self.path).unwrap().len()
    }

    pub(super) fn watcher_finished(&self) -> bool {
        self.watcher.as_ref().is_some_and(|w| w.task.is_finished())
    }

    pub(super) fn showing(&self, text: &str) -> bool {
        let discord = self.discord.lock().unwrap();
        discord.visible.values().any(|c| c.contains(text))
    }

    /// Captured lines containing `needle`, across every harness in this child.
    pub(super) fn logged(needle: &str) -> Vec<String> {
        let log = String::from_utf8_lossy(&LOG.lock().unwrap()).into_owned();
        log.lines()
            .filter(|line| line.contains(needle))
            .map(str::to_owned)
            .collect()
    }

    pub(super) fn frames(&self) -> Vec<(u64, u64, String)> {
        let tmux = format!("tmux_session={}", self.tmux);
        Self::logged("relay flight recorder")
            .iter()
            .filter(|line| line.contains(&tmux))
            .map(|line| {
                (
                    field(line, "data_start_offset").unwrap().parse().unwrap(),
                    field(line, "current_offset").unwrap().parse().unwrap(),
                    field(line, "route").unwrap(),
                )
            })
            .collect()
    }

    pub(super) async fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !done(self) {
            let row = self
                .row()
                .map(|row| (row.turn_start_offset, row.turn_nonce));
            let visible = self.discord.lock().unwrap().visible.clone();
            let frames = self.frames();
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {frames:?} {row:?} {visible:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until neither Discord nor the relay plan has moved for two seconds.
    pub(super) async fn settle(&self) {
        let mut last = (usize::MAX, usize::MAX);
        let mut quiet = 0;
        for _ in 0..150 {
            let now = (self.discord.lock().unwrap().calls, self.frames().len());
            quiet = if now == last { quiet + 1 } else { 0 };
            if quiet == 10 {
                return;
            }
            last = now;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("watcher never went quiet: frames={:?}", self.frames());
    }

    pub(super) fn observe(&self, bodies: &[&str]) -> Observed {
        let (channel, tmux, len) = (self.channel, &self.tmux, Some(self.len()));
        let frontier = delivered_frontier_current_generation(&CLAUDE, channel, tmux, len);
        let row = self.row().map(|row| {
            let start = row.turn_start_offset.unwrap_or(row.last_offset);
            (start, row.terminal_delivery_committed)
        });
        let frontier = frontier.map(|commit| commit.range);
        let mut observed = Observed {
            frames: self.frames(),
            frontier,
            row,
            ..Observed::default()
        };
        let discord = self.discord.lock().unwrap();
        for body in bodies {
            let showing = discord.visible.iter().filter(|(_, c)| c.contains(body));
            let ids: Vec<u64> = showing.map(|(id, _)| *id).collect();
            if ids.is_empty() {
                observed.missing_bytes += body.len();
                observed.overwritten += discord.shown.iter().any(|c| c.contains(body)) as usize;
            }
            observed.copies.push(ids);
        }
        observed
    }
}
