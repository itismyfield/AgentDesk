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

#[cfg(test)]
#[path = "streaming_baseline_tests.rs"]
mod streaming_baseline_tests;

#[path = "o_delegated_watcher_tests.rs"]
mod o_delegated_watcher_tests;

#[cfg(test)]
#[path = "post_stream_exit_host_tests.rs"]
mod post_stream_exit_host_tests;

const CHILD: &str = "ADK_STREAMING_HARNESS_CHILD";
const CLAUDE: ProviderKind = ProviderKind::Claude;
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

// `$1` after global flags. Builtins only: a failed read or an unknown state is recorded,
// never mistaken for another pane state. `deadpane` keeps the session with a dead pane,
// `unanswered` has no server socket, `listfail` fails only `list-panes`; kills are logged.
const FAKE_TMUX: &str = r#"#!/bin/sh
while [ "${1#-}" != "$1" ]; do shift; done
echo "$*" >> "ROOT/tmux-calls"
read -r state < "ROOT/pane" || state="unreadable"
case "$state" in busy|idle|dead|deadpane|unanswered|listfail) ;; *) echo "$* on pane '$state'" >> "ROOT/tmux-errors"; exit 97 ;; esac
case "$1" in
  has-session) [ "$state" = dead ] && { echo "can't find session" >&2; exit 1; }
    [ "$state" = unanswered ] && { echo "error connecting to ROOT/no-socket" >&2; exit 1; }; exit 0 ;;
  list-panes) [ "$state" = listfail ] && exit 1
    case "$state" in dead|deadpane) echo 1 ;; *) echo 0 ;; esac; exit 0 ;;
  kill-session) echo "$*" >> "ROOT/tmux-kills"; exit 0 ;;
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
    isolated_in("streaming_baseline_tests", test, &[])
}

/// `isolated` for a test in the sibling module `submodule`, with extra child env.
pub(super) fn isolated_in(submodule: &str, test: &str, envs: &[(&str, &str)]) -> bool {
    if std::env::var_os(CHILD).is_some() {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_env_filter(
                "agentdesk::relay_flight_recorder=info,agentdesk::inflight_remove=warn,\
                 agentdesk::services::discord::tmux::tmux_watcher::cancel_handoff=info,\
                 agentdesk::services::discord::tmux::tmux_watcher::turn_stream_collector=info,\
                 agentdesk::services::discord::host_liveness=info,\
                 agentdesk::services::discord::tmux::tmux_watcher::post_stream_exit::host_gate=debug,\
                 agentdesk::services::discord::tmux::watcher_lifecycle::watch_host=debug",
            )
            .with_writer(|| Capture)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("child log capture");
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let tmux = root.path().join("tmux");
    let dir = root.path().display().to_string();
    std::fs::write(&tmux, FAKE_TMUX.replace("ROOT", &dir)).unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let module = module_path!().split_once("::").unwrap().1;
    let exact = format!("{module}::{submodule}::{test}");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        // The empty writer list unless the case names its own; later envs win.
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .envs(envs.iter().copied())
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
    let errors = std::fs::read_to_string(root.path().join("tmux-errors")).unwrap_or_default();
    assert!(errors.is_empty(), "fake tmux in {test}:\n{errors}");
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
    post_retry: Option<Arc<PostRetry>>,
}

/// Rejects one matching POST without storing it, then holds its retry for state inspection.
struct PostRetry {
    content: &'static str,
    attempts: AtomicU64,
    release: tokio::sync::Notify,
}

impl PostRetry {
    fn new(content: &'static str) -> Arc<Self> {
        Arc::new(Self {
            content,
            attempts: AtomicU64::new(0),
            release: tokio::sync::Notify::new(),
        })
    }

    async fn intercept(&self, method: &Method, uri: &Uri, body: &Bytes) -> Option<Response> {
        let payload: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        if method != Method::POST
            || !uri.path().ends_with("/messages")
            || !payload["content"]
                .as_str()
                .is_some_and(|s| s.contains(self.content))
        {
            return None;
        }
        match self.attempts.fetch_add(1, Ordering::AcqRel) {
            0 => Some(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(
                        serde_json::json!({"message": "temporary card POST failure", "code": 0}),
                    ),
                )
                    .into_response(),
            ),
            1 => {
                self.release.notified().await;
                None
            }
            _ => None,
        }
    }
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
    beat: Arc<AtomicI64>,
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
        Self::on(case, seed, None).await
    }

    /// [`Harness::new`] on a runtime whose stored rows live in `pool`.
    pub(super) async fn on(case: u64, seed: &str, pool: Option<sqlx::PgPool>) -> Self {
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
                    let retry = discord.lock().unwrap().post_retry.clone();
                    if let Some(retry) = retry
                        && let Some(response) = retry.intercept(&method, &uri, &body).await
                    {
                        return response;
                    }
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
            shared: crate::services::discord::make_shared_data_for_tests_with_storage(pool),
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

    /// Sets the fake pane and checks the production tmux adapter now reports it.
    pub(super) fn pane(&self, state: &str) {
        // Renamed into place so a concurrent fake `tmux` never reads a torn state.
        let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
        std::fs::write(format!("{root}/pane.next"), format!("{state}\n")).unwrap();
        std::fs::rename(format!("{root}/pane.next"), format!("{root}/pane")).unwrap();
        if state == "dead" {
            let marker = crate::services::tmux_common::session_dead_marker_path(&self.tmux);
            std::fs::write(marker, "dead").unwrap();
        }
        let live = crate::services::tmux_diagnostics::tmux_session_has_live_pane(&self.tmux);
        let legacy = HostSnapshot::new(WatchHost::Legacy);
        let busy = super::super::liveness::watcher_pane_actively_streaming(&self.tmux, &legacy);
        let expected = (matches!(state, "busy" | "idle"), state == "busy");
        assert_eq!((live, busy), expected, "fake tmux {state}");
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
        self.cancel();
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
            beat.clone(),
            None,
        ));
        self.watcher = Some(Controls {
            cancel,
            resume,
            beat,
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

    /// Cancels the running watcher, if any.
    pub(super) fn cancel(&self) {
        if let Some(watcher) = self.watcher.as_ref() {
            watcher.cancel.store(true, Ordering::Release);
        }
    }

    /// `kill-session` calls the fake `tmux` received for this session.
    pub(super) fn kills(&self) -> usize {
        let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
        let kills = std::fs::read_to_string(format!("{root}/tmux-kills")).unwrap_or_default();
        kills
            .lines()
            .filter(|line| line.contains(&self.tmux))
            .count()
    }

    /// Fake `tmux` calls naming this session since the last take, oldest first.
    pub(super) fn take_tmux_calls(&self) -> Vec<String> {
        let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
        let log = format!("{root}/tmux-calls");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(log);
        calls
            .lines()
            .filter(|line| line.contains(&self.tmux))
            .map(str::to_string)
            .collect()
    }

    pub(super) fn watcher_finished(&self) -> bool {
        self.watcher.as_ref().is_some_and(|w| w.task.is_finished())
    }

    /// Waits for the watcher to end and fails on a panic or a cancelled task.
    pub(super) async fn exited(&mut self, what: &str) {
        self.until(what, Self::watcher_finished).await;
        let task = self.watcher.take().unwrap().task;
        task.await
            .unwrap_or_else(|e| panic!("watcher task {what}: {e}"));
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

    /// Captured lines for this channel containing `needle`.
    pub(super) fn events(&self, needle: &str) -> Vec<String> {
        let channel = format!(" channel_id={} ", self.channel.get());
        let lines = Self::logged(needle).into_iter();
        lines.filter(|line| line.contains(&channel)).collect()
    }

    /// Soft terminals this channel dropped with no committed owner for their bytes.
    pub(super) fn unowned_drops(&self) -> u64 {
        let rows = crate::services::observability::metrics::snapshot().into_iter();
        let rows = rows.filter(|row| row.channel_id == self.channel.get());
        rows.map(|row| row.relay_terminal_authority_denied).sum()
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
            if tokio::time::Instant::now() >= deadline {
                let row = self.row().map(|r| (r.turn_start_offset, r.turn_nonce));
                let visible = self.discord.lock().unwrap().visible.clone();
                let frames = self.frames();
                panic!("timed out on {what}: {frames:?} {row:?} {visible:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Offsets this session's watcher reported reading through: terminal frames and idle exits.
    fn read_ends(&self) -> Vec<u64> {
        let idle = format!("ready-for-input idle for {} at offset ", self.tmux);
        let idles = Self::logged(&idle).into_iter().filter_map(|line| {
            let at = line.split(&idle).nth(1)?;
            at.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
        });
        let frames = self.frames().into_iter().map(|frame| frame.1);
        frames.chain(idles).collect()
    }

    /// Waits until the watcher has read through everything appended so far and has come back
    /// to its poll loop after handling that read, then for quiet.
    pub(super) async fn drained(&self, what: &str) -> u64 {
        let end = self.len();
        self.until(what, |h| h.read_ends().iter().any(|&read| read >= end))
            .await;
        // The watcher stores its heartbeat only while polling for input, not while handling a read.
        let read_seen = crate::services::discord::tmux_watcher_now_ms();
        self.until("poll loop return", |h| h.heartbeat() > read_seen)
            .await;
        self.settle().await;
        end
    }

    pub(super) fn heartbeat(&self) -> i64 {
        self.watcher.as_ref().unwrap().beat.load(Ordering::Acquire)
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
