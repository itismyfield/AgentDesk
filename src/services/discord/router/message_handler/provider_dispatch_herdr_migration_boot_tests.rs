//! The real BOOT and first-init path precede migration planning on the actual dispatcher.

use super::*;
use crate::services::claude_tui::hook_server::{HookEventKind, publish_hook_event_for_tests};
use crate::services::tui_o::channel_policy::{self, Adoption};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::OStore;
use crate::services::tui_o::writer::activation::test_hook::{self, Step};
use crate::services::tui_o::writer::adoption::{LegacyCursor, LegacyView};
use crate::services::tui_o::writer::host::test_io::TestHost;
use crate::services::tui_o::writer::host::{self, HostParts};
use crate::services::tui_prompt_dedupe as dedupe;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

#[derive(Clone, Default)]
pub(super) struct ShadowCapture(
    Arc<Mutex<Vec<Value>>>,
    Arc<Mutex<Option<OnPlan>>>,
    Arc<Mutex<Vec<Value>>>,
);

type OnPlan = Box<dyn FnOnce() + Send>;

struct CaptureLayer(ShadowCapture);

#[derive(Default)]
struct Fields(serde_json::Map<String, Value>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), format!("{value:?}").into());
    }
}

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "agentdesk::claude_migration_dispatch_timing" {
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0.0.lock().unwrap().last_mut().unwrap()["observer_elapsed_us"] =
                fields.0["observer_elapsed_us"].clone();
            return;
        }
        if event.metadata().target() == "agentdesk::claude_migration_history_timing" {
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0.2.lock().unwrap().push(Value::Object(fields.0));
            return;
        }
        if event.metadata().target() != "agentdesk::claude_migration_shadow" {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.0.lock().unwrap().push(Value::Object(fields.0));
        let callback = self.0.1.lock().unwrap().take();
        if let Some(callback) = callback {
            callback();
        }
    }
}

impl ShadowCapture {
    fn with_on_plan(callback: impl FnOnce() + Send + 'static) -> Self {
        Self(
            Arc::default(),
            Arc::new(Mutex::new(Some(Box::new(callback)))),
            Arc::default(),
        )
    }
    pub(super) fn dispatch(&self) -> tracing::Dispatch {
        tracing::Dispatch::new(tracing_subscriber::registry().with(CaptureLayer(self.clone())))
    }

    pub(super) fn records(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }

    pub(super) fn plans(&self) -> Vec<Value> {
        self.records()
            .into_iter()
            .map(|record| {
                let plan = record["plan"]
                    .as_str()
                    .expect("shadow event carries its plan");
                serde_json::from_str(plan).expect("shadow plan is JSON")
            })
            .collect()
    }
}

#[test]
fn a_stop_during_shadow_observation_reaches_the_existing_prelaunch_gate_pg() {
    let fx = Fixture::new("shadow-stop", None);
    let launcher = Arc::new(Launcher::default());
    let token = Arc::new(CancelToken::new());
    let stopped = token.clone();
    let capture = ShadowCapture::with_on_plan(move || {
        assert!(crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available());
        stopped
            .herdr_interrupt_state()
            .expect("stop state installed before shadow observation")
            .user_stop
            .store(true, Ordering::Release);
    });
    let dispatcher = capture.dispatch();
    let _trace = tracing::dispatcher::set_default(&dispatcher);
    let ports = fx.ports(&launcher);
    let (result, _) = fx.turn_on_token(&HostedRecord::Legacy, &ports, token.clone(), || {
        if let Some(started) = fx.start_provider(&launcher, true) {
            fx.answer(&started);
        }
    });
    assert_eq!(
        capture.records().len(),
        1,
        "actual shadow root was observed"
    );
    assert_eq!(
        launcher.creates.load(Ordering::SeqCst),
        0,
        "diagnostic observation must precede the existing prelaunch stop check"
    );
    assert_eq!(result, Ok(()));
    assert!(launcher.nonces.lock().unwrap().is_empty());
    assert!(fx.rig.sends().is_empty());
    assert_eq!(fx.row(), None);
    assert_eq!(
        token
            .herdr_interrupt_state()
            .unwrap()
            .submission
            .lock()
            .unwrap()
            .phase,
        crate::services::provider::herdr_before_start::InputPhase::Closed
    );
}

#[test]
fn an_already_stopped_turn_skips_shadow_observation_and_all_main_effects_pg() {
    let fx = Fixture::new("shadow-already-stopped", None);
    let launcher = Arc::new(Launcher::default());
    let token = Arc::new(CancelToken::new());
    token
        .prepare_herdr_interrupt(ProviderKind::Claude, &fx.owner)
        .user_stop
        .store(true, Ordering::Release);
    let capture = ShadowCapture::default();
    let dispatcher = capture.dispatch();
    let _trace = tracing::dispatcher::set_default(&dispatcher);
    let ports = fx.ports(&launcher);
    let (result, _) = fx.turn_on_token(&HostedRecord::Legacy, &ports, token, || {});
    assert!(
        capture.records().is_empty(),
        "main's already-closed fast path must not start a diagnostic database read"
    );
    assert_eq!(result, Ok(()));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
    assert!(launcher.nonces.lock().unwrap().is_empty());
    assert!(fx.rig.sends().is_empty());
    assert_eq!(fx.row(), None);
}

struct Delivered {
    path: PathBuf,
    end: u64,
}

impl LegacyView for Delivered {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, _: &str) -> LegacyCursor {
        LegacyCursor::Bound {
            path: self.path.clone(),
            offset: self.end,
        }
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        Some(self.end)
    }

    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

fn in_boot_child(name: &str) -> bool {
    const CHILD: &str = "ADK_TEST_CLAUDE_MIGRATION_BOOT";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{}::{name}", module_path!().split_once("::").unwrap().1),
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env_remove(crate::services::tui_o::cutover::test_override::CHANNELS_ENV)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("1 passed; 0 failed; 0 ignored;"),
        "{stdout}"
    );
    eprintln!("{stdout}{}", String::from_utf8_lossy(&output.stderr));
    false
}

fn boot_config(fx: &Fixture) -> crate::config::Config {
    let root = crate::config::runtime_root().unwrap();
    serde_json::from_value(json!({
        "server": {}, "data": {"dir": root.join("data")},
        "cluster": {"enabled": true, "instance_id": NODE, "gateway_preferred_instance_id": NODE},
        "runtime": {"herdr_turn_enabled": true},
        "agents": [{"id": "migration-r13", "name": "Migration R13", "channels": {
            "claude": {"id": CHANNEL.to_string(), "runtime": "tui", "workspace": fx.cwd.path()}
        }}],
        "tui_o": {"writer": {"channels": [CHANNEL], "late_adopt": false}},
        "session_hosts": {"herdr": {
            "endpoints": {KEY: {"execution_node": NODE, "socket_path": fx.rig.socket(),
                "herdr_home": root.join("herdr"), "herdr_session": SESSION}},
            "channels": {CHANNEL.to_string(): KEY}
        }}
    }))
    .unwrap()
}

fn intake(fx: &Fixture) -> Value {
    fx.rt.block_on(async {
        sqlx::query_scalar("SELECT to_jsonb(i) FROM intake_outbox i WHERE channel_id = $1")
            .bind(CHANNEL.to_string())
            .fetch_one(&fx.pool)
            .await
            .unwrap()
    })
}

fn fifo(fx: &Fixture) -> Value {
    fx.rt.block_on(async {
        sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_array(id, channel_id, user_msg_id,
            attempt_no, status) ORDER BY created_at, id) FROM intake_outbox WHERE target_instance_id = $1")
            .bind(NODE).fetch_one(&fx.pool).await.unwrap()
    })
}

fn assert_no_init(root: &Path, opened: bool) {
    match OStore::existing(root) {
        Some(store) => {
            assert!(
                store.read_init(CHANNEL).unwrap().is_none(),
                "no migration init"
            );
            assert!(store.read_era().unwrap().is_none(), "no migration era");
        }
        None => assert!(
            !opened,
            "the actual Fresh host opened its store before admission"
        ),
    }
}

fn assert_no_migration(root: &Path) {
    let path = root.join("claude_migration");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Ok(metadata) => panic!(
            "migration state must stay absent: {} ({:?})",
            path.display(),
            metadata.file_type()
        ),
        Err(error) => panic!(
            "cannot prove migration state absent: {}: {error}",
            path.display()
        ),
    }
}

#[test]
fn migration_absence_requires_symlink_metadata_not_found() {
    let root = tempfile::tempdir().unwrap();
    assert_no_migration(root.path());
    let migration = root.path().join("claude_migration");
    std::fs::create_dir(&migration).unwrap();
    assert!(std::panic::catch_unwind(|| assert_no_migration(root.path())).is_err());
    std::fs::remove_dir(&migration).unwrap();
    std::fs::write(&migration, "not a directory").unwrap();
    assert!(std::panic::catch_unwind(|| assert_no_migration(root.path())).is_err());
    std::fs::remove_file(&migration).unwrap();
    std::os::unix::fs::symlink(root.path().join("missing-target"), &migration).unwrap();
    assert!(std::panic::catch_unwind(|| assert_no_migration(root.path())).is_err());
    std::fs::remove_file(&migration).unwrap();
    let not_directory = root.path().join("not-directory");
    std::fs::write(&not_directory, "not a directory").unwrap();
    assert!(std::panic::catch_unwind(|| assert_no_migration(&not_directory)).is_err());
    assert_no_migration(root.path());
}

fn old_source(fx: &Fixture) -> (String, PathBuf, Vec<u8>) {
    let session = uuid();
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        fx.cwd.path(),
        &session,
        None,
    )
    .unwrap();
    append(
        &path,
        &[
            json!({"type":"user", "sessionId":session, "timestamp":"2026-10-01T00:00:00Z",
            "message":{"role":"user","content":"old prompt"}}),
            json!({"type":"assistant", "sessionId":session, "timestamp":"2026-10-01T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"old answer"}]}}),
            json!({"type":"system", "subtype":"turn_duration", "sessionId":session,
            "timestamp":"2026-10-01T00:00:02Z"}),
        ],
    );
    let bytes = std::fs::read(&path).unwrap();
    dedupe::register_tmux_channel(fx.logical(), CHANNEL);
    dedupe::register_provider_session("claude", &session, fx.logical());
    dedupe::register_tmux_runtime_binding(
        fx.logical(),
        dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(session.clone()),
            last_offset: bytes.len() as u64,
            relay_last_offset: None,
        },
    );
    let attachment = fx.cwd.path().join("r13-input.txt");
    std::fs::write(&attachment, "preserved attachment").unwrap();
    fx.rt.block_on(async {
        sqlx::query(
            "UPDATE sessions SET claude_session_id = $1, raw_provider_session_id = $1,
            claude_session_id_recorded_at = NOW(), cwd = $2 WHERE channel_id = $3",
        )
        .bind(&session)
        .bind(fx.cwd.path().display().to_string())
        .bind(CHANNEL.to_string())
        .execute(&fx.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id,
            channel_id, user_msg_id, request_owner_id, user_text, turn_kind, agent_id, provider,
            status, attachment_refs, replay_source_message_ids, created_at)
            VALUES ($1, $1, $2, '7503001', '7503002', 'original pending prompt',
                    'standard', 'migration-r13', 'claude', 'pending', $3,
                    ARRAY['7503001','7503000'], '2026-10-01T00:00:00Z')",
        )
        .bind(NODE)
        .bind(CHANNEL.to_string())
        .bind(json!([{"filename":"r13-input.txt", "path":attachment}]))
        .execute(&fx.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id,
            channel_id, user_msg_id, request_owner_id, user_text, turn_kind, agent_id, provider,
            status, created_at) VALUES ($1, $1, $2, '7503011', '7503002', 'next pending prompt',
                    'standard', 'migration-r13-next', 'claude', 'pending', '2026-10-01T00:00:01Z')",
        )
        .bind(NODE)
        .bind((CHANNEL - 1).to_string())
        .execute(&fx.pool)
        .await
        .unwrap();
    });
    (session, path, bytes)
}

#[derive(Debug, PartialEq, Eq)]
enum FileTreeEntry {
    Directory,
    File(Vec<u8>),
}

fn file_tree(path: &Path) -> std::collections::BTreeMap<PathBuf, FileTreeEntry> {
    fn read(
        root: &Path,
        path: &Path,
        result: &mut std::collections::BTreeMap<PathBuf, FileTreeEntry>,
    ) {
        if !path.exists() {
            return;
        }
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    FileTreeEntry::Directory,
                );
                read(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    FileTreeEntry::File(std::fs::read(path).unwrap()),
                );
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    read(path, path, &mut result);
    result
}

#[test]
fn file_tree_observes_empty_migration_directories() {
    let root = tempfile::tempdir().unwrap();
    let before = file_tree(root.path());
    std::fs::create_dir_all(root.path().join("claude_migration/unused")).unwrap();
    let after = file_tree(root.path());
    assert_ne!(
        after, before,
        "an empty migration directory is a filesystem effect"
    );
    assert!(after.contains_key(Path::new("claude_migration")));
    assert!(after.contains_key(Path::new("claude_migration/unused")));
}

fn actual_dispatch(
    fx: &Fixture,
    launcher: &Arc<Launcher>,
    session: &str,
    capture: &ShadowCapture,
    stop_on_plan: bool,
) -> Result<(), String> {
    let (runtime, pool, finished, rig) = (&fx.rt, &fx.pool, &fx.finished, &fx.rig);
    let owner = fx.owner.clone();
    let cwd = fx.cwd.path().display().to_string();
    let endpoint = fx.endpoint();
    let events = dedupe::binding_events::test_root();
    let dispatcher = capture.dispatch();
    let launcher_thread = launcher.clone();
    let token = Arc::new(CancelToken::new());
    *fx.cancel.lock().unwrap() = token.clone();
    if stop_on_plan {
        let stopped = token.clone();
        *capture.1.lock().unwrap() = Some(Box::new(move || {
            stopped
                .herdr_interrupt_state()
                .unwrap()
                .user_stop
                .store(true, Ordering::Release);
        }));
    }
    finished.store(false, Ordering::SeqCst);
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            let _runtime = runtime.enter();
            let _tracing = tracing::dispatcher::set_default(&dispatcher);
            let _registry = rig.registry_on_this_thread();
            let _admission = open_admission();
            let _switch = force_switch_for_test(Some(true));
            dedupe::binding_events::set_test_root(events.as_deref());
            let row = runtime.block_on(herdr_turn::load(pool, &owner));
            let crate::db::dispatched_sessions::hosted_execution::HostedLookup::Found(row) = row else {
                panic!("the dispatcher reads the canonical Legacy row");
            };
            if !stop_on_plan { assert!(matches!(row.record, HostedRecord::Legacy)); }
            let host = TurnHost::Herdr(Box::new(HerdrTurnPlan { endpoint, row: Some(row) }));
            let provider = ProviderKind::Claude;
            let turn = StreamingTurn {
                pool: Some(pool), provider: &provider, prompt: "질문", session_id: Some(session),
                working_dir: &cwd, system_prompt: None, allowed_tools: &[], cancel: token,
                codex_observation: None, remote_profile: None, tmux_session_name: Some(&owner.logical_key),
                teardown: None, host: &host, channel_id: CHANNEL, model: None, native_fast_mode: None,
                codex_goals: None, compact_percent: None, compact_lower_bound_tokens: 0,
                compact_token_limit: None, cache_ttl_minutes: None, dispatch_type: None,
                force_fresh: false, submission: None,
            };
            let context = crate::services::provider_cli::ProviderExecutionContext {
                provider: "claude".into(), session_key: Some(session_key(&owner)), ..Default::default()
            };
            let (sender, _receiver) = std::sync::mpsc::channel();
            let result = crate::services::platform::with_provider_execution_context(context, || {
                herdr_turn::with_test_launch_host(launcher_thread, || {
                    execute(turn, crate::services::discord::router::intake_dispatch::replay_admission::dormant_start("r13").unwrap(), sender)
                })
            });
            finished.store(true, Ordering::SeqCst);
            result
        });
        if let Some(started) = fx.start_provider(launcher, false) {
            publish_hook_event_for_tests(HookEvent {
                provider: "claude".into(),
                session_id: started.session.clone(),
                kind: HookEventKind::SessionStart,
                payload: json!({"session_id":started.session,"source":"startup","transcript_path":started.path}),
                ..Default::default()
            });
            fx.answer(&started);
        }
        worker.join().unwrap()
    })
}

#[test]
fn r13_boot_first_init_and_actual_dispatch_keep_main_effects_pg() {
    if !in_boot_child("r13_boot_first_init_and_actual_dispatch_keep_main_effects_pg") {
        return;
    }
    let fx = Fixture::new("r13", None);
    let root = crate::config::runtime_root().unwrap();
    let (old, path, old_bytes) = old_source(&fx);
    let before_intake = intake(&fx);
    let before_fifo = fifo(&fx);
    assert_eq!(before_fifo[0][2], "7503001");
    assert_eq!(before_fifo[1][2], "7503011");
    assert_eq!(
        before_intake["replay_source_message_ids"],
        json!(["7503001", "7503000"])
    );
    assert_eq!(
        before_intake["attachment_refs"].as_array().unwrap().len(),
        1
    );
    assert_no_migration(&root);
    assert!(
        channel_policy::boot().is_none(),
        "no forced or earlier BOOT snapshot"
    );
    assert!(crate::config::session_hosts::with_boot(
        |boot| boot.is_none()
    ));
    let config = boot_config(&fx);
    fx.rt
        .block_on(crate::bootstrap::prepare_and_install_boot_snapshots(
            &config,
            Some(&fx.pool),
        ))
        .unwrap();
    assert_no_migration(&root);
    assert!(crate::config::session_hosts::herdr_endpoint(CHANNEL).is_some());
    let boot = channel_policy::boot().unwrap();
    assert!(boot.selected().contains(&CHANNEL));
    assert_eq!(boot.candidate(CHANNEL).unwrap().peek(), Adoption::Pending);
    assert_no_init(&root, false);
    assert_eq!(
        fx.rt.handle().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread
    );

    let source = source_id_for(&old, &path).unwrap();
    let io = TestHost::new([(CHANNEL, source)]);
    let rows = fx
        .rt
        .block_on(crate::db::o_channel_activation::activation_rows(
            &fx.pool,
            &CHANNEL.to_string(),
            NODE,
        ))
        .unwrap();
    assert_eq!(
        rows.open_intake, 1,
        "the first-init blocker is the seeded original request"
    );
    io.facts.lock().unwrap().open_intake = rows.open_intake;
    io.sessions
        .lock()
        .unwrap()
        .insert(CHANNEL, fx.logical().into());
    *io.legacy.lock().unwrap() = Some(Arc::new(Delivered {
        path: path.clone(),
        end: old_bytes.len() as u64,
    }));
    let (at, reached) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    test_hook::set(CHANNEL, Step::BeforeAdmit, move || {
        at.send(()).unwrap();
        released
            .recv_timeout(LIMIT)
            .expect("the first-init assertion releases its admission");
        Ok(())
    });
    let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());
    gate.acquired();
    let readiness = host::process_readiness();
    let mut handles = fx.rt.block_on(async {
        host::start_managed(
            ShadowProvider::Claude,
            true,
            crate::services::tui_o::cutover::boot_ownership(),
            || HostParts {
                io: io.clone(),
                runtime_root: Some(root.clone()),
                gate,
                readiness: readiness.clone(),
            },
        )
    });
    assert_eq!(
        handles.len(),
        1,
        "the actual installed BOOT selected one writer host"
    );
    reached
        .recv_timeout(LIMIT)
        .expect("Recovered::Fresh reached first activation admission");
    eprintln!("R13_FIRST_INIT_REACHED");
    assert_no_migration(&root);
    assert_no_init(&root, true);
    assert_eq!(intake(&fx), before_intake);
    release.send(()).unwrap();
    fx.rt.block_on(async {
        tokio::time::timeout(LIMIT, async {
            while boot.candidate(CHANNEL).unwrap().peek() == Adoption::Pending {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("main's first activation refuses the still-pending original input");
    });
    assert_no_migration(&root);
    assert_no_init(&root, true);
    assert_eq!(boot.candidate(CHANNEL).unwrap().peek(), Adoption::Released);
    assert!(!readiness.is_ready(CHANNEL));
    assert_no_init(&root, true);
    assert_eq!(
        intake(&fx),
        before_intake,
        "the first-init path neither rearms nor consumes input"
    );

    let before_dispatch = file_tree(&root);
    let before_home = file_tree(fx._home.1.path());
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:1".into(),
    );
    let launcher = Arc::new(Launcher::default());
    let capture = ShadowCapture::default();
    assert_eq!(
        actual_dispatch(&fx, &launcher, &old, &capture, false),
        Ok(())
    );
    assert_no_migration(&root);
    // These are main's fresh effects, not a migration handoff: one new UUID and one normal prompt.
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(fx.row(), Some(HostedState::Bound));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    let context = dedupe::binding_context::execution_context("claude", &nonce).unwrap();
    let fresh = context.expected_native_session_id.as_deref().unwrap();
    assert_ne!(fresh, old);
    assert_eq!(context.launch_mode, "fresh");
    assert_eq!(
        dedupe::resolve_tmux_session_name("claude", fresh).as_deref(),
        Some(fx.logical())
    );
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(fx.logical()),
        dedupe::binding_context::SpawnNonceMarker::Known(nonce.clone())
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        old_bytes,
        "the former transcript is not written"
    );
    assert_eq!(
        intake(&fx),
        before_intake,
        "shadow planning changes no original request custody"
    );
    assert_eq!(
        fifo(&fx),
        before_fifo,
        "the pending FIFO IDs and order are preserved"
    );
    assert_eq!(
        std::fs::read_to_string(fx.cwd.path().join("r13-input.txt")).unwrap(),
        "preserved attachment"
    );
    assert!(io.posts.to(CHANNEL).is_empty());
    let after_dispatch = file_tree(&root);
    let mut allowed = std::collections::BTreeSet::from([context_of(&nonce)
        .strip_prefix(&root)
        .unwrap()
        .to_path_buf()]);
    for extension in [
        "host_kind",
        "generation",
        "spawn_nonce",
        crate::services::tmux_common::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    ] {
        let marker = PathBuf::from(crate::services::tmux_common::session_temp_path(
            fx.logical(),
            extension,
        ));
        allowed.insert(marker.strip_prefix(&root).unwrap().to_path_buf());
    }
    // A normal prompt clears its hold file but keeps the parent directory.
    let holds = PathBuf::from("runtime/herdr_input_holds");
    if !before_dispatch.contains_key(&holds) {
        assert_eq!(after_dispatch.get(&holds), Some(&FileTreeEntry::Directory));
        allowed.insert(holds);
    }
    for file in allowed.clone() {
        for directory in file
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            if !before_dispatch.contains_key(directory) {
                assert_eq!(
                    after_dispatch.get(directory),
                    Some(&FileTreeEntry::Directory),
                    "a fresh file's new parent is an observed directory"
                );
                allowed.insert(directory.to_path_buf());
            }
        }
    }
    let changed: std::collections::BTreeSet<_> = before_dispatch
        .keys()
        .chain(after_dispatch.keys())
        .filter(|path| before_dispatch.get(*path) != after_dispatch.get(*path))
        .cloned()
        .collect();
    assert_eq!(
        changed, allowed,
        "only main's fresh context and session files change; no migration receipt/init/rearm"
    );
    let fresh_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        fx.cwd.path(),
        fresh,
        None,
    )
    .unwrap();
    let after_home = file_tree(fx._home.1.path());
    let changed_home: std::collections::BTreeSet<_> = before_home
        .keys()
        .chain(after_home.keys())
        .filter(|path| before_home.get(*path) != after_home.get(*path))
        .cloned()
        .collect();
    assert_eq!(
        changed_home,
        std::collections::BTreeSet::from([fresh_path
            .strip_prefix(fx._home.1.path())
            .unwrap()
            .to_path_buf(),]),
        "main's one fresh transcript is the only provider-home write"
    );
    assert_no_init(&root, true);
    let plans = capture.plans();
    assert_eq!(
        plans.len(),
        1,
        "actual dispatcher reached exactly one shadow root: {:?}",
        capture.records()
    );
    assert_eq!(plans[0]["decision"], "resume");
    assert_eq!(plans[0]["context"]["expected_native_session_id"], old);
    assert_eq!(plans[0]["source"]["path"], path.display().to_string());
    fx.rt
        .block_on(async { handles.pop().unwrap().stop_and_join().await })
        .unwrap();
}

#[test]
fn actual_dispatch_shadow_latency_and_history_lock_samples_pg() {
    if !in_boot_child("actual_dispatch_shadow_latency_and_history_lock_samples_pg") {
        return;
    }
    use std::io::Write;
    let fx = Fixture::new("shadow-cost", None);
    let config = boot_config(&fx);
    fx.rt
        .block_on(crate::bootstrap::prepare_and_install_boot_snapshots(
            &config,
            Some(&fx.pool),
        ))
        .unwrap();
    let (sid, path, _) = old_source(&fx);
    let source = source_id_for(&sid, &path).unwrap();
    let events_root = dedupe::binding_events::test_root().unwrap();
    let dir = events_root.join(dedupe::binding_events::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join(format!("{CHANNEL}.log"));
    let bound = fx.bound();
    let pending = pending_on(&fx.owner, NONCE, bound.location.clone(), None);
    dedupe::register_tmux_runtime_binding(fx.logical(), claude(&path, &sid));
    let _legacy_context = dedupe::binding_context::PreparedIncarnation::create(
        dedupe::binding_context::BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: NONCE.into(),
            tmux_session: fx.logical().into(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: Some(NODE.into()),
            expected_native_session_id: Some(sid.clone()),
            launch_mode: "fresh".into(),
            provider_root: Some(
                crate::services::claude_tui::transcript_tail::default_claude_home()
                    .unwrap()
                    .join("projects"),
            ),
            first_prompt_digest: None,
            source_policy: None,
            claude_resume: None,
        },
    )
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(fx.logical(), "spawn_nonce"),
        NONCE,
    )
    .unwrap();
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:1".into(),
    );
    for count in [0, 1_000, 10_000] {
        let mut file = std::fs::File::create(&log).unwrap();
        for seq in 1..=count {
            writeln!(file, "{}", json!({"seq":seq,"channel_id":CHANNEL,"provider":"claude","tmux_session":fx.logical(),"execution_nonce":NONCE,"old":null,"new":{"source":source},"cause":"resume","parent_hint":null,"evidence":{"hook_event":"session_start","received_at":chrono::Utc::now()},"committed_at":chrono::Utc::now(),"verified":true})).unwrap();
        }
        drop(file);
        for kind in ["legacy", "bound", "pending"] {
            match kind {
                "legacy" => {
                    fx.rt
                        .block_on(
                            sqlx::query(
                                "UPDATE sessions SET hosted_execution=NULL WHERE channel_id=$1",
                            )
                            .bind(CHANNEL.to_string())
                            .execute(&fx.pool),
                        )
                        .unwrap();
                }
                "bound" => fx.store(&bound),
                _ => fx.store(&pending),
            }
            let mut elapsed = Vec::new();
            let mut holds = Vec::new();
            let mut waits = Vec::new();
            for _ in 0..10 {
                let launcher = Arc::new(Launcher::default());
                let capture = ShadowCapture::default();
                assert_eq!(
                    actual_dispatch(&fx, &launcher, &sid, &capture, true),
                    Ok(())
                );
                let events = capture.records();
                assert_eq!(events.len(), 1, "actual dispatch must reach resolver");
                let plans = capture.plans();
                let value = |v: &Value| v.as_str().unwrap().parse::<u64>().unwrap();
                elapsed.push(value(&events[0]["observer_elapsed_us"]));
                let history = capture.2.lock().unwrap();
                if kind == "legacy" {
                    assert_eq!(
                        plans[0]["decision"], "resume",
                        "measurement must use a valid old source"
                    );
                    assert_eq!(plans[0]["readset"]["old_context"]["execution_nonce"], NONCE);
                    assert_eq!(plans[0]["source"]["session_id"], sid);
                    assert_eq!(plans[0]["source"]["path"], path.display().to_string());
                    assert_eq!(history.len(), 1, "actual strict reader must acquire LOGS");
                    assert_eq!(value(&history[0]["records"]), count);
                    assert_eq!(
                        value(&history[0]["bytes"]),
                        std::fs::metadata(&log).unwrap().len()
                    );
                    holds.push(value(&history[0]["lock_hold_us"]));
                    waits.push(value(&history[0]["lock_wait_us"]));
                } else {
                    assert_eq!(plans[0]["decision"], "existing_hosted");
                    assert!(
                        history.is_empty(),
                        "existing Herdr rows do not need migration history reads"
                    );
                    holds.push(0);
                    waits.push(0);
                }
                assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
            }
            elapsed.sort_unstable();
            holds.sort_unstable();
            waits.sort_unstable();
            eprintln!(
                "P2_COST {}",
                json!({"context":"present","kind":kind,"records":count,"samples":elapsed.len(),"observer_median_us":elapsed[elapsed.len()/2],"observer_max_us":elapsed.last(),"lock_hold_median_us":holds[holds.len()/2],"lock_hold_max_us":holds.last(),"lock_wait_max_us":waits.last()})
            );
        }
    }
}
