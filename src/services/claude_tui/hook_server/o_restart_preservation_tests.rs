//! A Claude channel the O writer hosts keeps its output across a host restart and a hook source
//! switch with Legacy's idle body task never started in the process.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::WriterAlarm;
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::adoption::{LegacyCursor, LegacyView};
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::test_io::{Alarms, AnyLease, Posts, TestHost};
use crate::services::tui_o::writer::host::{self, Custody, HostIo, HostParts, Readiness};
use std::sync::atomic::{AtomicUsize, Ordering};

const CHANNEL: u64 = 7_530;

/// The gateway stand-in of the host tests, reading the channel's real binding log.
struct LoggedHost(Arc<TestHost>);

impl HostIo for LoggedHost {
    type Port = Posts;
    type Lease = AnyLease;
    type Alarms = Alarms;
    type Bindings = ChannelBindingLog;

    fn port(&self) -> impl Future<Output = Arc<Posts>> + Send {
        self.0.port()
    }

    fn lease(&self) -> AnyLease {
        self.0.lease()
    }

    fn alarms(&self) -> Alarms {
        self.0.alarms()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        Arc::new(ChannelBindingLog::new(channel, provider))
    }

    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.0.activation_facts(channel, provider)
    }

    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.0.local_custody(channel, provider)
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.0.legacy()
    }

    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.0.legacy_busy(channel)
    }

    fn relaying(&self, channel: u64) -> bool {
        self.0.relaying(channel)
    }
}

/// Counts every question the host asks of Legacy's relay state.
struct Asked(Arc<AtomicUsize>);

impl LegacyView for Asked {
    fn started(&self) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        false
    }
    fn cursor(&self, _: &str) -> LegacyCursor {
        self.0.fetch_add(1, Ordering::SeqCst);
        LegacyCursor::Unbound
    }
    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
    fn tail_running(&self, _: &str) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        false
    }
}

fn body(id: &str, text: &str) -> Vec<u8> {
    let row = serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "message": {"id": id, "content": [{"type": "text", "text": text}]},
    });
    let mut line = serde_json::to_vec(&row).unwrap();
    line.push(b'\n');
    line
}

fn append(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

/// Whether Legacy's idle body task ever started here: its spawn flips this flag first.
fn legacy_body_task_started() -> bool {
    super::super::super::CLAUDE_IDLE_TRANSCRIPT_RELAY_STARTED.load(Ordering::SeqCst)
}

#[test]
fn a_restarted_o_writer_sends_owed_once_and_follows_a_hook_switch_without_legacy() {
    let name =
        "o_restart::a_restarted_o_writer_sends_owed_once_and_follows_a_hook_switch_without_legacy";
    if !in_child(name) {
        return;
    }
    assert!(!legacy_body_task_started(), "[legacy:absent] at the start");
    let (root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
    let ingress = Ingress::new();
    let _reset = Reset;
    let race = RegistrationRace::new(root.path(), CHANNEL);
    let dir = ingress_transcript_dir(&race);
    let (a_path, b_path) = (
        dir.join(format!("{}.jsonl", race.a)),
        dir.join(format!("{}.jsonl", race.b)),
    );
    // A new pane: its launch transcript holds no bytes yet, so O activates without Legacy.
    std::fs::write(&a_path, b"").unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    rehydrate_existing_claude_tui_bindings(&shared);
    let bound = || dedupe::runtime_binding_for_tmux_session(&race.tmux).and_then(|b| b.session_id);
    assert_eq!(bound(), Some(race.a.clone()), "the restart pass bound A");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let polls = |count: u32| {
        let wait = crate::services::tui_o::writer::actor::POLL_INTERVAL * count;
        rt.block_on(async { tokio::time::sleep(wait).await });
    };
    let _candidates = crate::services::tui_o::cutover::test_override::force_candidates(&[(
        CHANNEL,
        RuntimeHandoffKind::ClaudeTui,
    )]);
    let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());
    gate.acquired();
    let io = TestHost::new([]);
    let asked = Arc::new(AtomicUsize::new(0));
    *io.legacy.lock().unwrap() = Some(Arc::new(Asked(Arc::clone(&asked))));
    let start = || {
        let parts = || HostParts {
            io: Arc::new(LoggedHost(Arc::clone(&io))),
            runtime_root: Some(root.path().to_path_buf()),
            gate: Arc::clone(&gate),
            readiness: Arc::new(Readiness::default()),
        };
        rt.block_on(async { host::start(ShadowProvider::Claude, true, parts) })
    };
    let posts = || io.posts.to(CHANNEL);
    let count = |text: &str| posts().iter().filter(|post| *post == text).count();
    let (a, b, c) = ("A sent", "B owed", "C switched");

    let tasks = start();
    polls(3);
    append(&a_path, &body("m1", a));
    polls(3);
    assert_eq!(posts(), [a], "A is sent before the restart");

    // B lands while the gate is lost, so it is still owed when dcserver restarts.
    gate.lost();
    append(&a_path, &body("m2", b));
    polls(3);
    assert_eq!(posts(), [a], "B waits while the gate is lost");
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
    polls(2);
    // The restarted process lost the pane's binding; the restart pass registers it again.
    gate.acquired();
    dedupe::reset_state_for_tests();
    dedupe::clear_claude_session_rotation(&race.tmux);
    forget_channel_for_tests(CHANNEL);
    rehydrate_existing_claude_tui_bindings(&shared);
    assert_eq!(
        bound(),
        Some(race.a.clone()),
        "the restart pass bound A again"
    );
    let tasks = start();
    polls(6);
    assert_eq!(
        count(a),
        1,
        "[A:no_repost] the restart re-posts nothing already sent"
    );
    assert_eq!(
        count(b),
        1,
        "[B:once] the owed body goes once after the restart"
    );

    // The pane's /clear reaches the real hook ingress and moves the binding to B's transcript.
    let switched = ingress.claude_hook("SessionStart", &race.a, &race.payload, Some(&uuid()));
    assert_eq!(switched, 202, "the clear hook is durable");
    assert_eq!(bound(), Some(race.b.clone()), "the hook moved the binding");
    polls(3);
    append(&b_path, &body("n1", c));
    polls(6);
    assert_eq!(count(c), 1, "[C:once] the switched source's body goes once");
    assert_eq!(posts(), [a, b, c], "every body once, in order");

    let alarms = io.alarms.0.lock().unwrap().clone();
    let halted = |(_, alarm): &(u64, WriterAlarm)| matches!(alarm, WriterAlarm::Halted { .. });
    assert!(!alarms.iter().any(halted), "{alarms:?}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "[legacy:unasked] no Legacy relay state read"
    );
    assert!(
        !legacy_body_task_started(),
        "[legacy:absent] the idle body task never started"
    );
    let tails = super::super::super::CLAUDE_IDLE_RESPONSE_TAILS
        .lock()
        .unwrap();
    assert!(
        tails.is_empty(),
        "[legacy:absent] no Legacy response tail ran"
    );
    drop(tails);
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
}
