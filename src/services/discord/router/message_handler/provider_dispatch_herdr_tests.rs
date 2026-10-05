//! The Herdr turn executor from its entries: provider dispatch behind the switch, and the executor
//! over an in-process Herdr socket, a PG row, the hook receiver and the production attach.
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
    ExpectedExecution, HostedExecution, HostedLocation, HostedOwner, HostedRecord, HostedState,
    SourceRef,
};
use crate::services::claude::herdr_turn::{self, AttachRequest, HerdrTurn, HerdrTurnPorts};
use crate::services::claude_tui::hook_server::HookEvent;
use crate::services::claude_tui::hook_server::observation_ingress::tests::{Ingress, claude, uuid};
use crate::services::claude_tui::host_input::{
    HerdrInput, InputRefusal, InputTransport, MutationGate,
};
use crate::services::herdr_admission::{Admission, ForcedAdmission, force_for_test};
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunchEndpoint, HerdrLaunchHost,
};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::session_host::{
    EvidenceGap, HerdrMutation, HostMutation, HostRefusal, RestoreResume, ServerWitness,
    herdr_endpoints,
};
use crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope;
use crate::services::turn_host::{HerdrTurnPlan, force_switch_for_test};

const CHANNEL: u64 = 1_479_671_301_387_059_301;
const TOKEN: &str = "discord_0123456789abcdef";
const NONCE: &str = "0123456789abcdef0123456789abcdef";
const LIMIT: Duration = Duration::from_secs(30);

/// The launch's Herdr side: E7 off, one created pane, its evidence; it reports each nonce.
#[derive(Default)]
struct Launcher {
    creates: AtomicUsize,
    nonces: Mutex<Vec<String>>,
    /// The provider is not confirmed: the launch keeps its pane Pending without evidence.
    unconfirmed: AtomicBool,
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
        self.nonces
            .lock()
            .unwrap()
            .push(probe.execution_nonce.clone());
        if self.unconfirmed.load(Ordering::SeqCst) {
            return Err(EvidenceGap::NoneYet);
        }
        Ok(expected(&probe.execution_nonce))
    }
}

fn expected(nonce: &str) -> ExpectedExecution {
    let stamp = |pid, seconds| crate::db::dispatched_sessions::hosted_execution::ProcessStamp {
        pid,
        start: format!("darwin:{seconds}.000000"),
    };
    ExpectedExecution {
        binding_provider: "claude".into(),
        binding_nonce: nonce.into(),
        root: stamp(10, 1_001),
        provider_process: stamp(20, 1_002),
        provenance: "herdr_launch:ppid+env;exec=?".into(),
    }
}

/// The production attach and reconcile, with this test's launch host and hook receiver.
struct Ports<'a> {
    boot: BootPorts<'a>,
    launcher: Arc<Launcher>,
    hooks: Mutex<Option<tokio::sync::broadcast::Receiver<HookEvent>>>,
    rig: &'a HerdrRig,
    started: &'a AtomicBool,
    /// Pane writes and whether SessionStart was sent, as each attach began.
    attaches: Mutex<Vec<(usize, bool)>>,
    /// The turn is cancelled once its attach returns, before its prompt.
    cancel_after_attach: bool,
    cancel: &'a Mutex<Arc<CancelToken>>,
}

impl HerdrTurnPorts for Ports<'_> {
    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
        Some(self.launcher.clone())
    }

    fn hook_events(&self) -> tokio::sync::broadcast::Receiver<HookEvent> {
        self.hooks.lock().unwrap().take().unwrap()
    }

    fn attach(&self, request: &AttachRequest<'_>) -> Result<bool, String> {
        let seen = (self.rig.sends().len(), self.started.load(Ordering::SeqCst));
        self.attaches.lock().unwrap().push(seen);
        let attached = self.boot.attach(request);
        if self.cancel_after_attach {
            let cancel = self.cancel.lock().unwrap().clone();
            cancel.cancelled.store(true, Ordering::SeqCst);
        }
        attached
    }

    fn confirm_bound(
        &self,
        owner: &HostedOwner,
        record: &HostedExecution,
        target: &crate::services::session_host::HerdrTarget,
    ) -> Result<(), String> {
        self.boot.confirm_bound(owner, record, target)
    }
}

/// A Claude channel's canonical row on PG, its Herdr endpoint, hook receiver and project dir.
struct Fixture {
    ingress: Ingress,
    rt: tokio::runtime::Runtime,
    db: Option<crate::db::auto_queue::test_support::TestPostgresDb>,
    pool: PgPool,
    owner: HostedOwner,
    rig: HerdrRig,
    cwd: tempfile::TempDir,
    started: AtomicBool,
    finished: AtomicBool,
    cancel: Mutex<Arc<CancelToken>>,
    _home: (Guard, tempfile::TempDir, Guard),
    _root: TestRuntimeRootGuard,
}

impl Fixture {
    fn new(tag: &str, hosted: Option<Value>) -> Self {
        let root = TestRuntimeRootGuard::new();
        let home = tempfile::tempdir().unwrap();
        let env = Guard::set_path_after_shared_test_env_lock("CLAUDE_CONFIG_DIR", home.path());
        let claude_bin = home.path().join("claude");
        std::fs::write(&claude_bin, "#!/bin/bash\necho '2.1.0 (Claude Code)'\n").unwrap();
        let executable = std::os::unix::fs::PermissionsExt::from_mode(0o700);
        std::fs::set_permissions(&claude_bin, executable).unwrap();
        let bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &claude_bin);
        let ingress = Ingress::new();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: TOKEN.into(),
            channel_id: CHANNEL.to_string(),
            logical_key: format!("AgentDesk-claude-p9b3b-{tag}"),
            owner_node: NODE.into(),
            runtime_root: "/adk/runtime".into(),
        };
        let (db, pool) = rt.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id, hosted_execution)
                 VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
            )
            .bind(format!("claude/{TOKEN}/{NODE}:{}", owner.logical_key))
            .bind(TOKEN)
            .bind(CHANNEL.to_string())
            .bind(hosted)
            .execute(&pool)
            .await
            .unwrap();
            (db, pool)
        });
        Self {
            ingress,
            rt,
            db: Some(db),
            pool,
            owner,
            rig: HerdrRig::start(),
            cwd: tempfile::tempdir().unwrap(),
            started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            cancel: Mutex::new(Arc::new(CancelToken::new())),
            _home: (env, home, bin),
            _root: root,
        }
    }

    fn endpoint(&self) -> HerdrLaunchEndpoint {
        HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: KEY.into(),
            socket_addr: self.rig.socket().display().to_string(),
            herdr_session: SESSION.into(),
        }
    }

    fn logical(&self) -> &str {
        &self.owner.logical_key
    }

    /// A Bound execution of `NONCE` on the rig's pane, its `.host_kind` marker and attached source,
    /// as a launched turn leaves them.
    fn bound(&self) -> HostedExecution {
        let marker = crate::services::tmux_common::session_temp_path(self.logical(), "host_kind");
        std::fs::write(marker, "herdr").unwrap();
        let session = uuid();
        let transcript = self.ingress.transcript(&session);
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            self.logical(),
            claude(&transcript, &session),
        );
        HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: NONCE.into(),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: KEY.into(),
                socket_addr: self.rig.socket().display().to_string(),
                named_session: SESSION.into(),
                pane_id: PANE.into(),
            }),
            expected: Some(expected(NONCE)),
            source_ref: SourceRef {
                runtime_root: self.owner.runtime_root.clone(),
                channel: self.owner.channel_id.clone(),
                provider: "claude".into(),
                logical_key: self.owner.logical_key.clone(),
                execution_nonce: NONCE.into(),
                initial_source: None,
                baseline_event_seq: None,
            },
            owner: self.owner.clone(),
        }
    }

    fn row(&self) -> Option<HostedState> {
        let raw: Option<Value> = self.rt.block_on(async {
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
                .bind(CHANNEL.to_string())
                .fetch_one(&self.pool)
                .await
                .unwrap()
        });
        match HostedRecord::decode(raw.as_ref()) {
            HostedRecord::Known(record) => Some(record.state),
            _ => None,
        }
    }

    fn ports(&self, launcher: &Arc<Launcher>) -> Ports<'_> {
        Ports {
            boot: BootPorts {
                pool: &self.pool,
                channel_id: CHANNEL,
            },
            launcher: launcher.clone(),
            hooks: Mutex::new(Some(self.ingress.state.subscribe())),
            rig: &self.rig,
            started: &self.started,
            attaches: Mutex::default(),
            cancel_after_attach: false,
            cancel: &self.cancel,
        }
    }

    /// Cancels the turn now running.
    fn cancel_now(&self) {
        let cancel = self.cancel.lock().unwrap().clone();
        cancel.cancelled.store(true, Ordering::SeqCst);
    }

    /// The row as the turn host reads it.
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

    /// `bound()` written to the row, with the pane running its provider.
    fn store_bound(&self) -> HostedRecord {
        let record = self.bound();
        self.rt.block_on(async {
            sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE channel_id = $2")
                .bind(serde_json::to_value(&record).unwrap())
                .bind(CHANNEL.to_string())
                .execute(&self.pool)
                .await
                .unwrap();
        });
        self.rig.run_provider(&self.rig.context(NONCE), false);
        HostedRecord::Known(record)
    }

    /// Plays a launched pane's provider once its launch is evidenced, up to its SessionStart
    /// when `start`; `None` once the turn ended first.
    fn start_provider(&self, launcher: &Launcher, start: bool) -> Option<Started> {
        if !wait_for(&self.finished, "launch evidence", || {
            !launcher.nonces.lock().unwrap().is_empty()
        }) {
            return None;
        }
        let nonce = launcher.nonces.lock().unwrap()[0].clone();
        let root = crate::config::runtime_root().unwrap();
        let context = root.join(format!("runtime/binding_contexts/claude/{nonce}.json"));
        self.rig.run_provider(&context, false);
        let launched = crate::services::tui_prompt_dedupe::binding_context::execution_context(
            "claude", &nonce,
        )
        .unwrap();
        let session = launched.expected_native_session_id.unwrap();
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            self.cwd.path(),
            &session,
            None,
        )
        .unwrap();
        // The pane's hook relay names the launch context its environment carries.
        let envelope = HookBindingEnvelope::capture_from_env("claude", |name| {
            (name == "AGENTDESK_BINDING_CONTEXT").then(|| context.clone().into_os_string())
        });
        let started = Started {
            session,
            path,
            envelope: envelope.encode().unwrap(),
        };
        if start {
            let payload = json!({"session_id": started.session, "source": "startup",
                "transcript_path": started.path});
            self.started.store(true, Ordering::SeqCst);
            assert_eq!(started.hook(self, "SessionStart", &payload), 202);
        }
        Some(started)
    }

    /// The provider's answer to the prompt once its paste and Enter arrived.
    fn answer(&self, started: &Started) {
        if !wait_for(&self.finished, "the prompt", || self.rig.sends().len() == 2) {
            return;
        }
        let session = &started.session;
        let user = json!({"type": "user", "sessionId": session,
            "message": {"role": "user", "content": "질문"}});
        append(&started.path, &[user]);
        let payload =
            json!({"session_id": session, "prompt": "질문", "transcript_path": started.path});
        assert_eq!(started.hook(self, "UserPromptSubmit", &payload), 202);
        let answer = json!({"type": "assistant", "sessionId": session, "message": {
            "role": "assistant", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "답"}]}});
        let done = json!({"type": "system", "subtype": "turn_duration", "sessionId": session});
        append(&started.path, &[answer, done]);
    }

    /// Runs one turn on its own thread, as provider dispatch does on a blocking thread, while
    /// `claude` plays the pane's provider on this one.
    fn turn(
        &self,
        row: &HostedRecord,
        ports: &Ports<'_>,
        claude: impl FnOnce(),
    ) -> (Result<(), String>, Vec<StreamMessage>) {
        // The receiver stays on this thread; the executor takes only what it shares.
        let (rt, rig, pool) = (&self.rt, &self.rig, &self.pool);
        let (owner, endpoint, cwd) = (self.owner.clone(), self.endpoint(), self.cwd.path());
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
                let turn = HerdrTurn {
                    pool,
                    owner,
                    channel_id: CHANNEL,
                    endpoint,
                    row: Some(row),
                    prompt: "질문",
                    working_dir: cwd.to_str().unwrap(),
                    system_prompt: None,
                    model: None,
                    hook_endpoint: Some("http://127.0.0.1:1".into()),
                    cancel: Some(token),
                };
                let result = herdr_turn::execute(turn, ports, sender);
                finished.store(true, Ordering::SeqCst);
                (result, receiver.try_iter().collect())
            });
            // A provider that gave up, or a turn still reading after it, is cancelled, so the
            // executor never outlives the test.
            let played = std::panic::catch_unwind(std::panic::AssertUnwindSafe(claude));
            let deadline = Instant::now() + LIMIT;
            while played.is_ok() && !finished.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let outlived = played.is_ok() && !finished.load(Ordering::SeqCst);
            if played.is_err() || outlived {
                cancel.cancelled.store(true, Ordering::SeqCst);
            }
            let joined = executor.join();
            if let Err(panic) = played {
                std::panic::resume_unwind(panic);
            }
            assert!(
                !outlived,
                "the turn was still running after its provider finished"
            );
            joined.unwrap()
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
    let stop = std::env::temp_dir().join(format!("adk-ht-none-{}", uuid::Uuid::new_v4()));
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

fn prompt_sends() -> Vec<Value> {
    vec![
        json!({"pane_id": PANE, "text": "\u{1b}[200~질문\u{1b}[201~"}),
        json!({"pane_id": PANE, "keys": ["enter"]}),
    ]
}

/// A launched provider's session, transcript and the hook envelope its pane relays.
struct Started {
    session: String,
    path: PathBuf,
    envelope: String,
}

impl Started {
    fn hook(&self, fx: &Fixture, event: &str, payload: &Value) -> u16 {
        let uri = format!("/hooks/claude/{event}?session_id={}", self.session);
        let sent = fx
            .ingress
            .send_envelope(&uri, payload, None, Some(&self.envelope));
        sent.0
    }
}

fn hold_of(nonce: &str) -> PathBuf {
    let root = crate::config::runtime_root().unwrap();
    root.join("runtime/herdr_input_holds").join(nonce)
}

// A Legacy row: one create, the attach after SessionStart and before any pane write, one paste
// and Enter, then Bound once the transcript resolves the Pending source, and the watcher handoff.
#[test]
fn t_e1_a_legacy_row_launches_once_attaches_after_session_start_prompts_once_then_binds_pg() {
    let fx = Fixture::new("launch", None);
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let transcript = Mutex::new(PathBuf::new());
    let (result, messages) = fx.turn(&HostedRecord::Legacy, &ports, || {
        if let Some(started) = fx.start_provider(&launcher, true) {
            *transcript.lock().unwrap() = started.path.clone();
            fx.answer(&started);
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(*ports.attaches.lock().unwrap(), [(0, true)]);
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(fx.row(), Some(HostedState::Bound));
    let transcript = transcript.lock().unwrap().display().to_string();
    assert!(
        messages.iter().any(|message| matches!(message,
            StreamMessage::RuntimeReady { handoff: crate::services::agent_protocol::RuntimeHandoff::ClaudeTui {
                transcript_path, tmux_session_name, ..
            } } if *transcript_path == transcript && tmux_session_name == fx.logical())),
        "{messages:?}"
    );
}

// A Bound execution whose provider was replaced: the reconcile refuses it before any pane write,
// and nothing is launched in its place.
#[test]
fn t_e2_a_bound_mismatch_writes_nothing_and_relaunches_nothing_pg() {
    let fx = Fixture::new("mismatch", None);
    let record = fx.store_bound();
    fx.rig.run_provider(&fx.rig.context(NONCE), true);
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let (result, _) = fx.turn(&record, &ports, || {});
    let error = result.unwrap_err();
    assert!(error.contains("bound execution not confirmed"), "{error}");
    assert!(fx.rig.sends().is_empty());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
    assert_eq!(fx.row(), Some(HostedState::Bound));
}

// A paste whose reply never came may have landed: the turn stops there, with no Enter, no second
// paste and no relaunch.
#[test]
fn t_e3_an_unclear_send_is_never_sent_again_pg() {
    let fx = Fixture::new("unclear", None);
    let record = fx.store_bound();
    fx.rig.leave_sends_unanswered(true);
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let (result, _) = fx.turn(&record, &ports, || {});
    assert!(result.is_err());
    assert_eq!(fx.rig.sends(), prompt_sends()[..1]);
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
}

// A launch whose SessionStart never came stays Pending with its evidence; the next turn neither
// attaches nor prompts it, and nothing is created again.
#[test]
fn a_pending_execution_with_no_logged_start_is_not_attached_or_prompted_pg() {
    let fx = Fixture::new("unstarted", None);
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        if fx.start_provider(&launcher, false).is_some() {
            fx.cancel_now();
        }
    });
    let first = first.unwrap_err();
    assert!(first.contains("no SessionStart"), "{first}");
    assert_eq!(fx.row(), Some(HostedState::Pending));
    let ports = fx.ports(&launcher);
    let (second, _) = fx.turn(&fx.record(), &ports, || {});
    let second = second.unwrap_err();
    assert!(second.contains("no logged start"), "{second}");
    assert!(ports.attaches.lock().unwrap().is_empty());
    assert!(fx.rig.sends().is_empty());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}

// A Pending pane launched without evidence, whose provider would now probe as its own, is still
// neither attached nor prompted while no start of it is logged.
#[test]
fn a_reprobed_pending_execution_with_no_logged_start_is_not_attached_or_prompted_pg() {
    let fx = Fixture::new("reprobe", None);
    let launcher = Arc::new(Launcher::default());
    launcher.unconfirmed.store(true, Ordering::SeqCst);
    let ports = fx.ports(&launcher);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &ports, || {});
    assert!(first.is_err());
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    let HostedRecord::Known(pending) = fx.record() else {
        panic!("no stored execution");
    };
    assert_eq!(pending.state, HostedState::Pending);
    assert!(pending.expected.is_none());
    launcher.unconfirmed.store(false, Ordering::SeqCst);
    let root = crate::config::runtime_root().unwrap();
    let context = root.join(format!("runtime/binding_contexts/claude/{nonce}.json"));
    fx.rig.run_provider(&context, false);
    let ports = fx.ports(&launcher);
    let (second, _) = fx.turn(&fx.record(), &ports, || {});
    let second = second.unwrap_err();
    assert!(second.contains("no logged start"), "{second}");
    assert!(ports.attaches.lock().unwrap().is_empty());
    assert!(fx.rig.sends().is_empty());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}

// A Pending execution attached after its SessionStart, whose first prompt was cancelled before
// any write, takes the next turn's prompt once and turns Bound.
#[test]
fn a_pending_execution_with_a_logged_start_takes_its_first_prompt_pg() {
    let fx = Fixture::new("started", None);
    let launcher = Arc::new(Launcher::default());
    let mut ports = fx.ports(&launcher);
    ports.cancel_after_attach = true;
    let started = Mutex::new(None);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        *started.lock().unwrap() = fx.start_provider(&launcher, true);
    });
    assert!(first.is_err());
    assert_eq!(ports.attaches.lock().unwrap().len(), 1);
    assert!(fx.rig.sends().is_empty());
    assert_eq!(fx.row(), Some(HostedState::Pending));
    let started = started.into_inner().unwrap().unwrap();
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(!hold_of(&nonce).exists());
    let ports = fx.ports(&launcher);
    let (second, _) = fx.turn(&fx.record(), &ports, || fx.answer(&started));
    assert_eq!(second, Ok(()));
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(fx.row(), Some(HostedState::Bound));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}

// A paste that may have landed without its Enter holds the execution: the next turn writes
// nothing, so the earlier text is never submitted with it.
#[test]
fn after_an_unclear_paste_the_next_prompt_is_held_pg() {
    let fx = Fixture::new("held", None);
    let record = fx.store_bound();
    let launcher = Arc::new(Launcher::default());
    fx.rig.leave_sends_unanswered(true);
    let (first, _) = fx.turn(&record, &fx.ports(&launcher), || {});
    assert!(first.is_err());
    fx.rig.leave_sends_unanswered(false);
    let ports = fx.ports(&launcher);
    let (second, _) = fx.turn(&record, &ports, || {});
    let second = second.unwrap_err();
    assert!(second.contains("input held"), "{second}");
    assert_eq!(fx.rig.sends(), prompt_sends()[..1]);
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
}

// A turn cancelled after its paste was confirmed leaves that text in the composer: the next turn
// is held the same way.
#[test]
fn after_a_paste_then_cancel_the_next_prompt_is_held_pg() {
    let fx = Fixture::new("pasted", None);
    let record = fx.store_bound();
    let launcher = Arc::new(Launcher::default());
    let (first, _) = fx.turn(&record, &fx.ports(&launcher), || {
        if wait_for(&fx.finished, "the paste", || !fx.rig.sends().is_empty()) {
            fx.cancel_now();
        }
    });
    assert!(first.is_err());
    assert_eq!(fx.rig.sends(), prompt_sends()[..1]);
    let (second, _) = fx.turn(&record, &fx.ports(&launcher), || {});
    let second = second.unwrap_err();
    assert!(second.contains("input held"), "{second}");
    assert_eq!(fx.rig.sends(), prompt_sends()[..1]);
}

// A matched Bound execution with a settled composer takes the prompt exactly once, reads its
// answer and hands the transcript on, with nothing launched and no hold left.
#[test]
fn a_matched_bound_execution_takes_one_prompt_pg() {
    let fx = Fixture::new("warm", None);
    let record = fx.store_bound();
    let launcher = Arc::new(Launcher::default());
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical()).unwrap();
    let started = Started {
        session: binding.session_id.clone().unwrap(),
        path: PathBuf::from(&binding.output_path),
        envelope: String::new(),
    };
    let (result, messages) = fx.turn(&record, &fx.ports(&launcher), || {
        if wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            let user = json!({"type": "user", "sessionId": started.session,
                "message": {"role": "user", "content": "질문"}});
            append(&started.path, &[user]);
            let answer = json!({"type": "assistant", "sessionId": started.session, "message": {
                "role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "답"}]}});
            let done =
                json!({"type": "system", "subtype": "turn_duration", "sessionId": started.session});
            append(&started.path, &[answer, done]);
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
    assert!(!hold_of(NONCE).exists());
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, StreamMessage::RuntimeReady { .. })),
        "{messages:?}"
    );
}

// A judgment that no send used is gone after a refused pane, a refused paste or a buffer load the
// turn stopped after: a direct send then writes nothing.
#[test]
fn a_judgment_left_by_an_early_exit_admits_no_later_send() {
    let _root = TestRuntimeRootGuard::new();
    let marker =
        crate::services::tmux_common::session_temp_path("AgentDesk-claude-p9b3b-pin", "host_kind");
    std::fs::write(marker, "herdr").unwrap();
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let _admission = open_admission();
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: TOKEN.into(),
        channel_id: CHANNEL.to_string(),
        logical_key: "AgentDesk-claude-p9b3b-pin".into(),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    rig.run_provider(&rig.context(NONCE), false);
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: NONCE.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(expected(NONCE)),
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: "claude".into(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: NONCE.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    };
    let target = herdr_endpoints().target(&record).unwrap();
    let unjudged = Ok(HostMutation::Refused(HostRefusal::Precondition(
        "herdr_input_without_gate_judgment".into(),
    )));
    let exits: [(&str, &dyn Fn()); 3] = [
        ("another pane", &|| {
            let refused = MutationGate::admit(&target, "another-pane");
            assert_eq!(refused, Err(InputRefusal::Conflict));
        }),
        ("paste end marker", &|| {
            let refused = target.send_paste("a\u{1b}[201~b").unwrap();
            assert!(matches!(refused, HostMutation::Refused(_)), "{refused:?}");
        }),
        ("cancel after the buffer load", &|| {
            HerdrInput::new(&target)
                .load_buffer("buffer", "text")
                .unwrap();
        }),
    ];
    for (exit, run) in exits {
        target.pin(HerdrMutation::Input).unwrap();
        run();
        assert_eq!(target.send_text("x"), unjudged, "{exit}");
    }
    assert!(rig.sends().is_empty(), "{:?}", rig.sends());
}

/// A configured channel's turn as provider dispatch receives it.
fn dispatch(switch: Option<bool>) -> Result<(), String> {
    let _switch = force_switch_for_test(switch);
    let endpoint = HerdrLaunchEndpoint {
        execution_node: NODE.into(),
        config_key: KEY.into(),
        socket_addr: "/tmp/adk-ht-unused.sock".into(),
        herdr_session: SESSION.into(),
    };
    let host = TurnHost::Herdr(Box::new(HerdrTurnPlan {
        endpoint,
        row: None,
    }));
    let provider = ProviderKind::Claude;
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
        tmux_session_name: Some("AgentDesk-claude-dash"),
        teardown: None,
        host: &host,
        channel_id: CHANNEL,
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
    let (sender, _receiver) = std::sync::mpsc::channel();
    execute(turn, sender)
}

// Off or unset, a configured turn is refused as before and reaches no executor; on, it enters the
// Herdr executor, which here stops at its first check for want of a database.
#[test]
fn the_switch_alone_decides_whether_a_configured_turn_reaches_the_herdr_executor() {
    let refused = Err("herdr turn refused: executor_not_wired".to_string());
    assert_eq!(dispatch(None), refused);
    assert_eq!(dispatch(Some(false)), refused);
    assert_eq!(
        dispatch(Some(true)),
        Err("herdr turn: no database".to_string())
    );
}

// An unconfigured channel passes both entries' first judgements before its session key is built,
// with the switch on or off: its turn keeps the existing path.
#[tokio::test]
async fn an_unconfigured_channel_passes_both_entries_without_a_read() {
    use crate::services::turn_host::{intake_refusal_before_turn, refusal_before_turn};
    for on in [None, Some(true)] {
        let _switch = force_switch_for_test(on);
        let built = std::cell::Cell::new(false);
        let key = || {
            built.set(true);
            async { None }
        };
        assert_eq!(
            intake_refusal_before_turn(None, &ProviderKind::Claude, 8, key).await,
            None
        );
        let key = || {
            built.set(true);
            async { None }
        };
        assert_eq!(
            refusal_before_turn(None, &ProviderKind::Claude, 8, key).await,
            None
        );
        assert!(!built.get(), "{on:?}");
    }
}
