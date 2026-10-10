//! The Codex Herdr turn from its entries: both switches at intake and dispatch, then the cold
//! start over an in-process Herdr socket, a PG row, the hook receiver and the production attach.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sqlx::PgPool;

use super::herdr::BootPorts;
use super::*;
use crate::config::{TestEnvVarGuard as Guard, TestRuntimeRootGuard};
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedExecution, HostedOwner, HostedRecord, HostedState,
};
use crate::services::claude_tui::hook_server::observation_ingress::tests::Ingress;
use crate::services::codex::herdr_turn::{self, CodexHerdrPorts, CodexHerdrTurn};
use crate::services::herdr_admission::{Admission, ForcedAdmission, force_for_test};
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunchEndpoint, HerdrLaunchHost,
};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::session_host::{EvidenceGap, HerdrTarget, RestoreResume, ServerWitness};
use crate::services::tui_o::writer::tests::codex_herdr_drive::{O_CHANNEL, ODrive};
use crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope;
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingTarget, SourceId, binding_events_since,
};

const CHANNEL: u64 = O_CHANNEL;
const TOKEN: &str = "discord_0123456789abcdef";
const LIMIT: Duration = Duration::from_secs(30);
/// The status row Codex draws at the bottom of the screen.
const STATUS: &str = "  gpt-5.5 xhigh · /fixture/workspace";
/// An empty compact composer as Codex 0.160 draws it: a bare `›` with the status row below.
const READY: &str = "earlier output\n\n›\n\n  gpt-5.5 xhigh · /fixture/workspace";
/// The boxed composer the Herdr tests showed before; the Herdr reader reads no boxed form.
const BOXED_READY: &str = "earlier output\n\
╭──────────────────────────────────────────────────────────────╮\n\
│ ▌                                                            │\n\
╰──────────────────────────────────────────────────────────────╯\n\
  Esc to interrupt   Ctrl+J newline   ⏎ send";

/// The launch's Herdr side: E7 off, one created pane, its evidence; it reports each nonce.
#[derive(Default)]
struct Launcher {
    creates: AtomicUsize,
    nonces: Mutex<Vec<String>>,
}

impl HerdrLaunchHost for Launcher {
    fn restore_resume(&self, _endpoint: &HerdrLaunchEndpoint) -> RestoreResume {
        RestoreResume::Off {
            witness: ServerWitness::for_test(1),
        }
    }

    fn create(&self, _request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        self.creates.fetch_add(1, Ordering::SeqCst);
        HerdrCreateOutcome::Created {
            pane_id: PANE.into(),
        }
    }

    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        let nonce = probe.execution_nonce.clone();
        self.nonces.lock().unwrap().push(nonce.clone());
        let stamp = |pid, seconds| crate::db::dispatched_sessions::hosted_execution::ProcessStamp {
            pid,
            start: format!("darwin:{seconds}.000000"),
        };
        Ok(ExpectedExecution {
            binding_provider: "codex".into(),
            binding_nonce: nonce,
            root: stamp(10, 1_001),
            provider_process: stamp(20, 1_002),
            provenance: "herdr_launch:ppid+env;exec=?".into(),
        })
    }
}

/// The production attach, with this test's launch host; each attach logs the pane writes and
/// whether SessionStart was sent as it began, and may first let O post what it owes.
struct Ports<'a> {
    boot: BootPorts<'a>,
    launcher: Arc<Launcher>,
    rig: &'a HerdrRig,
    started: &'a AtomicBool,
    attaches: Mutex<Vec<(usize, bool)>>,
    o: Option<&'a Mutex<Option<ODrive>>>,
    posted_before_attach: Mutex<Vec<Vec<String>>>,
}

impl CodexHerdrPorts for Ports<'_> {
    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
        Some(self.launcher.clone())
    }

    fn attach(
        &self,
        owner: &HostedOwner,
        record: &HostedExecution,
        source: &SourceId,
        target: &HerdrTarget,
    ) -> Result<bool, String> {
        let seen = (self.rig.sends().len(), self.started.load(Ordering::SeqCst));
        self.attaches.lock().unwrap().push(seen);
        if let Some(o) = self.o {
            let mut o = o.lock().unwrap();
            let drive = o.get_or_insert_with(ODrive::new);
            let posts = tokio::runtime::Handle::current().block_on(drive.step());
            self.posted_before_attach.lock().unwrap().push(posts);
        }
        CodexHerdrPorts::attach(&self.boot, owner, record, source, target)
    }

    fn confirm_bound(
        &self,
        owner: &HostedOwner,
        record: &HostedExecution,
        target: &HerdrTarget,
    ) -> Result<(), String> {
        CodexHerdrPorts::confirm_bound(&self.boot, owner, record, target)
    }
}

/// A Codex channel's canonical row on PG, its Herdr endpoint, hook receiver, Codex home and a
/// Codex CLI that advertises hooks and daemon isolation unless `help` says otherwise.
struct Fixture {
    ingress: Ingress,
    rt: tokio::runtime::Runtime,
    db: Option<crate::db::auto_queue::test_support::TestPostgresDb>,
    pool: PgPool,
    owner: HostedOwner,
    rig: HerdrRig,
    cwd: PathBuf,
    home: PathBuf,
    started: AtomicBool,
    finished: AtomicBool,
    cancel: Mutex<Arc<CancelToken>>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    _endpoint: crate::services::claude_tui::hook_server::HookEndpointGuard,
    _guards: Vec<Guard>,
    _endpoint_lock: std::sync::MutexGuard<'static, ()>,
    _root: TestRuntimeRootGuard,
}

impl Fixture {
    fn new(tag: &str, help: &str) -> Self {
        let root = TestRuntimeRootGuard::new();
        let endpoint_lock = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("codex-home");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let codex = dir.path().join("codex");
        let stub = format!(
            "#!/bin/bash\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.160.0'\n\
             elif [ \"$1 $2\" = 'resume --help' ]; then echo '{help}'\n\
             else printf 'home=%s\\n' \"$CODEX_HOME\"; printf 'arg=%s\\n' \"$@\"; env; fi\n"
        );
        std::fs::write(&codex, stub).unwrap();
        let executable = std::os::unix::fs::PermissionsExt::from_mode(0o700);
        std::fs::set_permissions(&codex, executable).unwrap();
        let guards = vec![
            Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &codex),
            Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &home),
            Guard::set_value_after_shared_test_env_lock(
                "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
                "1".as_ref(),
            ),
        ];
        let endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:9".into(),
        );
        let ingress = Ingress::new();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let owner = HostedOwner {
            provider: "codex".into(),
            discord_token_hash: TOKEN.into(),
            channel_id: CHANNEL.to_string(),
            logical_key: format!("AgentDesk-codex-p10-{tag}"),
            owner_node: NODE.into(),
            runtime_root: "/adk/runtime".into(),
        };
        let (db, pool) = rt.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id)
                 VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3)",
            )
            .bind(format!("codex/{TOKEN}/{NODE}:{}", owner.logical_key))
            .bind(TOKEN)
            .bind(CHANNEL.to_string())
            .execute(&pool)
            .await
            .unwrap();
            (db, pool)
        });
        let cwd = tempfile::tempdir().unwrap();
        Self {
            ingress,
            rt,
            db: Some(db),
            pool,
            owner,
            rig: HerdrRig::start(),
            cwd: cwd.path().canonicalize().unwrap(),
            home,
            started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            cancel: Mutex::new(Arc::new(CancelToken::new())),
            _dirs: (dir, cwd),
            _endpoint: endpoint,
            _guards: guards,
            _endpoint_lock: endpoint_lock,
            _root: root,
        }
    }

    fn admitted(tag: &str) -> Self {
        Self::new(tag, "--dangerously-bypass-hook-trust --no-daemon")
    }

    fn logical(&self) -> &str {
        &self.owner.logical_key
    }

    fn ports<'a>(&'a self, launcher: &Arc<Launcher>) -> Ports<'a> {
        Ports {
            boot: BootPorts {
                pool: &self.pool,
                channel_id: CHANNEL,
            },
            launcher: launcher.clone(),
            rig: &self.rig,
            started: &self.started,
            attaches: Mutex::default(),
            o: None,
            posted_before_attach: Mutex::default(),
        }
    }

    fn record(&self) -> HostedRecord {
        let raw: Option<Value> = self.rt.block_on(async {
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
                .bind(CHANNEL.to_string())
                .fetch_one(&self.pool)
                .await
                .unwrap()
        });
        HostedRecord::decode(raw.as_ref())
    }

    fn row(&self) -> Option<HostedState> {
        match self.record() {
            HostedRecord::Known(record) => Some(record.state),
            _ => None,
        }
    }

    fn cancel_now(&self) {
        let cancel = self.cancel.lock().unwrap().clone();
        cancel.cancelled.store(true, Ordering::SeqCst);
    }

    /// The launched pane runs its provider and shows a ready composer; `None` once the turn ended.
    fn start_provider(&self, launcher: &Launcher, ready: bool) -> Option<String> {
        if !wait_for(&self.finished, "launch evidence", || {
            !launcher.nonces.lock().unwrap().is_empty()
        }) {
            return None;
        }
        let nonce = launcher.nonces.lock().unwrap()[0].clone();
        self.rig.run_provider(&context_of(&nonce), false);
        if ready {
            self.rig.answer("pane.read", screen(READY));
        }
        Some(nonce)
    }

    /// A new rollout of `session` under the Codex home, holding only its header.
    fn rollout(&self, session: &str) -> PathBuf {
        let path = self.home.join(format!(
            "sessions/2026/10/07/rollout-2026-10-07T07-00-00-{session}.jsonl"
        ));
        let header = json!({"timestamp": "2026-10-07T07:00:00.000Z", "type": "session_meta",
            "payload": {"id": session, "session_id": session, "cwd": self.cwd,
                "originator": "codex-tui", "cli_version": "0.160.0", "source": "cli"}});
        append(&path, &[header]);
        path
    }

    /// SessionStart(startup) of `session`, relayed with the launch context at `context`.
    fn session_start(&self, context: &Path, session: &str, path: &Path) -> u16 {
        let envelope = HookBindingEnvelope::capture_from_env("codex", |name| {
            (name == "AGENTDESK_BINDING_CONTEXT").then(|| context.as_os_str().to_owned())
        });
        let payload = json!({"session_id": session, "source": "startup",
            "hook_event_name": "SessionStart", "cwd": self.cwd, "transcript_path": path});
        let uri = format!("/hooks/codex/SessionStart?session_id={}", self.logical());
        self.started.store(true, Ordering::SeqCst);
        let envelope = envelope.encode().unwrap();
        self.ingress
            .send_envelope(&uri, &payload, None, Some(&envelope))
            .0
    }

    /// The pane's provider takes the one prompt, logs its start, then answers it in full.
    fn answer(&self, nonce: &str) -> Option<(String, PathBuf)> {
        if !wait_for(&self.finished, "the prompt", || self.rig.sends().len() == 2) {
            return None;
        }
        let session = uuid::Uuid::new_v4().to_string();
        let path = self.rollout(&session);
        assert_eq!(self.session_start(&context_of(nonce), &session, &path), 202);
        append(&path, &answer_lines());
        Some((session, path))
    }

    /// Runs one turn on its own thread, as provider dispatch does on a blocking thread, while
    /// `codex` plays the pane's provider on this one.
    fn turn(
        &self,
        row: &HostedRecord,
        ports: &Ports<'_>,
        codex: impl FnOnce(),
    ) -> (Result<(), String>, Vec<StreamMessage>) {
        let (rt, rig, pool) = (&self.rt, &self.rig, &self.pool);
        let (owner, cwd) = (self.owner.clone(), self.cwd.clone());
        let endpoint = HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: KEY.into(),
            socket_addr: self.rig.socket().display().to_string(),
            herdr_session: SESSION.into(),
        };
        let finished = &self.finished;
        finished.store(false, Ordering::SeqCst);
        let log_root = crate::services::tui_prompt_dedupe::binding_events::test_root();
        let cancel = Arc::new(CancelToken::new());
        *self.cancel.lock().unwrap() = cancel.clone();
        let token = cancel.clone();
        std::thread::scope(|scope| {
            let executor = scope.spawn(move || {
                let _runtime = rt.enter();
                crate::services::tui_prompt_dedupe::binding_events::set_test_root(
                    log_root.as_deref(),
                );
                let _registry = rig.registry_on_this_thread();
                let _admission = open_admission();
                let (sender, receiver) = std::sync::mpsc::channel();
                let owner_for_strict = owner.logical_key.clone();
                let turn = CodexHerdrTurn {
                    pool,
                    owner,
                    channel_id: CHANNEL,
                    endpoint,
                    row: Some(row),
                    prompt: "질문",
                    working_dir: cwd.to_str().unwrap(),
                    system_prompt: None,
                    allowed_tools: &[],
                    model: None,
                    fast_mode: None,
                    goals: None,
                    compact_token_limit: None,
                    cancel: Some(token),
                };
                let strict_pin = if owner_for_strict.contains("strict-") {
                    use crate::services::tui_o::exact_episode::{EpisodeEvidence, EpisodeMetadata};
                    let EpisodeEvidence::Pin(mut pin) = crate::services::tui_o::exact_episode::tests::fixture().remove(0).evidence else { panic!("pin fixture"); };
                    pin.episode = uuid::Uuid::new_v4();
                    pin.owner = owner_for_strict.clone();
                    pin.source = None;
                    rt.block_on(crate::services::tui_o::exact_pg::record_episode_evidence(true, pool, &EpisodeMetadata::new(pin.episode, uuid::Uuid::new_v4(), EpisodeEvidence::Pin(pin.clone())))).unwrap();
                    Some(pin)
                } else { None };
                let _strict = strict_pin.clone().map(|pin| crate::services::tui_o::exact_submission::install(pool.clone(), pin));
                let result = crate::services::tui_o::exact_submission::dispatch(|| herdr_turn::execute(turn, ports, sender));
                if let Some(pin) = strict_pin {
                    rt.block_on(async {
                        let mut connection = pool.acquire().await.unwrap();
                        let resolution = crate::services::tui_o::exact_pg::resolve_in_tx(&mut connection, pin.episode).await.unwrap();
                        let attempted: i64 = sqlx::query_scalar("SELECT count(*) FROM public.delivery_journal_events WHERE canonical_payload->>'episode'=$1 AND canonical_payload->'evidence'->>'type'='InputAttemptBegun'").bind(pin.episode.to_string()).fetch_one(&mut *connection).await.unwrap();
                        if attempted == 0 {
                            assert_eq!(resolution.authority(), crate::services::tui_o::exact_episode::Authority::Policy);
                        } else {
                            assert_eq!(resolution.authority(), crate::services::tui_o::exact_episode::Authority::Pending);
                        }
                    });
                }
                finished.store(true, Ordering::SeqCst);
                (result, receiver.try_iter().collect())
            });
            let played = std::panic::catch_unwind(std::panic::AssertUnwindSafe(codex));
            let deadline = Instant::now() + LIMIT;
            while played.is_ok() && !finished.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let outlived = played.is_ok() && !finished.load(Ordering::SeqCst);
            if played.is_err() || outlived {
                cancel.cancelled.store(true, Ordering::SeqCst);
            }
            let (result, messages) = executor.join().unwrap();
            if let Err(panic) = played {
                std::panic::resume_unwind(panic);
            }
            let result = match outlived {
                true => Err(format!("the turn outlived its provider: {result:?}")),
                false => result,
            };
            (result, messages)
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let (db, pool) = (self.db.take().unwrap(), self.pool.clone());
        self.rt.block_on(async {
            pool.close().await;
            db.drop().await;
        });
    }
}

fn open_admission() -> ForcedAdmission {
    let stop = std::env::temp_dir().join(format!("adk-cx-none-{}", uuid::Uuid::new_v4()));
    force_for_test(Admission::new(None, Some(stop)))
}

/// `false` once the executor ended first; its result then tells why.
fn wait_for(finished: &AtomicBool, what: &str, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + LIMIT;
    while !done() {
        if finished.load(Ordering::SeqCst) {
            return false;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn append(path: &Path, lines: &[Value]) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

fn answer_lines() -> Vec<Value> {
    turn_lines("t1", "답")
}

/// Turn `turn` of the rollout: its start, one assistant message `text` and its completion.
fn turn_lines(turn: &str, text: &str) -> Vec<Value> {
    let mut lines = started_lines(turn, text);
    lines.push(
        json!({"type": "event_msg", "payload": {"type": "task_complete",
        "turn_id": turn, "last_agent_message": text}}),
    );
    lines
}

/// Turn `turn` started with one assistant message `text`, not complete yet.
fn started_lines(turn: &str, text: &str) -> Vec<Value> {
    vec![
        json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": turn}}),
        json!({"type": "response_item", "timestamp": "2026-10-07T07:00:01.000Z", "payload": {
            "type": "message", "role": "assistant", "id": format!("msg_{turn}_{text}"),
            "content": [{"type": "output_text", "text": text}]}}),
    ]
}

/// The pane's provider answers turn `turn` with `text` once `sends` pane writes arrived.
fn reply(fx: &Fixture, path: &Path, sends: usize, turn: &str, text: &str) {
    if wait_for(&fx.finished, "the follow-up", || {
        fx.rig.sends().len() == sends
    }) {
        append(path, &turn_lines(turn, text));
    }
}

fn texts(messages: &[StreamMessage]) -> Vec<&str> {
    fn text(m: &StreamMessage) -> Option<&str> {
        match m {
            StreamMessage::Text { content } => Some(content.trim()),
            _ => None,
        }
    }
    messages.iter().filter_map(text).collect()
}

fn screen(text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": PANE, "workspace_id": "w1", "tab_id": "w1:1", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 3, "truncated": false
    }})
}

fn prompt_sends() -> Vec<Value> {
    vec![
        json!({"pane_id": PANE, "text": "질문"}),
        json!({"pane_id": PANE, "keys": ["enter"]}),
    ]
}

fn context_of(nonce: &str) -> PathBuf {
    let root = crate::config::runtime_root().unwrap();
    root.join(format!("runtime/binding_contexts/codex/{nonce}.json"))
}

/// Where a launch keeps the fingerprint of its launch options.
fn options_of(fx: &Fixture) -> PathBuf {
    let files =
        crate::services::codex_tui::session::CodexTuiSessionFiles::for_tmux_session(fx.logical());
    files.launch_options_fingerprint_path
}

fn hold_of(nonce: &str) -> PathBuf {
    let root = crate::config::runtime_root().unwrap();
    root.join("runtime/herdr_input_holds").join(nonce)
}

fn pane_events(fx: &Fixture) -> usize {
    let events = binding_events_since(CHANNEL, 0).unwrap();
    let logical = fx.logical();
    events.iter().filter(|e| e.tmux_session == logical).count()
}

// T1-4/T1-6/T1-9/T1-14/T2-1: a prompt-less launch takes one prompt and logs nothing until its own
// start, then binds and ends its hold; the next turn writes once and reads only its own reply.
#[test]
fn cold_start_prompts_once_then_its_own_session_start_binds_it_and_ends_the_hold_pg() {
    let fx = Fixture::admitted("cold");
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let before_start = Mutex::new(None);
    let launched = Mutex::new(None);
    let (result, messages) = fx.turn(&HostedRecord::Legacy, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        if wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            *before_start.lock().unwrap() = Some(pane_events(&fx));
        }
        *launched.lock().unwrap() = fx.answer(&nonce).map(|(_, path)| (nonce, path));
    });
    assert_eq!(result, Ok(()));
    assert_eq!(*before_start.lock().unwrap(), Some(0), "B1: nothing logged");
    assert_eq!(*ports.attaches.lock().unwrap(), [(2, true)]);
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fx.row(), Some(HostedState::Bound));
    let (nonce, path) = launched.into_inner().unwrap().unwrap();
    assert!(
        !hold_of(&nonce).exists(),
        "the bound cold start ends its hold"
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Text { content } if content.trim() == "답")),
        "{messages:?}"
    );
    let rollout = path.canonicalize().unwrap().display().to_string();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, StreamMessage::RuntimeReady { handoff:
            crate::services::agent_protocol::RuntimeHandoff::CodexTui { rollout_path, .. } }
            if *rollout_path == rollout)),
        "{messages:?}"
    );
    let script = crate::services::tmux_common::session_temp_path(fx.logical(), "sh");
    let script = std::fs::read_to_string(script).unwrap();
    let exec = script.find("\nexec ").unwrap();
    assert!(script[..exec].ends_with(&format!(
        "unset {}",
        crate::services::herdr_launch::HERDR_PANE_ENV.join(" ")
    )));
    let ran = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .env("HERDR_ENV", "1")
        .env("CODEX_HOME", "/elsewhere")
        .output()
        .unwrap();
    let ran = String::from_utf8(ran.stdout).unwrap();
    let args: Vec<&str> = ran.lines().filter_map(|l| l.strip_prefix("arg=")).collect();
    assert!(args.contains(&"--no-daemon"), "{args:?}");
    assert!(
        args.contains(&"--dangerously-bypass-hook-trust"),
        "{args:?}"
    );
    assert!(
        args.contains(&"check_for_update_on_startup=false"),
        "{args:?}"
    );
    assert!(!args.contains(&"--") && !args.contains(&"질문"), "{args:?}");
    assert!(
        ran.contains(&format!("home={}\n", fx.home.display())),
        "{ran}"
    );
    assert!(!ran.contains("HERDR_ENV="), "{ran}");

    let (second, messages) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        reply(&fx, &path, 4, "t2", "둘째")
    });
    assert_eq!(second, Ok(()));
    assert_eq!(fx.rig.sends(), [prompt_sends(), prompt_sends()].concat());
    assert_eq!(
        texts(&messages),
        ["둘째"],
        "only the reply after the prompt"
    );
    assert!(
        !hold_of(&nonce).exists(),
        "a submitted follow-up ends its hold"
    );
    assert_eq!(fx.row(), Some(HostedState::Bound));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}

// T1-15: a cancel before any write leaves the Pending pane with no hold; the next turn creates
// nothing and sends only its own prompt once.
#[test]
fn a_cancel_before_the_first_write_leaves_the_pending_pane_for_the_next_prompt_pg() {
    let fx = Fixture::admitted("cancel");
    let launcher = Arc::new(Launcher::default());
    let (first, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        if fx.start_provider(&launcher, false).is_some() {
            fx.cancel_now();
        }
    });
    assert!(first.unwrap_err().contains("cancel"));
    assert!(fx.rig.sends().is_empty());
    assert_eq!(fx.row(), Some(HostedState::Pending));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(!hold_of(&nonce).exists());
    // Other launch options than the pane's refuse its first prompt before any read or write.
    let kept = options_of(&fx);
    let launched_with = std::fs::read_to_string(&kept).unwrap();
    std::fs::write(&kept, "other options").unwrap();
    let (changed, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(changed.unwrap_err().contains("LaunchOptionsChanged"));
    assert!(fx.rig.sends().is_empty());
    std::fs::write(&kept, launched_with).unwrap();
    let ports = fx.ports(&launcher);
    let (second, _) = fx.turn(&fx.record(), &ports, || {
        fx.rig.answer("pane.read", screen(READY));
        fx.answer(&nonce);
    });
    assert_eq!(second, Ok(()));
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fx.row(), Some(HostedState::Bound));
}

// A first write the gate refuses before any key lands releases the hold; the next prompt is
// written once on the same pane.
#[test]
fn a_first_write_refused_before_any_key_releases_the_hold_for_the_next_prompt_pg() {
    let fx = Fixture::admitted("refused-write");
    let launcher = Arc::new(Launcher::default());
    let marker = crate::services::tmux_common::session_temp_path(fx.logical(), "host_kind");
    let (first, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        if fx.start_provider(&launcher, false).is_some() {
            std::fs::remove_file(&marker).unwrap();
            fx.rig.answer("pane.read", screen(READY));
        }
    });
    assert!(first.is_err());
    assert!(fx.rig.sends().is_empty());
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(!hold_of(&nonce).exists());
    assert_eq!(fx.row(), Some(HostedState::Pending));
    std::fs::write(&marker, "herdr").unwrap();
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        fx.rig.answer("pane.read", screen(READY));
        fx.answer(&nonce);
    });
    assert_eq!(second, Ok(()));
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}

// T1-7/T1-8: with only an older execution's starts logged or relayed, nothing attaches or is
// resent, the row stays Pending and the hold keeps the next prompt out.
#[test]
fn without_its_own_session_start_the_prompt_stays_held_and_is_never_resent_pg() {
    let fx = Fixture::admitted("unstarted");
    let older = crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
        crate::services::tui_prompt_dedupe::binding_context::BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: fx.logical().into(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(fx.home.join("sessions")),
            first_prompt_digest: None,
            source_policy: None,
        },
    )
    .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(fx.logical(), "spawn_nonce");
    std::fs::write(&marker, &older.context.execution_nonce).unwrap();
    crate::services::tui_prompt_dedupe::register_provider_session(
        "codex",
        fx.logical(),
        fx.logical(),
    );
    crate::services::tui_prompt_dedupe::register_codex_herdr_placeholder(fx.logical(), CHANNEL);
    let old_session = uuid::Uuid::new_v4().to_string();
    let old_path = fx.rollout(&old_session);
    assert_eq!(fx.session_start(&older.path, &old_session, &old_path), 202);
    fx.started.store(false, Ordering::SeqCst);
    let logged = pane_events(&fx);
    assert_eq!(
        logged, 1,
        "the older execution's start is the pane's latest record"
    );

    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        if fx.start_provider(&launcher, true).is_some()
            && wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2)
        {
            let session = uuid::Uuid::new_v4().to_string();
            let path = fx.rollout(&session);
            fx.session_start(&older.path, &session, &path);
        }
    });
    let first = first.unwrap_err();
    assert!(first.contains("no session start"), "{first}");
    assert!(ports.attaches.lock().unwrap().is_empty());
    assert_eq!(pane_events(&fx), logged);
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(fx.row(), Some(HostedState::Pending));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(hold_of(&nonce).exists());
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(second.unwrap_err().contains("input held"));
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}

// N1: a first write that may have landed although its reply never came keeps the hold, though no
// write was confirmed and no Enter tried; the next turn writes nothing.
#[test]
fn an_unclear_first_write_keeps_the_hold_and_the_next_prompt_writes_nothing_pg() {
    let fx = Fixture::admitted("unclear");
    let launcher = Arc::new(Launcher::default());
    fx.rig.leave_sends_unanswered(true);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        fx.start_provider(&launcher, true);
    });
    assert!(first.is_err());
    assert_eq!(fx.rig.sends(), prompt_sends()[..1]);
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(hold_of(&nonce).exists());
    fx.rig.leave_sends_unanswered(false);
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(second.unwrap_err().contains("input held"));
    assert_eq!(fx.rig.sends(), prompt_sends()[..1]);
    assert_eq!(fx.row(), Some(HostedState::Pending));
}

// 6a: a cancel once the prompt is submitted does not end the wait for the launch's own start; the
// pane still attaches and binds, and its hold ends.
#[test]
fn a_cancel_after_the_first_write_still_binds_its_own_start_and_ends_the_hold_pg() {
    let fx = Fixture::admitted("late-cancel");
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        if wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            fx.cancel_now();
        }
        fx.answer(&nonce);
    });
    assert_eq!(result, Ok(()));
    assert_eq!(*ports.attaches.lock().unwrap(), [(2, true)]);
    assert_eq!(fx.row(), Some(HostedState::Bound));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(!hold_of(&nonce).exists());
}

// A Pending pane whose own start was logged after its turn gave up never takes a second first
// prompt, even once its hold is gone.
#[test]
fn a_pending_pane_whose_own_start_was_logged_takes_no_second_first_prompt_pg() {
    let fx = Fixture::admitted("late-start");
    let launcher = Arc::new(Launcher::default());
    let (first, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        fx.start_provider(&launcher, true);
    });
    assert!(first.unwrap_err().contains("no session start"));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    let session = uuid::Uuid::new_v4().to_string();
    let path = fx.rollout(&session);
    assert_eq!(fx.session_start(&context_of(&nonce), &session, &path), 202);
    assert_eq!(pane_events(&fx), 1);
    std::fs::remove_file(hold_of(&nonce)).unwrap();
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(
        second
            .unwrap_err()
            .contains("already took its first prompt")
    );
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}

/// A bound cold start whose hold release meets a holds directory of `mode`; the turn's result,
/// its nonce and its rollout.
fn bound_with_holds_dir(tag: &str, mode: u32) -> (Fixture, Result<(), String>, String, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::admitted(tag);
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let path = Mutex::new(None);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        if wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            let dir = hold_of(&nonce).parent().unwrap().to_owned();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        *path.lock().unwrap() = fx.answer(&nonce).map(|(_, path)| path);
    });
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    let path = path.into_inner().unwrap().unwrap();
    (fx, result, nonce, path)
}

// N2: a removal whose directory sync fails is NotDurable with the hold gone; a removal that fails
// is Kept with the hold in place, and the next prompt is held.
#[test]
fn a_release_is_not_durable_only_after_its_unlink_and_kept_only_without_it_pg() {
    use std::os::unix::fs::PermissionsExt;
    let restore = |nonce: &str| {
        let dir = hold_of(nonce).parent().unwrap().to_owned();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    let (fx, result, nonce, path) = bound_with_holds_dir("unsynced", 0o300);
    assert_eq!(result, Ok(()));
    assert!(!hold_of(&nonce).exists(), "NotDurable: the hold is gone");
    std::fs::write(hold_of("unsynced-probe"), "").unwrap();
    assert!(matches!(
        crate::services::claude::herdr_turn::release_hold("unsynced-probe"),
        crate::services::claude::herdr_turn::HoldRelease::NotDurable(_)
    ));
    assert!(!hold_of("unsynced-probe").exists());
    restore(&nonce);
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&Arc::default()), || {
        reply(&fx, &path, 4, "t2", "둘째")
    });
    assert_eq!(second, Ok(()), "the removed hold holds nothing");
    drop(fx);

    let (fx, result, nonce, _) = bound_with_holds_dir("unlinked", 0o500);
    assert_eq!(result, Ok(()));
    assert!(hold_of(&nonce).exists(), "Kept: the hold stays");
    assert!(matches!(
        crate::services::claude::herdr_turn::release_hold(&nonce),
        crate::services::claude::herdr_turn::HoldRelease::Kept(_)
    ));
    restore(&nonce);
    let sends = fx.rig.sends().len();
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&Arc::default()), || {});
    assert!(second.unwrap_err().contains("input held"));
    assert_eq!(fx.rig.sends().len(), sends);
}

// T1-5: a CLI without daemon isolation, or without the hook trust bypass, is refused before its
// Pending row: no row change, no create, no host marker.
#[test]
fn a_launch_without_daemon_isolation_or_hooks_writes_no_pending_row_pg() {
    for (help, refused) in [
        (
            "--dangerously-bypass-hook-trust",
            "DaemonIsolationUnavailable",
        ),
        ("--no-daemon", "HooksUnavailable"),
    ] {
        let fx = Fixture::new("refused", help);
        let launcher = Arc::new(Launcher::default());
        let (result, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {});
        let error = result.unwrap_err();
        assert!(error.contains(refused), "{error}");
        assert_eq!(fx.record(), HostedRecord::Legacy, "{help}");
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
        let marker = crate::services::tmux_common::session_temp_path(fx.logical(), "host_kind");
        assert!(!Path::new(&marker).exists(), "{help}");
    }
}

// T1-12/T1-13: O posts a cold start's answer once, written before the attach or with its start
// relayed and attached again; no attach adds a binding record or moves O's cursor back.
#[test]
fn o_posts_a_cold_start_once_across_its_attach_and_a_repeated_start_pg() {
    let fx = Fixture::admitted("o");
    let launcher = Arc::new(Launcher::default());
    let o = Mutex::new(None);
    let mut ports = fx.ports(&launcher);
    ports.o = Some(&o);
    let launched = Mutex::new(None);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        if let Some(nonce) = fx.start_provider(&launcher, true) {
            *launched.lock().unwrap() = fx.answer(&nonce).map(|started| (nonce, started));
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(*ports.posted_before_attach.lock().unwrap(), [vec!["답"]]);
    let (nonce, (session, path)) = launched.into_inner().unwrap().unwrap();
    let source = match binding_events_since(CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .new
        .clone()
    {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => source,
        other => panic!("no source logged: {other:?}"),
    };
    let logged = pane_events(&fx);
    let mut o = o.into_inner().unwrap().unwrap();
    let step = |o: &mut ODrive| {
        let _runtime = fx.rt.enter();
        fx.rt.block_on(o.step())
    };
    assert_eq!(step(&mut o), ["답"]);
    let captured = o.captured_through(&source).unwrap();
    assert_eq!(captured, std::fs::metadata(&path).unwrap().len());

    assert_eq!(fx.session_start(&context_of(&nonce), &session, &path), 202);
    let record = match fx.record() {
        HostedRecord::Known(record) => record,
        other => panic!("{other:?}"),
    };
    let attached = {
        let _runtime = fx.rt.enter();
        let _registry = fx.rig.registry_on_this_thread();
        let target = crate::services::session_host::herdr_endpoints()
            .target(&record)
            .unwrap();
        let boot = BootPorts {
            pool: &fx.pool,
            channel_id: CHANNEL,
        };
        CodexHerdrPorts::attach(&boot, &fx.owner, &record, &source, &target)
    };
    assert_eq!(attached, Ok(true));
    assert_eq!(
        pane_events(&fx),
        logged,
        "neither the repeat nor the attach logs a record"
    );
    assert_eq!(step(&mut o), ["답"]);
    assert_eq!(o.captured_through(&source), Some(captured));
}

const CLAUDE_CHANNEL: u64 = 1_490_141_479_707_086_938;
const CODEX_CHANNEL: u64 = 1_490_141_485_167_808_532;

fn session_key(provider: &ProviderKind) -> String {
    format!(
        "{}/{TOKEN}/mac-mini:AgentDesk-{}-dash",
        provider.as_str(),
        provider.as_str()
    )
}

// T1-1/T1-1b/T1-2/T1-10: each provider follows only its own switch at intake and dispatch, with no
// tmux or provider I/O; on, Codex is O-ready by its own kind and its `!clear` is refused.
#[tokio::test(flavor = "multi_thread")]
async fn each_provider_follows_only_its_own_herdr_switch_at_intake_and_dispatch_pg() {
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
    use crate::services::tui_prompt_dedupe::binding_context::tests;
    let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, env) = tests::fixture_after_shared_test_env_lock();
    let tmux = tests::fake_tmux(root.path());
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (store, era) = crate::services::herdr_launch::o_store_for_test(
        root.path(),
        &[CLAUDE_CHANNEL, CODEX_CHANNEL],
    );
    for channel in [CLAUDE_CHANNEL, CODEX_CHANNEL] {
        let mut seeded = store.open_channel(&era, channel).unwrap().unwrap();
        seeded.set_binding_checkpoint(3).unwrap();
    }
    let absent = root.path().join("no-stop-file");
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("mac-mini"),
        &[(CLAUDE_CHANNEL, "mac-mini"), (CODEX_CHANNEL, "mac-mini")],
    );
    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&[
        (CLAUDE_CHANNEL, ClaudeTui),
        (CODEX_CHANNEL, CodexTui),
    ]);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let _admission = force_for_test(Admission::new(Some("on".as_ref()), Some(absent)));
    let judge = |provider: ProviderKind, claude: bool, codex: bool| {
        let pool = &pool;
        async move {
            let _claude = crate::services::turn_host::force_switch_for_test(Some(claude));
            let _codex = crate::services::turn_host::force_codex_switch_for_test(Some(codex));
            let channel = match provider {
                ProviderKind::Claude => CLAUDE_CHANNEL,
                _ => CODEX_CHANNEL,
            };
            let key = session_key(&provider);
            let built = || async { Some(session_key(&provider)) };
            let intake = crate::services::turn_host::intake_refusal_before_turn(
                Some(pool),
                &provider,
                channel,
                built,
            )
            .await;
            let host =
                crate::services::turn_host::for_turn(Some(pool), &provider, channel, Some(&key))
                    .await;
            let (sender, _receiver) = std::sync::mpsc::channel();
            let turn = StreamingTurn {
                pool: None,
                provider: &provider,
                prompt: "question",
                session_id: None,
                working_dir: "/tmp",
                system_prompt: None,
                allowed_tools: &[],
                cancel: Arc::new(CancelToken::new()),
                remote_profile: None,
                tmux_session_name: Some("AgentDesk-dash"),
                teardown: None,
                host: &host,
                channel_id: channel,
                model: None,
                native_fast_mode: None,
                codex_goals: None,
                compact_percent: None,
                compact_lower_bound_tokens: 0,
                compact_token_limit: None,
                cache_ttl_minutes: None,
                dispatch_type: None,
                force_fresh: false,
            };
            let dispatched = execute(turn, sender);
            let clear = crate::services::discord::admin_host_guard::read_herdr_clear(
                Some(pool),
                &provider,
                channel,
                Some(&key),
            )
            .await
            .err();
            (intake, dispatched, clear)
        }
    };
    let entered = Err("herdr turn: no database".to_string());
    for claude in [false, true] {
        for codex in [false, true] {
            let case = format!("claude={claude} codex={codex}");
            let (intake, dispatched, _) = judge(ProviderKind::Claude, claude, codex).await;
            match claude {
                true => assert_eq!((intake, &dispatched), (None, &entered), "{case}"),
                false => {
                    let unwired = HerdrRefusal::ExecutorNotWired;
                    assert_eq!(intake, Some(unwired.clone()), "{case}");
                    assert_eq!(dispatched, Err(unwired.to_string()), "{case}");
                }
            }
            let (intake, dispatched, clear) = judge(ProviderKind::Codex, claude, codex).await;
            match codex {
                true => {
                    assert_eq!((intake, &dispatched), (None, &entered), "{case}");
                    let unsupported =
                        crate::services::discord::admin_host_guard::HERDR_CODEX_CLEAR_UNSUPPORTED;
                    assert_eq!(clear.as_deref(), Some(unsupported), "{case}");
                }
                false => {
                    let provider = "codex".to_string();
                    let refused = HerdrRefusal::ProviderUnsupported { provider };
                    assert_eq!(intake, Some(refused.clone()), "{case}");
                    assert_eq!(dispatched, Err(refused.to_string()), "{case}");
                    assert_eq!(clear, Some(refused.to_string()), "{case}");
                }
            }
        }
    }
    for driver in ["tmux.calls", "codex.calls", "claude.calls"] {
        assert!(!root.path().join(driver).exists(), "{driver} ran");
    }
    drop((tmux, env, lock));
    pool.close().await;
    db.drop().await;
}

// A Herdr turn takes its stop state before launch even with the Escape switch off, and records
// its submitted prompt there; no stop intent exists until a user stop records one.
#[test]
fn a_herdr_turn_takes_its_stop_state_with_the_escape_switch_off_pg() {
    let fx = Fixture::admitted("stopstate");
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2);
        fx.answer(&nonce);
    });
    assert_eq!(result, Ok(()));
    let token = fx.cancel.lock().unwrap().clone();
    assert!(!crate::services::provider::cancel_token_claude_interrupt::herdr_cancel_enabled());
    let state = token
        .herdr_interrupt_state()
        .expect("the turn holds its stop state");
    use crate::services::provider::cancel_token_claude_interrupt::HerdrSubmission;
    assert_eq!(
        *state.submission.lock().unwrap(),
        HerdrSubmission::Submitted
    );
    assert!(!state.user_stop.load(Ordering::SeqCst));
    assert_eq!(token.tmux_session_name().as_deref(), Some(fx.logical()));
}

#[path = "provider_dispatch_codex_herdr_followup_tests.rs"]
mod followup;

/// Codex at work, as a stop delivery reads its pane.
const WORKING: &str = "• Working (1s • esc to interrupt)";

/// A runtime whose user stops this fixture's channel: its token hash, the pane's Herdr marker.
fn stop_runtime(fx: &Fixture) -> Arc<crate::services::discord::SharedData> {
    let pool = Some(fx.pool.clone());
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(pool);
    Arc::get_mut(&mut shared).unwrap().token_hash = TOKEN.into();
    let marker = crate::services::tmux_common::session_temp_path(fx.logical(), "host_kind");
    std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, "herdr").unwrap();
    shared
}

fn mailbox_turn(
    fx: &Fixture,
    shared: &Arc<crate::services::discord::SharedData>,
    token: Arc<CancelToken>,
) {
    let channel = poise::serenity_prelude::ChannelId::new(CHANNEL);
    let (user, message) = (
        poise::serenity_prelude::UserId::new(7),
        poise::serenity_prelude::MessageId::new(CHANNEL + 1),
    );
    let start =
        crate::services::discord::mailbox_try_start_turn(shared, channel, token, user, message);
    assert!(fx.rt.block_on(start));
}

/// A `/stop` through the production user-stop entry with the Escape switch on, as its Herdr reply.
fn user_stop(fx: &Fixture, shared: &Arc<crate::services::discord::SharedData>) -> String {
    use crate::services::discord::turn_bridge::{CommandStop, begin_user_stop};
    use crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE;
    let channel = poise::serenity_prelude::ChannelId::new(CHANNEL);
    HERDR_CANCEL_OVERRIDE.set(Some(true));
    let stop = begin_user_stop(shared, &ProviderKind::Codex, channel, true, "/stop");
    let stop = fx.rt.block_on(stop);
    HERDR_CANCEL_OVERRIDE.set(None);
    match stop {
        CommandStop::Herdr(stop) => format!("{stop:?}"),
        _ => "not a Herdr stop".into(),
    }
}

fn escapes(fx: &Fixture) -> usize {
    let escape = json!({"pane_id": PANE, "keys": ["esc"]});
    fx.rig
        .sends()
        .iter()
        .filter(|send| **send == escape)
        .count()
}

/// Waits up to five seconds for `n` Escapes, then a moment more for any extra one.
fn settle_escapes(fx: &Fixture, n: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    while escapes(fx) < n.max(1) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(500));
    escapes(fx)
}

#[derive(Debug, PartialEq)]
struct LateStop {
    /// The reply to the stop taken while the turn was submitted but not bound.
    unbound: String,
    /// Escapes then, and once the pane played `own_turn`.
    escapes: (usize, usize),
    intent_kept: bool,
    result: Result<(), String>,
}

/// A cold Herdr Codex turn stopped by its user after its prompt is submitted and before its own
/// Source and attach; `own_turn` then plays the pane's provider from its new rollout.
fn stopped_before_attach(
    tag: &str,
    own_turn: impl FnOnce(&Fixture, &Arc<crate::services::discord::SharedData>, &Path),
) -> LateStop {
    let fx = Fixture::admitted(tag);
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let shared = stop_runtime(&fx);
    let seen = Mutex::new(None);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        if !wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            return;
        }
        let token = fx.cancel.lock().unwrap().clone();
        mailbox_turn(&fx, &shared, token.clone());
        fx.rig.answer("pane.read", screen(WORKING));
        let unbound = (user_stop(&fx, &shared), escapes(&fx));
        let state = token.herdr_interrupt_state().unwrap();
        let intent_kept = state.user_stop.load(Ordering::SeqCst);
        let session = uuid::Uuid::new_v4().to_string();
        let path = fx.rollout(&session);
        assert_eq!(fx.session_start(&context_of(&nonce), &session, &path), 202);
        own_turn(&fx, &shared, &path);
        *seen.lock().unwrap() = Some((unbound, intent_kept, escapes(&fx)));
    });
    let ((unbound, before), intent_kept, after) = seen.into_inner().unwrap().unwrap();
    LateStop {
        unbound,
        escapes: (before, after),
        intent_kept,
        result,
    }
}

fn pending_stop() -> String {
    "Requested(NotSent(Pending))".into()
}

// P2-2: a stop that met the turn unbound keeps its intent and sends nothing; once the turn's own
// start is read after its attach, the existing delivery sends exactly one Escape, never a second.
#[test]
fn a_stop_before_the_attach_sends_one_escape_at_the_turns_own_start_pg() {
    let again = Mutex::new(None);
    let late = stopped_before_attach("late-stop", |fx, shared, path| {
        append(path, &started_lines("t1", "답"));
        let sent = settle_escapes(fx, 1);
        // Later records and a repeated stop reach the reader and the mailbox, never the pane.
        let usage = json!({"type": "event_msg", "payload": {"type": "token_count"}});
        append(path, &[usage.clone(), usage]);
        std::thread::sleep(Duration::from_millis(400));
        *again.lock().unwrap() = Some((sent, user_stop(fx, shared)));
        let done = json!({"type": "event_msg", "payload": {"type": "task_complete",
            "turn_id": "t1", "last_agent_message": "답"}});
        append(path, &[done]);
    });
    let expected = LateStop {
        unbound: pending_stop(),
        escapes: (0, 1),
        intent_kept: true,
        result: Ok(()),
    };
    assert_eq!(late, expected);
    let (sent, repeated) = again.into_inner().unwrap().unwrap();
    assert_eq!(sent, 1, "one Escape at the turn's own start");
    assert_eq!(repeated, "AlreadyRequested");
}

// A turn whose own start arrives with its terminal and the next turn's head sends no late Escape,
// nor does one whose channel already holds another token when its own start is read.
#[test]
fn a_late_stop_sends_nothing_after_a_terminal_or_for_a_replaced_token_pg() {
    let ended = stopped_before_attach("late-ended", |fx, _, path| {
        let next =
            json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "t2"}});
        append(path, &[answer_lines(), vec![next]].concat());
        settle_escapes(fx, 0);
    });
    let replaced = stopped_before_attach("late-replaced", |fx, shared, path| {
        let channel = poise::serenity_prelude::ChannelId::new(CHANNEL);
        let finish =
            crate::services::discord::mailbox_finish_turn(shared, &ProviderKind::Codex, channel);
        fx.rt.block_on(finish);
        mailbox_turn(fx, shared, Arc::new(CancelToken::new()));
        append(path, &started_lines("t1", "답"));
        settle_escapes(fx, 0);
        let done = json!({"type": "event_msg", "payload": {"type": "task_complete",
            "turn_id": "t1", "last_agent_message": "답"}});
        append(path, &[done]);
    });
    for late in [ended, replaced] {
        let expected = LateStop {
            unbound: pending_stop(),
            escapes: (0, 0),
            intent_kept: true,
            result: Ok(()),
        };
        assert_eq!(late, expected);
    }
}

// The admission replay of a turn never runs its late stop; the live reader runs it once, at the
// turn's own start.
#[test]
fn only_the_live_reader_runs_a_late_stop_at_the_turns_own_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late-stop.jsonl");
    append(&path, &started_lines("t1", "답"));
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: TOKEN.into(),
        channel_id: CHANNEL.to_string(),
        logical_key: "AgentDesk-codex-p10-late-replay".into(),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    let token = Arc::new(CancelToken::new());
    let state = token.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = runs.clone();
    assert!(state.arm_late_stop(Box::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    })));
    let mut file = std::fs::File::open(&path).unwrap();
    let len = file.metadata().unwrap().len();
    let decoder = crate::services::codex_tui::rollout_tail::RolloutRecordDecoder::replay_herdr_turn;
    assert_eq!(decoder(&mut file, 0, len, &token), None);
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a replay runs no stop");
    let (tx, _rx) = std::sync::mpsc::channel();
    let until = Instant::now() + Duration::from_millis(500);
    let alive = move || Instant::now() < until;
    let tail = crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset;
    tail(&path, 0, Some("late"), tx, Some(token.clone()), alive).unwrap();
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the live reader runs it once"
    );
}

fn write_raw(path: &Path, bytes: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes.as_bytes()).unwrap();
}

// A late attempt that met an unfinished record sent nothing, so it runs once more when the reader
// passes the next complete record: one Escape then, never another.
#[test]
fn a_late_stop_refused_before_any_send_runs_at_the_next_complete_record_pg() {
    let seen = Mutex::new(None);
    let late = stopped_before_attach("late-retry", |fx, shared, path| {
        let usage = json!({"type": "event_msg", "payload": {"type": "token_count"}});
        let record = usage.to_string();
        let (head, rest) = record.split_at(record.len() / 2);
        let lines: String = started_lines("t1", "답")
            .iter()
            .map(|line| format!("{line}\n"))
            .collect();
        write_raw(path, &(lines + head));
        let unfinished = settle_escapes(fx, 0);
        write_raw(path, &format!("{rest}\n"));
        let sent = settle_escapes(fx, 1);
        append(path, &[usage.clone(), usage]);
        std::thread::sleep(Duration::from_millis(400));
        *seen.lock().unwrap() = Some((unfinished, sent, user_stop(fx, shared)));
        let done = json!({"type": "event_msg", "payload": {"type": "task_complete",
            "turn_id": "t1", "last_agent_message": "답"}});
        append(path, &[done]);
    });
    let expected = LateStop {
        unbound: pending_stop(),
        escapes: (0, 1),
        intent_kept: true,
        result: Ok(()),
    };
    assert_eq!(late, expected);
    let (unfinished, sent, repeated) = seen.into_inner().unwrap().unwrap();
    assert_eq!(
        (unfinished, sent),
        (0, 1),
        "none over the unfinished record, one after it"
    );
    assert_eq!(repeated, "AlreadyRequested");
}

// A stop that landed before the turn took its stop state leaves the turn to write nothing: it
// launches nothing, sends nothing to the pane and takes no stop state.
#[test]
fn a_turn_cancelled_before_its_stop_state_writes_nothing_pg() {
    let fx = Fixture::admitted("cancelled-first");
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let token = Arc::new(CancelToken::new());
    token.publish_cancel("mailbox_cancel_active_turn");
    let endpoint = HerdrLaunchEndpoint {
        execution_node: NODE.into(),
        config_key: KEY.into(),
        socket_addr: fx.rig.socket().display().to_string(),
        herdr_session: SESSION.into(),
    };
    let _runtime = fx.rt.enter();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let (sender, _receiver) = std::sync::mpsc::channel();
    let turn = CodexHerdrTurn {
        pool: &fx.pool,
        owner: fx.owner.clone(),
        channel_id: CHANNEL,
        endpoint,
        row: Some(&HostedRecord::Legacy),
        prompt: "질문",
        working_dir: fx.cwd.to_str().unwrap(),
        system_prompt: None,
        allowed_tools: &[],
        model: None,
        fast_mode: None,
        goals: None,
        compact_token_limit: None,
        cancel: Some(token.clone()),
    };
    let result = herdr_turn::execute(turn, &ports, sender);
    let cancelled = crate::services::codex_tui::input::PROMPT_READY_CANCELLED_ERROR;
    assert_eq!(result, Err(cancelled.to_string()));
    assert!(
        launcher.nonces.lock().unwrap().is_empty(),
        "nothing launched"
    );
    assert!(fx.rig.sends().is_empty(), "nothing sent to the pane");
    assert!(token.herdr_interrupt_state().is_none());
    assert!(token.tmux_session_name().is_none());
}
