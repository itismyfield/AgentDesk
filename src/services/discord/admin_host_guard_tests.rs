//! Admin commands and diagnostics on every stored host case, through their real entries.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use poise::serenity_prelude::{ChannelId, Http, HttpBuilder};

use crate::services::discord::admin_host_guard::ManagedReset;
use crate::services::discord::commands::{
    SoftClearNotifyMode, build_health_report, build_status_report, clear_channel_session_state,
    reset_channel_provider_state, reset_provider_session_if_pending,
};
use crate::services::discord::host_defer_gate::tests::{
    Case, Nameless, ScriptedTmux, map_channel, postgres, with_second_bot,
};
use crate::services::discord::host_teardown_gate::test_support::{channel_key, shared_on};
use crate::services::provider::ProviderKind;
use crate::services::session_backend::{SessionHandle, insert_process_session};

/// A local stand-in for Discord and the runtime API logging each request as `METHOD path body`.
pub(crate) struct Recorder {
    pub(crate) http: Arc<Http>,
    pub(crate) port: u16,
    calls: Arc<Mutex<Vec<String>>>,
    server: tokio::task::AbortHandle,
}

/// A recorder answer for one request, ahead of its default message body.
type Answer = Arc<dyn Fn(&Method, &str) -> Option<serde_json::Value> + Send + Sync>;

impl Recorder {
    pub(crate) async fn start() -> Self {
        Self::start_with(Arc::new(|_: &Method, _: &str| None)).await
    }

    pub(crate) async fn start_with(answer: Answer) -> Self {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = calls.clone();
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, body: Bytes| {
                let (recorded, answer) = (recorded.clone(), answer.clone());
                async move {
                    let body = String::from_utf8_lossy(&body);
                    let call = format!("{method} {} {body}", uri.path());
                    recorded.lock().unwrap().push(call);
                    if method == Method::DELETE {
                        return StatusCode::NO_CONTENT.into_response();
                    }
                    if let Some(answer) = answer(&method, uri.path()) {
                        return axum::Json(answer).into_response();
                    }
                    axum::Json(serde_json::json!({
                        "id": "900001", "channel_id": "1", "content": "",
                        "author": {"id": "1", "username": "t", "discriminator": "0001", "avatar": null},
                        "timestamp": "2026-10-02T00:00:00+00:00", "edited_timestamp": null,
                        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                        "attachments": [], "embeds": [], "pinned": false, "type": 0
                    }))
                    .into_response()
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
        let server = server.abort_handle();
        let http = Arc::new(http);
        Self {
            http,
            port,
            calls,
            server,
        }
    }

    pub(crate) fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Whether this process runs the test body: the parent re-runs `test` alone in a child, as
/// the runtime API the body points at its recorder is process-global.
pub(crate) fn api_child(test: &str) -> bool {
    const CHILD: &str = "ADK_ADMIN_HOST_GUARD_API_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let exe = std::env::current_exe().expect("test binary");
    let output = std::process::Command::new(exe)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("child test run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let passed = output.status.success() && stdout.contains("1 passed");
    // The child's panic leads, so a missing test database reads as such before its summary.
    assert!(passed, "{stderr}\n{stdout}");
    false
}

async fn session_id(shared: &crate::services::discord::SharedData, channel: ChannelId) -> bool {
    let core = shared.core.lock().await;
    core.sessions[&channel].session_id.is_some()
}

/// A live process session under `name` whose kill the returned flag observes.
pub(crate) fn process(name: &str, pid: u32) -> Arc<AtomicBool> {
    let alive = Arc::new(AtomicBool::new(true));
    let handle = SessionHandle::TestProcess {
        pid,
        alive: alive.clone(),
    };
    insert_process_session(name.to_string(), handle);
    alive
}

// `/clear` and a provider reset refuse a session the host guard keeps before they change
// anything; a legacy row, or no row yet, clears and kills as in main.
#[tokio::test]
async fn clear_and_reset_refuse_a_kept_session_before_any_change_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let (provider, http) = (ProviderKind::Claude, Arc::new(Http::new("")));
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_302_387_061_000 + n as u64);
        let channel_name = format!("p4c2-clear-{n}");
        let name = provider.build_tmux_session_name(&channel_name);
        map_channel(&shared, channel, &channel_name).await;
        let mut core = shared.core.lock().await;
        core.sessions.get_mut(&channel).unwrap().session_id = Some("sid".into());
        drop(core);
        case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        let alive = process(&name, n as u32 + 64_000);
        tmux.take_calls();

        let clear = clear_channel_session_state(
            &http,
            &shared,
            &provider,
            channel,
            "/clear",
            SoftClearNotifyMode::Suppress,
        );
        let cleared = clear.await;
        if case.admitted() {
            assert!(cleared.is_ok(), "{case:?}: {cleared:?}");
            assert!(!alive.load(Ordering::SeqCst), "{case:?}: main kills");
            continue;
        }
        let error = cleared
            .expect_err("a kept session refuses the clear")
            .to_string();
        assert!(error.contains(&name), "{case:?}: {error}");
        let reset = reset_channel_provider_state(
            &http, &shared, &provider, channel, "/restart", true, false, true,
        );
        let reset = reset.await;
        let refused = matches!(&reset, ManagedReset::Refused(reason) if reason.contains(&name));
        assert!(refused, "{case:?}: {reset:?}");
        let pending = &shared.overrides.model_session_reset_pending;
        pending.insert(channel);
        reset_provider_session_if_pending(&http, &shared, &provider, channel, channel).await;
        assert!(
            pending.contains(&channel),
            "{case:?}: the pending reset is kept"
        );
        assert!(
            alive.load(Ordering::SeqCst),
            "{case:?}: the process is kept"
        );
        assert!(session_id(&shared, channel).await, "{case:?}: session kept");
        assert_eq!(
            tmux.take_calls(),
            Vec::<String>::new(),
            "{case:?}: no tmux call"
        );
        crate::services::session_backend::remove_process_session(&name);
    }
    db.drop().await;
}

// A channel holding no name is judged by its own row before `/clear` changes anything: only
// a found legacy row, or no row with nothing in flight, clears as in main.
#[tokio::test]
async fn clear_judges_a_nameless_channel_by_its_own_row_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let (shared, _registry) = with_second_bot(&pool, "p4c2-second-bot").await;
    let (provider, http) = (ProviderKind::Claude, Arc::new(Http::new("")));
    let own = shared.token_hash.clone();
    for (n, case) in Nameless::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_302_387_066_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("p4c2-nameless-{n}"));
        map_channel(&shared, channel, "unnamed").await;
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        (session.channel_name, session.session_id) = (None, Some("sid".into()));
        drop(core);
        case.seed(&pool, &own, "p4c2-second-bot", channel.get(), &name)
            .await;
        tmux.take_calls();

        let clear = clear_channel_session_state(
            &http,
            &shared,
            &provider,
            channel,
            "/clear",
            SoftClearNotifyMode::Suppress,
        );
        let cleared = clear.await;
        if matches!(case, Nameless::Legacy | Nameless::Missing) {
            assert!(cleared.is_ok(), "{case:?}: {cleared:?}");
            assert!(!session_id(&shared, channel).await, "{case:?}: main clears");
            continue;
        }
        let error = cleared.expect_err("a nameless kept channel refuses the clear");
        assert!(error.to_string().contains("채널 이름"), "{case:?}: {error}");
        assert!(session_id(&shared, channel).await, "{case:?}: session kept");
        let core = shared.core.lock().await;
        assert!(!core.sessions[&channel].cleared, "{case:?}: not cleared");
        drop(core);
        let calls = tmux.take_calls();
        assert_eq!(calls, Vec::<String>::new(), "{case:?}: no tmux call");
    }
    db.drop().await;
}

// The status and health reports show a kept session's host as unsupported without probing
// tmux by its name; a legacy row or no row reads tmux as in main.
#[tokio::test]
async fn reports_name_a_kept_host_without_probing_tmux_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_302_387_062_000 + n as u64);
        let channel_name = format!("p4c2-report-{n}");
        let name = provider.build_tmux_session_name(&channel_name);
        map_channel(&shared, channel, &channel_name).await;
        case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        tmux.take_calls();
        let status = build_status_report(&shared, &provider, channel).await;
        let health = build_health_report(&shared, &provider, channel).await;
        let expected = if case.admitted() {
            "`missing`"
        } else {
            "`unsupported-host`"
        };
        for report in [&status, &health] {
            assert!(report.contains(expected), "{case:?}: {report}");
        }
        let probes = tmux.take_calls();
        let named = probes.iter().filter(|call| call.contains(&name)).count();
        assert_eq!(named == 0, !case.admitted(), "{case:?}: {probes:?}");
    }
    db.drop().await;
}

/// The runtime API and Discord as a reset dispatch in `thread` under `parent` sees them.
fn dispatch_api(thread: ChannelId, parent: ChannelId) -> Answer {
    Arc::new(move |method: &Method, path: &str| {
        let channel = |id: ChannelId, kind: u8, parent: Option<String>| {
            serde_json::json!({
                "id": id.to_string(), "type": kind, "name": "p4c2-dispatch", "guild_id": "42000",
                "position": 0, "permission_overwrites": [], "nsfw": false, "parent_id": parent,
                "thread_metadata": {"archived": false, "auto_archive_duration": 60,
                    "archive_timestamp": "2026-10-02T00:00:00Z", "locked": false}
            })
        };
        match (method.as_str(), path) {
            ("GET", "/api/internal/card-thread") => Some(serde_json::json!({
                "dispatch_type": "implementation",
                "dispatch_context": r#"{"reset_provider_state":true}"#,
            })),
            ("GET", path) if path == format!("/api/v10/channels/{thread}") => {
                Some(channel(thread, 11, Some(parent.to_string())))
            }
            ("GET", path) if path == format!("/api/v10/channels/{parent}") => {
                Some(channel(parent, 0, None))
            }
            _ => None,
        }
    })
}

// A dispatch whose reset the host guard refuses reports it and stops before its turn: no
// mailbox claim, the session's delivery pointer stays and the input it took goes back.
#[tokio::test]
async fn a_refused_dispatch_reset_stops_before_the_turn_pg() {
    use crate::services::discord::host_teardown_gate::test_support::Stored;
    let test = "services::discord::admin_host_guard::tests::a_refused_dispatch_reset_stops_before_the_turn_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    // A regression that reaches the turn launch must not start a real provider CLI.
    let stubs = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .next()
        .unwrap();
    for cli in ["claude", "codex"] {
        std::fs::write(stubs.join(cli), "#!/bin/sh\nexit 1\n").unwrap();
        let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        std::fs::set_permissions(stubs.join(cli), mode).unwrap();
    }
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let (thread, parent) = (
        ChannelId::new(1_479_671_302_387_070_001),
        ChannelId::new(1_479_671_302_387_070_000),
    );
    let api = Recorder::start_with(dispatch_api(thread, parent)).await;
    crate::services::discord::internal_api::init(api.port, None);
    let provider = ProviderKind::Claude;
    let name = provider.build_tmux_session_name("p4c2-dispatch");
    map_channel(&shared, thread, "p4c2-dispatch").await;
    let workdir = tempfile::tempdir().unwrap();
    let mut core = shared.core.lock().await;
    let session = core.sessions.get_mut(&thread).unwrap();
    session.current_path = Some(workdir.path().display().to_string());
    (session.pending_uploads, session.cleared) = (vec!["taken-upload".into()], true);
    drop(core);
    let key = channel_key(&shared, &name);
    Case::Stored(Stored::Hosted)
        .seed(&pool, &key, &name, thread.get())
        .await;
    let pointer = "UPDATE sessions SET active_turn_delivery_outbox_id = 4242, \
                   thread_channel_id = $2 WHERE session_key = $1";
    sqlx::query(pointer)
        .bind(&key)
        .bind(thread.get().to_string())
        .execute(&pool)
        .await
        .expect("pointer");
    let alive = process(&name, 67_000);
    api.take();

    let request = crate::services::discord::IntakeRequest {
        intake_outbox_id: None,
        channel_id: thread,
        user_msg_id: poise::serenity_prelude::MessageId::new(thread.get() + 7),
        source_message_ids: Vec::new(),
        busy_followup_retry_user_msg_id: poise::serenity_prelude::MessageId::new(thread.get() + 7),
        request_owner: poise::serenity_prelude::UserId::new(4350),
        request_owner_name: "p4c2-dispatch".to_string(),
        user_text: "DISPATCH:p4c2-dispatch-1 implement it".to_string(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: crate::services::discord::TurnKind::Foreground,
        preserve_on_cancel: false,
    };
    let intake = crate::services::discord::execute_intake_turn_core;
    let preloaded = vec!["preloaded-upload".into()];
    intake(&api.http, &shared, "test-token", request, preloaded)
        .await
        .expect("intake");

    let calls = api.take();
    let reported = calls
        .iter()
        .any(|c| c.contains("dispatch reset을(를) 적용하지 않았어요"));
    assert!(reported, "the refusal is reported: {calls:?}");
    let active = shared.mailbox(thread).has_active_turn().await.unwrap();
    assert!(!active, "no turn is claimed");
    let pointer: Option<i64> = sqlx::query_scalar(
        "SELECT active_turn_delivery_outbox_id FROM sessions WHERE session_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("pointer");
    assert_eq!(pointer, Some(4242), "the delivery pointer stays");
    let core = shared.core.lock().await;
    let session = &core.sessions[&thread];
    assert_eq!(
        session.pending_uploads,
        ["taken-upload", "preloaded-upload"],
        "input returned"
    );
    assert!(session.cleared, "the clear flag returns");
    drop(core);
    assert!(alive.load(Ordering::SeqCst), "the process is kept");
    crate::services::session_backend::remove_process_session(&name);
    db.drop().await;
}

/// One stored host case: the shared stored-row cases plus two `.host_kind` markers no
/// tmux reading accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Host {
    Case(Case),
    /// The marker path is a directory, so the marker cannot be read.
    MarkerUnreadable,
    /// The marker names a host this build does not know.
    MarkerZellij,
}

impl Host {
    pub(crate) const ALL: [Self; 9] = [
        Self::Case(Case::ALL[0]),
        Self::Case(Case::ALL[1]),
        Self::Case(Case::ALL[2]),
        Self::Case(Case::ALL[3]),
        Self::Case(Case::ALL[4]),
        Self::Case(Case::ALL[5]),
        Self::Case(Case::ALL[6]),
        Self::MarkerUnreadable,
        Self::MarkerZellij,
    ];

    pub(crate) fn admitted(self) -> bool {
        matches!(self, Self::Case(case) if case.admitted())
    }

    /// Whether a tmux liveness probe of the session runs: a marker naming no tmux skips it.
    pub(crate) fn probed(self) -> bool {
        use crate::services::discord::host_teardown_gate::test_support::Stored;
        matches!(
            self,
            Self::Case(Case::Conflict)
                | Self::Case(Case::Stored(
                    Stored::Legacy | Stored::Hosted | Stored::Future | Stored::Missing
                ))
        )
    }

    pub(crate) async fn seed(self, pool: &sqlx::PgPool, key: &str, name: &str, channel: u64) {
        let marker = crate::services::tmux_common::session_temp_path(name, "host_kind");
        match self {
            Self::Case(case) => case.seed(pool, key, name, channel).await,
            Self::MarkerUnreadable => std::fs::create_dir_all(&marker).unwrap(),
            Self::MarkerZellij => {
                std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
                std::fs::write(marker, "zellij").unwrap();
            }
        }
    }
}

/// What tmux answers: every session up, a server without the session, no binary, no server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tmux {
    Live,
    Dead,
    Missing,
    NoSocket,
}

impl Tmux {
    pub(crate) const ALL: [Self; 4] = [Self::Live, Self::Dead, Self::Missing, Self::NoSocket];
}

/// PATH-first tmux logging each call and answering as the current [`Tmux`] mode says.
pub(crate) struct ModeTmux {
    dir: tempfile::TempDir,
    _env: crate::config::TestEnvVarGuard,
}

impl ModeTmux {
    /// Needs the shared test-env lock held, e.g. by a `TestRuntimeRootGuard`.
    pub(crate) fn install() -> Self {
        let dir = tempfile::TempDir::new().expect("tmux dir");
        let binary = dir.path().join("tmux");
        let mut file = std::fs::File::create(&binary).expect("mode tmux");
        writeln!(
            file,
            "#!/bin/sh\n[ \"$1\" = -u ] && shift\nd=\"$(dirname \"$0\")\"\n\
             echo \"$*\" >> \"$d/calls\"\ncase \"$(cat \"$d/mode\")\" in\n\
             missing) exit 127 ;;\n\
             nosocket) echo \"no server running on $d/socket\" >&2; exit 1 ;;\n\
             live) case \"$1\" in list-panes) echo 0 ;; capture-pane) echo pane ;; esac; exit 0 ;;\n\
             esac\necho \"can't find session: $3\" >&2; exit 1"
        )
        .expect("mode tmux body");
        drop(file);
        let permissions = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(paths).expect("join PATH");
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let env = set("PATH", std::path::Path::new(&path));
        Self { dir, _env: env }
    }

    pub(crate) fn serve(&self, mode: Tmux) {
        let mode = format!("{mode:?}").to_lowercase();
        std::fs::write(self.dir.path().join("mode"), mode).unwrap();
    }

    /// The logged calls that would kill or type into a session, clearing the log.
    pub(crate) fn take_writes(&self) -> Vec<String> {
        let log = self.dir.path().join("calls");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(log);
        let writes = ["kill-", "send-keys", "respawn", "new-session"];
        let write = |call: &&str| writes.iter().any(|verb| call.starts_with(verb));
        calls.lines().filter(write).map(str::to_string).collect()
    }
}

/// A channel with a legacy row whose live pane runs another runtime kind than the turn expects,
/// so main's runtime recreate kills it; returns its pane name.
async fn mismatched_live_pane(
    shared: &crate::services::discord::SharedData,
    pool: &sqlx::PgPool,
    channel: ChannelId,
    workdir: &std::path::Path,
) -> String {
    use crate::services::discord::host_teardown_gate::test_support::Stored;
    let label = format!("p9b1-entry-{}", channel.get() % 10_000);
    let name = ProviderKind::Claude.build_tmux_session_name(&label);
    map_channel(shared, channel, &label).await;
    let mut core = shared.core.lock().await;
    let session = core.sessions.get_mut(&channel).unwrap();
    session.current_path = Some(workdir.display().to_string());
    session.session_id = Some("sid".into());
    (session.pending_uploads, session.cleared) = (vec!["taken-upload".into()], true);
    drop(core);
    let key = channel_key(shared, &name);
    Case::Stored(Stored::Legacy)
        .seed(pool, &key, &name, channel.get())
        .await;
    let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: "/runtime/p9b1-tui.jsonl".to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: None,
        last_offset: 0,
        relay_last_offset: None,
    };
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&name, binding);
    let prompt = crate::services::tmux_common::session_temp_path(&name, "prompt");
    std::fs::write(prompt, "pending prompt").unwrap();
    name
}

/// What a runtime recreate of `name` did: killed it, forgot its binding, removed its files.
fn recreated(tmux: &ModeTmux, name: &str) -> [bool; 3] {
    let writes = tmux.take_writes();
    let killed = writes
        .iter()
        .any(|w| w.starts_with("kill-session") && w.contains(name));
    let bound = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(name);
    let prompt = crate::services::tmux_common::session_temp_path(name, "prompt");
    [
        killed,
        bound.is_none(),
        !std::path::Path::new(&prompt).exists(),
    ]
}

/// A real provider CLI must not start if a regression reaches the launch.
fn stub_provider_clis() {
    let stubs = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .next()
        .unwrap();
    for cli in ["claude", "codex"] {
        std::fs::write(stubs.join(cli), "#!/bin/sh\nexit 1\n").unwrap();
        let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        std::fs::set_permissions(stubs.join(cli), mode).unwrap();
    }
}

fn intake_request(channel: ChannelId, text: &str) -> crate::services::discord::IntakeRequest {
    let message = poise::serenity_prelude::MessageId::new(channel.get() + 7);
    crate::services::discord::IntakeRequest {
        intake_outbox_id: None,
        channel_id: channel,
        user_msg_id: message,
        source_message_ids: Vec::new(),
        busy_followup_retry_user_msg_id: message,
        request_owner: poise::serenity_prelude::UserId::new(4350),
        request_owner_name: "p9b1-entry".to_string(),
        user_text: text.to_string(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: crate::services::discord::TurnKind::Foreground,
        preserve_on_cancel: false,
    }
}

// A Herdr-configured channel's message, O writer ready or not, is refused through intake before
// the runtime recreate touches its live pane, and its input goes back; unconfigured, main recreates.
#[tokio::test]
async fn intake_refuses_a_configured_turn_before_its_runtime_recreate_pg() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::herdr_launch::{force_writer_accepts, o_store_for_test};
    let test = "services::discord::admin_host_guard::tests::intake_refuses_a_configured_turn_before_its_runtime_recreate_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ModeTmux::install();
    tmux.serve(Tmux::Live);
    stub_provider_clis();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let api = Recorder::start().await;
    let workdir = tempfile::tempdir().unwrap();
    let base = 1_479_671_302_387_072_000;
    let (unready, ready) = (base, base + 1);
    let runtime_root = crate::config::runtime_root().expect("a runtime root");
    let (store, era) = o_store_for_test(&runtime_root, &[ready]);
    let mut seeded = store.open_channel(&era, ready).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();
    let _owned =
        crate::services::tui_o::cutover::test_override::force_channels(&[(ready, ClaudeTui)]);
    let _writer = force_writer_accepts(Some(true));
    let cases = [
        (unready, Some("o_writer_not_ready")),
        (ready, Some("executor_not_wired")),
        (base + 2, None),
    ];
    for (channel, refusal) in cases {
        let channel = ChannelId::new(channel);
        let name = mismatched_live_pane(&shared, &pool, channel, workdir.path()).await;
        let _hosts = refusal.map(|_| {
            crate::config::session_hosts::force_for_test(
                Some("mac-mini"),
                &[(channel.get(), "mac-mini")],
            )
        });
        tmux.take_writes();
        api.take();

        let intake = crate::services::discord::execute_intake_turn_core;
        let request = intake_request(channel, "hello");
        let preloaded = vec!["preloaded-upload".into()];
        intake(&api.http, &shared, "test-token", request, preloaded)
            .await
            .expect("intake");

        let effects = recreated(&tmux, &name);
        let Some(refusal) = refusal else {
            assert_eq!(effects, [true; 3], "main recreates an unconfigured pane");
            continue;
        };
        assert_eq!(
            effects, [false; 3],
            "{refusal}: nothing of the pane is touched"
        );
        let core = shared.core.lock().await;
        let session = &core.sessions[&channel];
        assert_eq!(session.session_id.as_deref(), Some("sid"), "{refusal}");
        let uploads = &session.pending_uploads;
        assert_eq!(uploads, &["taken-upload", "preloaded-upload"], "{refusal}");
        assert!(session.cleared, "{refusal}: the clear flag returns");
        drop(core);
        assert!(!shared.mailbox(channel).has_active_turn().await.unwrap());
        let calls = api.take();
        let notice = format!("herdr turn refused: {refusal}");
        let reported = calls.iter().any(|c| c.contains(&notice));
        assert!(reported, "the typed refusal is reported: {calls:?}");
    }
    db.drop().await;
}

// A dispatch asking to reset a Herdr-configured channel is refused, with admission stopped,
// before the reset kills its process or tmux session or drops its provider session.
#[tokio::test]
async fn a_configured_channel_refuses_a_dispatch_reset_before_any_change_pg() {
    use crate::services::discord::host_teardown_gate::test_support::Stored;
    let test = "services::discord::admin_host_guard::tests::a_configured_channel_refuses_a_dispatch_reset_before_any_change_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    stub_provider_clis();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let (thread, parent) = (
        ChannelId::new(1_479_671_302_387_073_001),
        ChannelId::new(1_479_671_302_387_073_000),
    );
    let api = Recorder::start_with(dispatch_api(thread, parent)).await;
    crate::services::discord::internal_api::init(api.port, None);
    let name = ProviderKind::Claude.build_tmux_session_name("p4c2-dispatch");
    map_channel(&shared, thread, "p4c2-dispatch").await;
    let workdir = tempfile::tempdir().unwrap();
    let mut core = shared.core.lock().await;
    let session = core.sessions.get_mut(&thread).unwrap();
    session.current_path = Some(workdir.path().display().to_string());
    session.session_id = Some("sid".into());
    drop(core);
    let key = channel_key(&shared, &name);
    Case::Stored(Stored::Legacy)
        .seed(&pool, &key, &name, thread.get())
        .await;
    let alive = process(&name, 67_100);
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("mac-mini"),
        &[(thread.get(), "mac-mini")],
    );
    let stopped = crate::services::herdr_admission::Admission::new(Some("off".as_ref()), None);
    let _off = crate::services::herdr_admission::force_for_test(stopped);
    tmux.take_calls();
    api.take();

    let intake = crate::services::discord::execute_intake_turn_core;
    let request = intake_request(thread, "DISPATCH:p9b1-dispatch-1 implement it");
    intake(&api.http, &shared, "test-token", request, Vec::new())
        .await
        .expect("intake");

    assert!(alive.load(Ordering::SeqCst), "the process is kept");
    let kills: Vec<_> = tmux
        .take_calls()
        .into_iter()
        .filter(|c| c.starts_with("kill-"))
        .collect();
    assert_eq!(kills, Vec::<String>::new(), "no tmux kill");
    assert!(
        session_id(&shared, thread).await,
        "the provider session is kept"
    );
    assert!(!shared.mailbox(thread).has_active_turn().await.unwrap());
    let calls = api.take();
    let reported = calls
        .iter()
        .any(|c| c.contains("herdr turn refused: admission_stopped(env)"));
    assert!(reported, "the typed refusal is reported: {calls:?}");
    crate::services::session_backend::remove_process_session(&name);
    db.drop().await;
}

// A headless turn on a Herdr-configured channel fails as an invalid target before it claims the
// mailbox or recreates the live pane; unconfigured, main starts it and recreates the pane.
#[tokio::test]
async fn headless_refuses_a_configured_turn_before_its_runtime_recreate_pg() {
    use crate::services::discord::router::{
        HeadlessTurnStartError, reserve_headless_turn, start_reserved_headless_turn_with_owner,
    };
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    let test = "services::discord::admin_host_guard::tests::headless_refuses_a_configured_turn_before_its_runtime_recreate_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ModeTmux::install();
    tmux.serve(Tmux::Live);
    stub_provider_clis();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let (proxy, gateway, _server) =
        discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let workdir = tempfile::tempdir().unwrap();
    for configured in [true, false] {
        let channel = ChannelId::new(1_479_671_302_387_074_000 + u64::from(configured));
        let name = mismatched_live_pane(&shared, &pool, channel, workdir.path()).await;
        let _hosts = configured.then(|| {
            crate::config::session_hosts::force_for_test(
                Some("mac-mini"),
                &[(channel.get(), "mac-mini")],
            )
        });
        tmux.take_writes();

        let started = start_reserved_headless_turn_with_owner(
            &ctx,
            channel,
            "status",
            "p9b1-entry",
            poise::serenity_prelude::UserId::new(1),
            &shared,
            "test-token",
            None,
            None,
            None,
            None,
            Some(false),
            reserve_headless_turn(),
        )
        .await;

        let effects = recreated(&tmux, &name);
        if !configured {
            assert!(started.is_ok(), "{started:?}");
            assert_eq!(effects, [true; 3], "main recreates an unconfigured pane");
            continue;
        }
        assert_eq!(effects, [false; 3], "nothing of the pane is touched");
        assert!(!shared.mailbox(channel).has_active_turn().await.unwrap());
        assert!(
            session_id(&shared, channel).await,
            "the provider session is kept"
        );
        let refused = "herdr turn refused: o_writer_not_ready".to_string();
        assert_eq!(started, Err(HeadlessTurnStartError::InvalidTarget(refused)));
    }
    db.drop().await;
}

// A worker turn on a Herdr-configured channel its runtime holds no session for cannot hand the
// input back, so the row fails with the refusal and keeps its attachments instead of done.
#[tokio::test]
async fn a_worker_refusal_without_a_session_fails_its_row_with_its_attachments_pg() {
    use crate::services::cluster::attachment_transfer::*;
    use crate::services::cluster::intake_worker::{TickOutcome, run_intake_worker_tick};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let agent = "INSERT INTO agents (id, name, provider, discord_channel_id) \
                 VALUES ('agent-h', 'Test', 'claude', 'unused')";
    sqlx::query(agent).execute(&pool).await.unwrap();
    let channel = 1_479_671_302_387_075_000_u64;
    let identity = AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: channel.to_string(),
        user_msg_id: "75001".into(),
    };
    let entry = AttachmentEntryV1 {
        filename: "a.png".into(),
        bytes: b"png fixture".to_vec(),
        sha256: attachment_sha256_hex(b"png fixture"),
    };
    let bundle = AttachmentBundleV1 {
        version: 1,
        identity: identity.clone(),
        source_attachment_count: 1,
        entries: vec![entry],
    };
    let bundle = validate_attachment_bundle_v1(bundle, &identity).unwrap();
    let refs = vec![store::put(&pool, &bundle).await.unwrap()];
    let refs = serde_json::to_value(refs).unwrap();
    let payload = crate::db::intake_outbox::InsertPendingPayload {
        target_instance_id: "worker-1".into(),
        forwarded_by_instance_id: "leader-1".into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: refs.clone(),
        channel_id: channel.to_string(),
        user_msg_id: "75001".into(),
        request_owner_id: "100".into(),
        request_owner_name: Some("Tester".into()),
        user_text: "hello".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "standard".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "agent-h".into(),
        provider: "claude".into(),
        home_epoch: None,
    };
    let insert = crate::db::intake_outbox::insert_pending;
    let row = insert(&pool, &payload, 1, None).await.unwrap();
    let api = Recorder::start().await;
    let _rest = crate::services::discord::shared_state::test_rest::install(api.http.clone());
    let owner = crate::services::discord::health::owner_runtime_for_tests::registered("claude");
    let (_registry, owner) = owner.await;
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("mac-mini"), &[(channel, "mac-mini")]);

    let not_cancelled = || false;
    let tick = run_intake_worker_tick(&pool, &owner, "worker-1", "claude", "o", &not_cancelled);
    assert_eq!(tick.await.unwrap(), TickOutcome::Processed);

    let row: (String, Option<String>, serde_json::Value) = sqlx::query_as(
        "SELECT status::TEXT, last_error, attachment_refs FROM intake_outbox WHERE id = $1",
    )
    .bind(row)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "failed_post_accept", "not done: {row:?}");
    assert_eq!(row.2, refs, "the row keeps its attachments");
    let error = row.1.unwrap_or_default();
    assert!(error.contains("herdr turn refused"), "{error}");
    let core = owner.core.lock().await;
    assert!(
        !core.sessions.contains_key(&ChannelId::new(channel)),
        "no session started"
    );
    drop(core);
    let calls = api.take();
    let reported = calls.iter().any(|c| c.contains("herdr turn refused"));
    assert!(reported, "the typed refusal is reported: {calls:?}");
    db.drop().await;
}

/// The runtime API and Discord as a dispatch on `parent` whose card reuses an archived `thread`.
fn reuse_api(parent: ChannelId, thread: ChannelId) -> Answer {
    Arc::new(move |method: &Method, path: &str| {
        let channel = |id: ChannelId, kind: u8, parent: Option<String>| {
            serde_json::json!({
                "id": id.to_string(), "type": kind, "name": "p9b1-reuse", "guild_id": "42000",
                "position": 0, "permission_overwrites": [], "nsfw": false, "parent_id": parent,
                "thread_metadata": {"archived": true, "auto_archive_duration": 60,
                    "archive_timestamp": "2026-10-02T00:00:00Z", "locked": false}
            })
        };
        match (method.as_str(), path) {
            ("GET", "/api/internal/card-thread") => Some(serde_json::json!({
                "dispatch_type": "implementation",
                "active_thread_id": thread.to_string(),
            })),
            ("GET", path) if path == format!("/api/v10/channels/{thread}") => {
                Some(channel(thread, 11, Some(parent.to_string())))
            }
            ("GET", path) if path == format!("/api/v10/channels/{parent}") => {
                Some(channel(parent, 0, None))
            }
            _ => None,
        }
    })
}

// A dispatch on an unconfigured channel reusing a Herdr-configured thread is refused before the
// thread is unarchived, bootstrapped or mapped, with or without its session; input goes back.
#[tokio::test]
async fn a_dispatch_reusing_a_configured_thread_is_refused_before_its_bootstrap_pg() {
    let test = "services::discord::admin_host_guard::tests::a_dispatch_reusing_a_configured_thread_is_refused_before_its_bootstrap_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    stub_provider_clis();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let workdir = tempfile::tempdir().unwrap();
    for thread_has_session in [false, true] {
        let base = 1_479_671_302_387_076_000 + 10 * u64::from(thread_has_session);
        let (parent, thread) = (ChannelId::new(base), ChannelId::new(base + 1));
        let api = Recorder::start_with(reuse_api(parent, thread)).await;
        crate::services::discord::internal_api::init(api.port, None);
        map_channel(&shared, parent, "p9b1-reuse-parent").await;
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&parent).unwrap();
        session.current_path = Some(workdir.path().display().to_string());
        (session.pending_uploads, session.cleared) = (vec!["taken-upload".into()], true);
        drop(core);
        if thread_has_session {
            map_channel(&shared, thread, "p9b1-reuse-thread").await;
        }
        let before = shared.core.lock().await.sessions.get(&thread).cloned();
        let _hosts = crate::config::session_hosts::force_for_test(
            Some("mac-mini"),
            &[(thread.get(), "mac-mini")],
        );
        api.take();

        let intake = crate::services::discord::execute_intake_turn_core;
        let request = intake_request(parent, "DISPATCH:p9b1-reuse-1 implement it");
        let preloaded = vec!["preloaded-upload".into()];
        intake(&api.http, &shared, "test-token", request, preloaded)
            .await
            .expect("intake");

        let calls = api.take();
        let label = format!("thread session {thread_has_session}");
        let unarchived = calls.iter().any(|c| c.starts_with("PATCH"));
        assert!(!unarchived, "{label}: no unarchive: {calls:?}");
        let sessions = || async {
            let core = shared.core.lock().await;
            let session = core.sessions.get(&thread);
            session.map(|s| (s.current_path.clone(), s.worktree.is_some()))
        };
        let before = before.map(|s| (s.current_path, s.worktree.is_some()));
        assert_eq!(sessions().await, before, "{label}: no bootstrap");
        let mapped = shared.dispatch.thread_parents.get(&parent).map(|t| *t);
        assert_eq!(mapped, None, "{label}: no mapping");
        assert!(!shared.mailbox(thread).has_active_turn().await.unwrap());
        let core = shared.core.lock().await;
        let returned = &core.sessions[&parent].pending_uploads;
        assert_eq!(returned, &["taken-upload", "preloaded-upload"], "{label}");
        drop(core);
        let reported = calls.iter().any(|c| c.contains("herdr turn refused"));
        assert!(
            reported,
            "{label}: the typed refusal is reported: {calls:?}"
        );

        // Any other route into the thread's bootstrap creates no session for it either.
        let path = workdir.path().display().to_string();
        let bootstrap = crate::services::discord::bootstrap_thread_session;
        let fresh = bootstrap(&shared, thread, &path, &api.http, None).await;
        assert_eq!(
            sessions().await,
            before,
            "{label}: bootstrap creates nothing"
        );
        assert!(!fresh, "{label}: not bootstrapped");
    }
    db.drop().await;
}

// `/clear` on a Herdr-configured channel served by a runtime whose backend is not managed tmux
// is refused before its selector, input or durable selector clear changes.
#[tokio::test]
async fn an_unmanaged_runtime_clear_of_a_configured_channel_changes_nothing_pg() {
    let test = "services::discord::admin_host_guard::tests::an_unmanaged_runtime_clear_of_a_configured_channel_changes_nothing_pg";
    if !api_child(test) {
        return;
    }
    use crate::services::discord::host_teardown_gate::test_support::Stored;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let api = Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let (provider, http) = (ProviderKind::Gemini, Arc::new(Http::new("")));
    assert!(!provider.uses_managed_tmux_backend());
    let channel = ChannelId::new(1_479_671_302_387_077_000);
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("mac-mini"),
        &[(channel.get(), "mac-mini")],
    );
    let name = provider.build_tmux_session_name("p9b1-unmanaged");
    map_channel(&shared, channel, "p9b1-unmanaged").await;
    let mut core = shared.core.lock().await;
    let session = core.sessions.get_mut(&channel).unwrap();
    (session.session_id, session.pending_uploads) = (Some("sid".into()), vec!["kept".into()]);
    drop(core);
    Case::Stored(Stored::Legacy)
        .seed(&pool, &channel_key(&shared, &name), &name, channel.get())
        .await;
    tmux.take_calls();

    let mode = SoftClearNotifyMode::Suppress;
    let clear = clear_channel_session_state(&http, &shared, &provider, channel, "/clear", mode);
    let cleared = clear.await;
    let core = shared.core.lock().await;
    let session = &core.sessions[&channel];
    assert_eq!(session.session_id.as_deref(), Some("sid"), "selector kept");
    assert_eq!(session.pending_uploads, ["kept"], "input kept");
    drop(core);
    let calls = api.take();
    assert!(
        !calls.iter().any(|c| c.contains("clear-session-id")),
        "{calls:?}"
    );
    assert_eq!(tmux.take_calls(), Vec::<String>::new(), "no tmux call");
    let error = cleared.expect_err("a configured channel refuses the clear");
    assert!(error.to_string().contains("Herdr"), "{error}");
    db.drop().await;
}

// A clear's target is main's managed-reset verdict unless the Herdr turn switch is on for a
// configured channel; then one that cannot be judged is refused before any tmux call.
#[tokio::test]
async fn the_clear_target_keeps_mains_verdict_unless_a_configured_channel_is_switched_on_pg() {
    use super::{ResetTarget, clear_reset_target, managed_reset_refusal};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let unresolved = || -> std::future::Ready<Option<String>> {
        panic!("main's verdict resolves no session key")
    };
    let same = |main: Option<String>, target: ResetTarget| match (main, target) {
        (Some(main), ResetTarget::Refused(reason)) => main == reason,
        (None, ResetTarget::LegacyTmux) => true,
        _ => false,
    };
    for (s, switch) in [None, Some(false), Some(true)].into_iter().enumerate() {
        let _switch = crate::services::turn_host::force_switch_for_test(switch);
        for (n, case) in Case::ALL.into_iter().enumerate() {
            let channel = ChannelId::new(1_479_671_302_387_091_000 + (s * 100 + n) as u64);
            let channel_name = format!("p9b4-target-{s}-{n}");
            let name = provider.build_tmux_session_name(&channel_name);
            map_channel(&shared, channel, &channel_name).await;
            case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
                .await;
            let main = managed_reset_refusal(&shared, &provider, channel, true, false, None).await;
            let target = clear_reset_target(&shared, &provider, channel, None, unresolved).await;
            assert!(same(main, target), "{switch:?} {case:?}");
        }
    }
    let channel = ChannelId::new(1_479_671_302_387_092_000);
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("mac-mini"),
        &[(channel.get(), "mac-mini")],
    );
    map_channel(&shared, channel, "p9b4-configured").await;
    tmux.take_calls();
    for switch in [None, Some(false), Some(true)] {
        let _switch = crate::services::turn_host::force_switch_for_test(switch);
        let main = managed_reset_refusal(&shared, &provider, channel, true, false, None).await;
        let key = || async { Some("p9b4-no-row".to_string()) };
        let target = match switch {
            Some(true) => clear_reset_target(&shared, &provider, channel, None, key).await,
            _ => clear_reset_target(&shared, &provider, channel, None, unresolved).await,
        };
        match switch {
            Some(true) => assert!(
                matches!(&target, ResetTarget::Refused(reason) if reason.contains("herdr turn refused")),
                "switched on, a configured channel without its row is refused"
            ),
            _ => assert!(same(main, target), "{switch:?}"),
        }
    }
    assert_eq!(tmux.take_calls(), Vec::<String>::new(), "no tmux call");
    db.drop().await;
}
