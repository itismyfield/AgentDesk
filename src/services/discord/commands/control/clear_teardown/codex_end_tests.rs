use super::*;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::codex_tui::execution_end::attempt;
use crate::services::process::{ProcessGroupProbe, ProcessIdentity, ProcessIdentityProbe};
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tmux_common as tc;
use crate::services::tui_o::store::codex_end::{self, ContextSnapshot};
use crate::services::tui_prompt_dedupe::binding_context::{
    self, BindingContext, PreparedIncarnation,
};
use crate::services::tui_prompt_dedupe::binding_events as p5;
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding};

struct Fixture {
    logs: tempfile::TempDir,
    _state: std::sync::MutexGuard<'static, ()>,
    _host: crate::config::TestEnvVarGuard,
    _config: crate::config::TestEnvVarGuard,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Fixture {
    fn new() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let logs = tempfile::tempdir().unwrap();
        let config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &logs.path().join("missing-config.yaml"),
        );
        let host = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_INSTANCE_ID",
            std::ffi::OsStr::new("a1-host"),
        );
        let state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        dedupe::reset_state_for_tests();
        attempt::reset_for_tests();
        p5::set_test_root(Some(logs.path()));
        Self {
            logs,
            _state: state,
            _host: host,
            _config: config,
            _root: root,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        p5::set_test_root(None);
        attempt::reset_for_tests();
        dedupe::reset_state_for_tests();
    }
}

struct FixtureProcess {
    pid: u32,
    identity: ProcessIdentity,
    reaper: Option<std::thread::JoinHandle<()>>,
    timer: Option<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)>,
}

impl FixtureProcess {
    fn start(group: u32) -> Self {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(group as i32)
            .spawn()
            .unwrap();
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid);
        let reaper = std::thread::spawn(move || {
            child.wait().unwrap();
        });
        Self {
            pid,
            identity,
            reaper: Some(reaper),
            timer: None,
        }
    }

    fn exit_after_pane_death(&mut self, pane: &Self, delay: Duration) {
        let (cancel, received) = std::sync::mpsc::channel();
        let (pid, identity) = (self.pid, self.identity);
        let (pane_pid, pane_identity) = (pane.pid, pane.identity);
        let timer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while pane_identity.probe(pane_pid) != ProcessIdentityProbe::GoneOrReused {
                if received.recv_timeout(Duration::from_millis(10)).is_ok() {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "pane exit precedes the child delay"
                );
            }
            assert_eq!(identity.probe(pid), ProcessIdentityProbe::Same);
            if received.recv_timeout(delay) == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                && identity.probe(pid) == ProcessIdentityProbe::Same
            {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        });
        self.timer = Some((cancel, timer));
    }
}

impl Drop for FixtureProcess {
    fn drop(&mut self) {
        if let Some((cancel, timer)) = self.timer.take() {
            let _ = cancel.send(());
            timer.join().unwrap();
        }
        if self.identity.probe(self.pid) == ProcessIdentityProbe::Same {
            unsafe {
                libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }
        self.reaper.take().unwrap().join().unwrap();
    }
}

struct Pane {
    channel: u64,
    label: String,
    name: String,
    context: BindingContext,
    context_path: PathBuf,
    source: PathBuf,
    process: FixtureProcess,
    fake: FakeTmux,
}

fn binding(path: &Path, session: &str) -> TuiRuntimeBinding {
    TuiRuntimeBinding {
        runtime_kind: CodexTui,
        output_path: path.canonicalize().unwrap().display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_owned()),
        last_offset: 0,
        relay_last_offset: None,
    }
}

impl Pane {
    fn start(dir: &Path) -> Self {
        let id = uuid::Uuid::new_v4();
        let channel = 1_479_671_305_348_000_000 + u64::from(id.as_fields().0);
        let label = format!("a1-{}", id.simple());
        let name = ProviderKind::Codex.build_tmux_session_name(&label);
        let fake = FakeTmux::install(&name);
        fake.become_missing_after_kill();
        let process = FixtureProcess::start(0);
        fake.run_pane(process.pid);
        tc::write_tmux_owner_marker(&name).unwrap();
        let provider_root = dir.join(format!("provider-alias-{id}"));
        std::os::unix::fs::symlink(dir.canonicalize().unwrap(), &provider_root).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: name.clone(),
            channel_id: Some(channel),
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: binding_context::stable_host_identity(),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(provider_root),
            first_prompt_digest: None,
            source_policy: Some("shadow".into()),
        };
        let context_path = PreparedIncarnation::create(context.clone()).unwrap().path;
        std::fs::write(
            tc::session_temp_path(&name, "spawn_nonce"),
            &context.execution_nonce,
        )
        .unwrap();
        let source = dir.join(format!("rollout-2026-10-23-{id}.jsonl"));
        let header = serde_json::json!({"type":"session_meta","payload":{
            "id":id.to_string(),"source":"cli","cwd":"/private/tmp",
            "timestamp":(context.created_at + chrono::Duration::seconds(1)).to_rfc3339()
        }});
        std::fs::write(&source, format!("{header}\n")).unwrap();
        dedupe::register_tmux_channel(&name, channel);
        dedupe::register_launched_tmux_runtime_binding(&name, binding(&source, &id.to_string()));
        assert_eq!(p5::binding_events_since(channel, 0).unwrap().len(), 1);
        crate::services::codex_tui::execution_end::capture::capture(channel, &name)
            .unwrap_or_else(|reason| panic!("valid capture fixture: {reason}"));
        Self {
            channel,
            label,
            name,
            context,
            context_path,
            source,
            process,
            fake,
        }
    }

    fn end(&self, guarded: bool) -> ClearReport {
        runtime().block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let guard = guarded.then(|| {
                shared
                    .session_transition_lock(serenity::ChannelId::new(self.channel))
                    .try_lock_owned()
                    .unwrap()
            });
            let mut report = ClearReport::default();
            end_session(
                &shared,
                &ProviderKind::Codex,
                serenity::ChannelId::new(self.channel),
                (Some(&self.name), None),
                guard,
                &mut report,
            )
            .await;
            report
        })
    }

    fn records(&self) -> Vec<serde_json::Value> {
        let path = crate::config::runtime_root()
            .unwrap()
            .join("o_codex_ends")
            .join(format!("{}.json", self.channel));
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["records"]
                .as_array()
                .unwrap()
                .clone(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read End store: {error}"),
        }
    }

    fn rewrite_context(&self, change: impl FnOnce(&mut BindingContext)) {
        let mut context = self.context.clone();
        change(&mut context);
        std::fs::write(&self.context_path, serde_json::to_vec(&context).unwrap()).unwrap();
    }

    fn rewrite_event_nonce(&self, nonce: &str) {
        let mut events = p5::records_strict(self.channel).unwrap().unwrap();
        events[0].execution_nonce = Some(nonce.to_owned());
        let mut bytes = Vec::new();
        for event in events {
            serde_json::to_writer(&mut bytes, &event).unwrap();
            bytes.push(b'\n');
        }
        let path = p5::test_root()
            .unwrap()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{}.log", self.channel));
        std::fs::write(path, bytes).unwrap();
        p5::forget_channel_for_tests(self.channel);
    }

    fn install_ticket(&self, id: &str) {
        tc::with_tmux_source_authority(&self.name, |authority| {
            let epoch = attempt::generation(self.channel, authority);
            assert!(attempt::install_ticket(self.channel, authority, epoch, id));
        });
    }

    fn assert_no_end(&self, case: &str) {
        assert!(self.records().is_empty(), "{case} must remain Unknown");
        assert!(
            attempt::ticket(self.channel, &self.name).is_none(),
            "{case} cannot install a ticket"
        );
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs(body: impl FnOnce()) -> String {
    let logs = Logs::default();
    let copy = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .with_writer(move || copy.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, body);
    String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
}

#[test]
fn codex_clear_records_exact_end_after_marker_cleanup() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    assert_eq!(
        binding_context::observe_spawn_nonce_marker(&pane.name),
        binding_context::SpawnNonceMarker::Absent
    );
    let records = pane.records();
    assert_eq!(
        records.len(),
        1,
        "a confirmed Codex clear must durably record its exact old execution"
    );
    assert_eq!(records[0]["execution_nonce"], pane.context.execution_nonce);
    assert_eq!(records[0]["tmux_session"], pane.name);
    assert_eq!(records[0]["binding_seq"], 1);
    let canonical_root = fixture.logs.path().canonicalize().unwrap();
    assert_ne!(Some(&canonical_root), pane.context.provider_root.as_ref());
    assert_eq!(
        records[0]["provider_root"],
        serde_json::to_value(&canonical_root).unwrap()
    );
    assert_eq!(
        records[0]["context"],
        serde_json::to_value(&pane.context).unwrap(),
        "stored context retains raw immutable values"
    );
    let immutable =
        binding_context::execution_context("codex", &pane.context.execution_nonce).unwrap();
    assert_eq!(immutable, pane.context);
    let stored = codex_end::read(&crate::config::runtime_root().unwrap(), pane.channel).unwrap();
    assert_eq!(stored[0].context, ContextSnapshot::from(&immutable));
    assert_eq!(
        attempt::ticket(pane.channel, &pane.name).as_deref(),
        records[0]["end_id"].as_str()
    );
    let end = records[0].clone();
    attempt::reset_for_tests();
    assert_eq!(pane.records(), [end], "restart preserves the End file");
    assert!(
        attempt::ticket(pane.channel, &pane.name).is_none(),
        "restart never restores the volatile ticket"
    );
}

#[test]
fn explicit_clear_command_path_records_end_with_only_its_existing_kill() {
    const CHILD: &str = "ADK_CODEX_END_CLEAR_COMMAND_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let exact = format!(
            "{}::explicit_clear_command_path_records_end_with_only_its_existing_kill",
            module_path!().split_once("::").unwrap().1
        );
        let root = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .env("AGENTDESK_CONFIG", root.path().join("missing-config.yaml"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            result.status.success()
                && stdout.lines().any(|line| line
                    .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")),
            "{exact}: {}\n{stdout}\n{stderr}",
            result.status
        );
        eprintln!("{stdout}\n{stderr}");
        return;
    }
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.fake.take_calls();
    runtime().block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = serenity::ChannelId::new(pane.channel);
        super::super::clear_persist_failure_tests::seed_session(&shared, channel).await;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .channel_name = Some(pane.label.clone());
        let error = crate::services::discord::internal_api::lookup_dispatch_info("a1-no-context")
            .await
            .expect_err("the fresh child has no global API context");
        assert!(error.contains("context is unavailable"), "{error}");
        let report = super::super::clear_channel_session_state(
            &Arc::new(serenity::Http::new("")),
            &shared,
            &ProviderKind::Codex,
            channel,
            "!clear",
            super::super::SoftClearNotifyMode::Suppress,
        )
        .await
        .unwrap();
        assert!(!report.end_unconfirmed);
        assert!(
            shared
                .core
                .lock()
                .await
                .sessions
                .get(&channel)
                .unwrap()
                .cleared
        );
    });
    assert_eq!(
        pane.records().len(),
        1,
        "the actual !clear path reaches the End observer"
    );
    assert_eq!(
        pane.fake
            .take_calls()
            .iter()
            .filter(|call| call.starts_with("kill-session "))
            .count(),
        1,
        "the observer adds no kill effect"
    );
}

#[test]
fn unguarded_unacknowledged_and_live_pane_clears_record_nothing() {
    let fixture = Fixture::new();
    for case in ["unguarded", "lost_kill_ack", "live_pane"] {
        let pane = Pane::start(fixture.logs.path());
        match case {
            "lost_kill_ack" => pane.fake.lose_kill_ack_after_exit(),
            "live_pane" => pane.fake.keep_pane_alive(),
            _ => {}
        }
        let report = pane.end(case != "unguarded");
        if case == "lost_kill_ack" {
            assert!(report.end_unconfirmed);
        }
        pane.assert_no_end(case);
    }
}

#[test]
fn context_owner_root_host_and_source_mismatches_record_nothing() {
    let fixture = Fixture::new();
    for case in [
        "context_channel",
        "context_pane",
        "context_nonce",
        "context_provider",
        "host",
        "root",
        "owner",
        "missing_owner",
        "foreign_owner",
        "source_header",
        "source_inode",
        "wrong_event_nonce",
        "created_at",
        "first_prompt_digest",
    ] {
        let pane = Pane::start(fixture.logs.path());
        match case {
            "context_channel" => {
                pane.rewrite_context(|context| context.channel_id = Some(pane.channel + 1))
            }
            "context_pane" => {
                pane.rewrite_context(|context| context.tmux_session.push_str("-other"))
            }
            "context_nonce" => {
                pane.rewrite_context(|context| context.execution_nonce = "b".repeat(32))
            }
            "context_provider" => {
                pane.rewrite_context(|context| context.provider = "claude".into())
            }
            "host" => pane.rewrite_context(|context| context.host = Some("another-host".into())),
            "root" => {
                let elsewhere = fixture.logs.path().join("wrong-root");
                std::fs::create_dir_all(&elsewhere).unwrap();
                pane.rewrite_context(|context| {
                    context.provider_root = Some(elsewhere.canonicalize().unwrap())
                });
            }
            "owner" => {
                pane.rewrite_context(|context| context.owner_runtime_root.push_str("-other"))
            }
            "missing_owner" => {
                std::fs::remove_file(tc::session_temp_path(&pane.name, "owner")).unwrap();
            }
            "foreign_owner" => {
                std::fs::write(
                    tc::session_temp_path(&pane.name, "owner"),
                    "foreign-runtime",
                )
                .unwrap();
            }
            "source_header" => std::fs::write(
                &pane.source,
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"wrong\",\"source\":\"cli\"}}\n",
            )
            .unwrap(),
            "source_inode" => {
                let bytes = std::fs::read(&pane.source).unwrap();
                let replacement = pane.source.with_extension("replacement");
                std::fs::write(&replacement, bytes).unwrap();
                std::fs::rename(&replacement, &pane.source).unwrap();
            }
            "wrong_event_nonce" => pane.rewrite_event_nonce(&"d".repeat(32)),
            "created_at" => pane.rewrite_context(|context| {
                context.created_at += chrono::Duration::seconds(2);
            }),
            "first_prompt_digest" => {
                let mut body = std::fs::read_to_string(&pane.source).unwrap();
                body.push_str("{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"different input\"}}\n");
                std::fs::write(&pane.source, body).unwrap();
                pane.rewrite_context(|context| {
                    context.first_prompt_digest =
                        Some(format!("sha256:{:x}", Sha256::digest(b"launch input")));
                });
            }
            _ => unreachable!(),
        }
        let logs = capture_logs(|| {
            pane.end(true);
        });
        pane.assert_no_end(case);
        let reason = match case {
            "created_at" => Some("created_at before context"),
            "first_prompt_digest" => Some("first prompt mismatch"),
            _ => None,
        };
        if let Some(reason) = reason {
            assert!(
                logs.lines()
                    .any(|line| line.contains("Codex clear end not captured")
                        && line.contains(&format!("reason=\"{reason}\""))),
                "{case}: exact capture reason {reason:?} missing: {logs}"
            );
        }
    }
}

#[test]
fn unsupported_fresh_policies_record_no_end_and_explain_the_contract_boundary() {
    let fixture = Fixture::new();
    for policy in ["legacy", "verified"] {
        let pane = Pane::start(fixture.logs.path());
        pane.rewrite_context(|context| context.source_policy = Some(policy.into()));
        let logs = capture_logs(|| {
            pane.end(true);
        });
        pane.assert_no_end(policy);
        let warning = logs
            .lines()
            .find(|line| line.contains("Codex clear end not captured"))
            .unwrap_or_else(|| panic!("missing capture warning: {logs}"));
        assert!(
            warning.contains(&format!(
                "reason=\"fresh policy {policy} outside shadow contract\""
            )),
            "{warning}"
        );
    }
}

#[test]
fn resumed_same_source_can_end_when_unchanged_binding_keeps_an_older_nonce() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    let source = p5::records_strict(pane.channel).unwrap().unwrap()[0]
        .new
        .clone();
    let p5::BindingTarget::Source(source) = source else {
        panic!("settled source");
    };
    pane.rewrite_context(|context| {
        context.launch_mode = "resume".into();
        context.expected_native_session_id = Some(source.session_id.clone());
    });
    pane.rewrite_event_nonce(&"d".repeat(32));
    pane.end(true);
    let records = pane.records();
    assert_eq!(
        records.len(),
        1,
        "an Unchanged resume has no new binding nonce"
    );
    assert_eq!(records[0]["execution_nonce"], pane.context.execution_nonce);
    assert_eq!(records[0]["source"]["session_id"], source.session_id);
}

#[test]
fn a_binding_that_moves_during_clear_cannot_be_recorded_as_the_captured_end() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.fake.hold_kills();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !pane.fake.killing() {
                assert!(Instant::now() < deadline, "clear reaches its existing kill");
                std::thread::sleep(Duration::from_millis(10));
            }
            p5::set_test_root(Some(fixture.logs.path()));
            let next = fixture.logs.path().join("rebound.jsonl");
            std::fs::write(&next, "{}\n").unwrap();
            dedupe::register_launched_tmux_runtime_binding(&pane.name, binding(&next, "other"));
            p5::set_test_root(None);
            pane.fake.release_kills();
        });
        pane.end(true);
    });
    assert_eq!(p5::binding_events_since(pane.channel, 0).unwrap().len(), 2);
    pane.assert_no_end("rebound");
}

#[test]
fn an_immutable_context_changed_during_kill_invalidates_confirmation() {
    let fixture = Fixture::new();
    for move_alias in [false, true] {
        let pane = Pane::start(fixture.logs.path());
        pane.fake.hold_kills();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !pane.fake.killing() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                if move_alias {
                    let elsewhere = fixture.logs.path().join("moved-root");
                    std::fs::create_dir(&elsewhere).unwrap();
                    let alias = pane.context.provider_root.as_ref().unwrap();
                    std::fs::remove_file(alias).unwrap();
                    std::os::unix::fs::symlink(elsewhere, alias).unwrap();
                } else {
                    pane.rewrite_context(|context| {
                        context.first_prompt_digest = Some("changed".into())
                    });
                }
                pane.fake.release_kills();
            });
            pane.end(true);
        });
        pane.assert_no_end(if move_alias {
            "root alias moved after capture"
        } else {
            "context changed after capture"
        });
    }
}

#[test]
fn late_group_child_inside_wait_can_finish_but_surviving_group_is_unknown_and_diagnosed() {
    let fixture = Fixture::new();
    for (delay, positive) in [
        (Duration::from_millis(300), true),
        (Duration::from_secs(30), false),
    ] {
        let pane = Pane::start(fixture.logs.path());
        let mut child = FixtureProcess::start(pane.process.pid);
        assert_eq!(
            crate::services::process::process_group_probe(pane.process.pid),
            ProcessGroupProbe::Live
        );
        child.exit_after_pane_death(&pane.process, delay);
        let started = Instant::now();
        let logs = capture_logs(|| {
            pane.end(true);
        });
        if positive {
            assert_eq!(
                pane.records().len(),
                1,
                "the group is gone before the two-second boundary"
            );
            continue;
        }
        pane.assert_no_end("late group child");
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "the observer waits its full bound"
        );
        let warning = logs
            .lines()
            .find(|line| line.contains("Codex clear end not recorded"))
            .unwrap_or_else(|| panic!("missing structured warning: {logs}"));
        for expected in [
            " reason=",
            " pane=",
            " pid=",
            " pgid=",
            " pane_state=",
            " managed_pid=",
            " pid_state=",
            " pgid_state=",
        ] {
            assert!(
                warning.contains(expected),
                "{expected} missing from {warning}"
            );
        }
        assert!(
            warning.contains("reason=\"pane/pid/pgid not confirmed vacant\""),
            "{warning}"
        );
        assert!(warning.contains(&pane.name));
        assert!(warning.contains(&format!(" pid={}", pane.process.pid)));
        assert!(warning.contains(&format!(" pgid={}", pane.process.pid)));
        assert!(warning.contains("pgid_state=Live"), "{warning}");
    }
}

fn remote_dispatch(channel: Option<u64>, pane: Option<&str>) -> Result<(), String> {
    use crate::services::remote::{RemoteAuth, RemoteProfile};
    let _remote_tmux = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_REMOTE_TMUX",
        std::ffi::OsStr::new("true"),
    );
    let profile = RemoteProfile {
        name: "a1-policy-refusal".into(),
        host: "invalid.invalid".into(),
        port: 22,
        user: "fixture".into(),
        auth: RemoteAuth::KeyFile {
            path: "/dev/null".into(),
            passphrase: None,
        },
        default_path: "/private/tmp".into(),
        claude_path: None,
    };
    let (sender, _receiver) = std::sync::mpsc::channel();
    crate::services::codex::execute_command_streaming(
        "not sent",
        None,
        "/private/tmp",
        sender,
        None,
        None,
        None,
        Some(&profile),
        pane,
        None,
        channel,
        Some(ProviderKind::Codex),
        None,
        None,
        None,
        None,
        false,
    )
}

#[test]
fn actual_dispatch_takes_only_the_exact_panes_first_attempt_ticket() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    let ticket = attempt::ticket(pane.channel, &pane.name).expect("clear installs a ticket");
    let other = format!("{}-other", pane.name);
    assert!(
        remote_dispatch(Some(pane.channel), Some(&other))
            .unwrap_err()
            .contains("non-goal")
    );
    assert_eq!(
        attempt::ticket(pane.channel, &pane.name),
        Some(ticket.clone()),
        "a same-channel different pane cannot consume the ticket"
    );
    assert!(
        remote_dispatch(Some(pane.channel), None)
            .unwrap_err()
            .contains("disabled")
    );
    assert_eq!(
        attempt::ticket(pane.channel, &pane.name),
        Some(ticket),
        "headless calls do not touch the registry"
    );
    assert!(
        remote_dispatch(Some(pane.channel), Some(&pane.name))
            .unwrap_err()
            .contains("non-goal")
    );
    assert!(
        attempt::ticket(pane.channel, &pane.name).is_none(),
        "the first same-pane refused attempt consumes the ticket"
    );
    assert!(remote_dispatch(Some(pane.channel), Some(&pane.name)).is_err());
    assert!(
        attempt::ticket(pane.channel, &pane.name).is_none(),
        "a later attempt cannot recover a consumed ticket"
    );
}

#[test]
fn actual_local_fresh_direct_dispatch_consumes_ticket_before_safe_cli_failure() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.install_ticket("dispatch-fixture");
    let before = pane.records();
    assert!(before.is_empty());
    assert!(attempt::ticket(pane.channel, &pane.name).is_some());
    pane.fake.enable_fixture_launch();
    let _hosting_lock = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    struct Hosting(crate::config::Config);
    impl Drop for Hosting {
        fn drop(&mut self) {
            crate::services::provider_hosting::install_provider_hosting_config(&self.0);
        }
    }
    let previous = crate::config_live_reload::current()
        .map(|config| (*config).clone())
        .unwrap_or_default();
    let _hosting = Hosting(previous);
    let mut config = crate::config::Config::default();
    config.providers.insert(
        "codex".into(),
        crate::config::ProviderConfig {
            tui_hosting: Some(true),
            runtime: Some("tui".into()),
            ..Default::default()
        },
    );
    crate::services::provider_hosting::install_provider_hosting_config(&config);
    use crate::services::codex::{CodexSourceMode, SOURCE_MODE_TEST};
    struct SourceMode(Option<CodexSourceMode>);
    impl Drop for SourceMode {
        fn drop(&mut self) {
            SOURCE_MODE_TEST.with(|mode| mode.set(self.0));
        }
    }
    let _mode =
        SourceMode(SOURCE_MODE_TEST.with(|mode| mode.replace(Some(CodexSourceMode::Shadow))));
    let _codex_home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CODEX_HOME",
        fixture.logs.path(),
    );
    let cancel = Arc::new(CancelToken::new());
    cancel
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut result = None;
    let logs = capture_logs(|| {
        let (sender, _receiver) = std::sync::mpsc::channel();
        result = Some(crate::services::codex::execute_command_streaming(
            "first fresh instruction",
            None,
            fixture.logs.path().to_str().unwrap(),
            sender,
            None,
            None,
            Some(cancel),
            None,
            Some(&pane.name),
            None,
            Some(pane.channel),
            Some(ProviderKind::Codex),
            None,
            None,
            None,
            None,
            true,
        ));
    });
    let error = result
        .unwrap()
        .expect_err("the fixture's explicitly missing CLI cannot launch");
    assert!(error.contains("Codex CLI not found"), "{error}");
    assert!(
        logs.contains("runtime_kind=\"direct-tui\""),
        "the actual DirectTui branch must run: {logs}"
    );
    assert!(
        attempt::ticket(pane.channel, &pane.name).is_none(),
        "a failed first fresh dispatch consumes the ticket"
    );
    assert_eq!(
        pane.records(),
        before,
        "dispatch cannot seal or rewrite the End in A1"
    );
}

#[test]
fn actual_same_pane_dispatch_during_clear_preserves_end_but_invalidates_its_ticket() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.fake.hold_kills();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !pane.fake.killing() {
                assert!(Instant::now() < deadline, "clear reaches its existing kill");
                std::thread::sleep(Duration::from_millis(10));
            }
            let result = remote_dispatch(Some(pane.channel), Some(&pane.name));
            pane.fake.release_kills();
            assert!(result.unwrap_err().contains("non-goal"));
        });
        pane.end(true);
    });
    assert_eq!(
        pane.records().len(),
        1,
        "the old execution's End remains a fact"
    );
    assert!(
        attempt::ticket(pane.channel, &pane.name).is_none(),
        "the actual dispatch changed the captured epoch before confirm"
    );
}

#[test]
fn actual_clear_published_unsynced_end_never_receives_a_ticket() {
    use crate::services::tui_o::store::fault::{self, Step};
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    let dir = crate::config::runtime_root().unwrap().join("o_codex_ends");
    let sync = fault::plant(
        &dir,
        Step::ReplaceDirSync,
        std::io::ErrorKind::Other,
        Some(1),
    );
    let logs = capture_logs(|| {
        pane.end(true);
    });
    drop(sync);
    assert_eq!(
        pane.records().len(),
        1,
        "rename published the End before the sync failed"
    );
    assert!(attempt::ticket(pane.channel, &pane.name).is_none());
    let warning = logs
        .lines()
        .find(|line| line.contains("Codex clear end not recorded"))
        .unwrap_or_else(|| panic!("missing warning: {logs}"));
    assert!(
        warning.contains(" reason=\"End published unsynced:"),
        "{warning}"
    );
}

#[test]
fn an_attempt_active_before_clear_prevents_a_ticket_and_later_attempts_change_the_epoch() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    let guard = attempt::begin(Some(pane.channel), Some(&pane.name));
    pane.end(true);
    assert_eq!(
        pane.records().len(),
        1,
        "a racing dispatch does not erase an observed End"
    );
    assert!(
        attempt::ticket(pane.channel, &pane.name).is_none(),
        "a possible late spawn cannot receive the End ticket"
    );
    drop(guard);
    pane.install_ticket("after-finished-attempt");
    let before =
        tc::with_tmux_source_authority(&pane.name, |a| attempt::generation(pane.channel, a));
    let next = attempt::begin(Some(pane.channel), Some(&pane.name));
    let after =
        tc::with_tmux_source_authority(&pane.name, |a| attempt::generation(pane.channel, a));
    assert!(after > before);
    assert!(attempt::ticket(pane.channel, &pane.name).is_none());
    drop(next);
    let no_pane = attempt::begin(Some(pane.channel), None);
    assert_eq!(
        tc::with_tmux_source_authority(&pane.name, |a| attempt::generation(pane.channel, a)),
        after
    );
    drop(no_pane);
}

mod capture_fold_tests;
mod successor_tests;
