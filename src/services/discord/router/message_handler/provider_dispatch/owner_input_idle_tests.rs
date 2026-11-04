//! Owned fronts into an idle Claude pane through the drive and the provider's own observer, whose
//! transcript is found as the live path finds it: by the account's home or the pane's binding.
use super::*;
use crate::services::agent_protocol::StreamMessage;
use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, register_tmux_runtime_binding};
use std::io::Write;
use std::sync::mpsc::Sender;

use crate::services::claude_tui::input::submission::tests::wait;
use crate::services::discord::router::message_handler::provider_dispatch::{
    CLAUDE_LEGACY_PROBE, ClaudeLegacyProbe,
};

const ASSISTANT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer {n}"}]}}"#;

/// Each Enter the pane records gets its AI turn: an answer, then the turn's end.
fn answers(rig: &Rig) -> std::thread::JoinHandle<()> {
    let (renderer, transcript) = (rig.renderer.clone(), rig.transcript());
    std::thread::spawn(move || {
        for n in 1..=2 {
            let entered = || events(&renderer).iter().filter(|e| *e == "Enter").count() >= n;
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !entered() {
                assert!(std::time::Instant::now() < deadline, "Enter {n} never came");
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            let answer = ASSISTANT.replace("{n}", &n.to_string());
            writeln!(file, "{answer}\n{}", IDLE.trim_end()).unwrap();
        }
    })
}

/// The pane's runtime binding naming `session`, as the relay rehydrates it after each turn ends.
fn rehydrate(rig: &Rig, session: &str) {
    let binding = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: rig.transcript().display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_string()),
        last_offset: 0,
        relay_last_offset: None,
    };
    register_tmux_runtime_binding(&rig.name, binding);
}

/// A Discord turn on the rig's pane for `front`, resuming `session` from a dir without its transcript;
/// `driver` stands in for the provider's driver when the turn launches.
fn idle_turn(
    rig: &Rig,
    (session, dir): (&str, &Path),
    (front, settled): (Front, Settled),
    driver: impl FnOnce(&Sender<StreamMessage>) -> Option<ClaudeLegacyProbe>,
) -> (Result<(), String>, Vec<StreamMessage>) {
    let claim = rig.claim(&front).expect("granted front");
    let owner = OwnerTurn {
        mailbox: rig.mailbox.clone(),
        claim,
        settled,
        origin: InputOrigin::Discord,
    };
    let (host, provider) = (TurnHost::Tmux, ProviderKind::Claude);
    let turn = StreamingTurn {
        pool: None,
        shared: None,
        provider: &provider,
        prompt: &front.1.text,
        session_id: Some(session),
        working_dir: dir.to_str().unwrap(),
        system_prompt: None,
        allowed_tools: &[],
        cancel: Arc::new(CancelToken::new()),
        codex_observation: None,
        tmux_session_name: Some(&rig.name),
        teardown: None,
        host: &host,
        channel_id: rig.channel,
        model: None,
        native_fast_mode: None,
        effort: None,
        codex_goals: None,
        compact_percent: None,
        compact_lower_bound_tokens: 0,
        compact_token_limit: None,
        cache_ttl_minutes: None,
        dispatch_type: None,
        force_fresh: false,
        submission: None,
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    if let Some(driver) = driver(&sender) {
        *CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((rig.channel, driver));
    }
    let _entered = rig.runtime.enter();
    let result = execute_owned(turn, sender, owner);
    (result, receiver.try_iter().collect())
}

/// After a person's turn ends, one wake hands two queued inputs to an idle pane whose transcript only
/// its binding names: in order, each pasted, entered and answered once, then nothing more.
#[test]
fn an_idle_pane_takes_each_queued_front_once_through_its_own_observer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-idle-d2", 7_272_595, "normal", view("", false), BUSY);
    let second: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":rig.original.message_id + 1,"channel_id":rig.channel,
        "source_message_ids":[rig.original.message_id + 1],"queued_generation":1,
        "text":"second question"}))
    .unwrap();
    let admitted = admit(&rig.mailbox, &second, AdmissionTicket::new());
    rig.runtime.block_on(admitted);
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    let (fronts, inbox) = std::sync::mpsc::channel::<(Front, Settled)>();
    let fronts = std::sync::Mutex::new(fronts);
    let idle: Handoff = Arc::new(move |front| {
        let (settled, answer) = tokio::sync::oneshot::channel();
        let sent = fronts.lock().unwrap().send((front, settled));
        Box::pin(async move {
            sent.map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, rig.channel),
        idle,
    );
    // The person's turn ends; the supervisor's boundary wake is the one event.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(rig.transcript())
        .unwrap();
    writeln!(file, "{}", IDLE.trim_end()).unwrap();
    let answering = answers(&rig);
    {
        let _entered = rig.runtime.enter();
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
    }
    let mut handed = Vec::new();
    let mut said = Vec::new();
    while let Ok((front, settled)) = inbox.recv_timeout(Duration::from_secs(15)) {
        handed.push(front.1.text.clone());
        rehydrate(&rig, &session);
        let at = (session.as_str(), elsewhere.path());
        let (result, sent) = idle_turn(&rig, at, (front, settled), |_| None);
        assert_eq!(result, Ok(()), "{handed:?}");
        said.extend(sent.into_iter().filter_map(|m| match m {
            StreamMessage::Text { content } => Some(content),
            _ => None,
        }));
        if handed.len() == 2 {
            break;
        }
    }
    answering.join().unwrap();
    assert_eq!(handed, [PROMPT, "second question"], "in order, each once");
    let events = rig.events();
    let count = |key: &str| events.iter().filter(|e| *e == key).count();
    assert_eq!((count("paste"), count("Enter")), (2, 2), "{events:?}");
    let answered = |n: &str| said.iter().filter(|text| text.contains(n)).count();
    assert_eq!(
        (answered("answer 1"), answered("answer 2")),
        (1, 1),
        "{said:?}"
    );
    assert!(
        inbox.recv_timeout(Duration::from_secs(3)).is_err(),
        "nothing more is handed on"
    );
    let snapshot = rig.snapshot();
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

struct RestoreObserverRegistry(Option<Arc<crate::services::discord::health::HealthRegistry>>);

impl Drop for RestoreObserverRegistry {
    fn drop(&mut self) {
        let restored = self
            .0
            .take()
            .unwrap_or_else(|| Arc::new(crate::services::discord::health::HealthRegistry::new()));
        crate::engine::ops::turn_ops::set_global_health_registry(&restored);
    }
}

struct AttachedOwner {
    rig: Rig,
    shared: Arc<crate::services::discord::SharedData>,
    home: std::path::PathBuf,
    drive: Arc<Drive>,
    fronts: Inbox,
    passes: std::sync::mpsc::Receiver<()>,
    wakes: Arc<std::sync::atomic::AtomicUsize>,
}

impl AttachedOwner {
    fn new(name: &str, channel: u64, transcript: &str) -> Self {
        let mut rig = rig(name, channel, "normal", view("", false), transcript);
        let entered = rig.runtime.enter();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let mailbox = shared.mailbox(poise::serenity_prelude::ChannelId::new(channel));
        drop(entered);
        let path = crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude/input-owner-attached")
            .join(format!("{channel}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        rig.runtime.block_on(async {
            mailbox
                .v2_adopt(path, "attached-owner".into())
                .await
                .unwrap();
            let accepted = admit(&mailbox, &rig.original, rig.ticket.clone()).await;
            assert!(matches!(
                accepted,
                crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
            ));
        });
        rig.mailbox = mailbox;
        let (drive, fronts) = rig.drive();
        let (finished, passes) = std::sync::mpsc::channel();
        let settled = Arc::new(std::sync::Mutex::new(finished));
        let drive = Arc::new(Drive {
            mailbox: drive.mailbox.clone(),
            channel,
            handoff: drive.handoff.clone(),
            settled: Some(Arc::new(move || {
                settled.lock().unwrap().send(()).unwrap();
                Box::pin(async {})
            })),
        });
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (wake_drive, counted) = (drive.clone(), wakes.clone());
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_KICKOFF_FOR_TESTS
            .lock()
            .unwrap()
            .insert(
                channel,
                Arc::new(move || {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    crate::services::discord::queue_io::input_owner::drive::wake(
                        wake_drive.clone(),
                    );
                }),
            );
        let home = rig.f.root.path().join("observer-home");
        Self {
            rig,
            shared,
            home,
            drive,
            fronts,
            passes,
            wakes,
        }
    }

    fn launch_source(&self) -> (String, std::path::PathBuf) {
        let session = uuid::Uuid::new_v4().to_string();
        let project = self.rig.f.root.path().join("observer-project");
        fs::create_dir_all(&project).unwrap();
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            &project,
            &session,
            Some(&self.home),
        )
        .unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(self.rig.transcript(), &path).unwrap();
        let context = crate::config::runtime_root()
            .unwrap()
            .join("runtime/binding_contexts/claude")
            .join(format!("{}.json", self.rig.nonce));
        let script = crate::services::tmux_common::session_temp_path(
            &self.rig.name,
            crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        );
        fs::write(script, format!(
            "export AGENTDESK_BINDING_CONTEXT='{}'\ncd '{}'\nexec 'claude' '--session-id' '{session}'\n",
            context.display(), project.display(),
        )).unwrap();
        (session, path)
    }

    fn attach(&self) -> bool {
        let _entered = self.rig.runtime.enter();
        let pass = crate::services::discord::tui_prompt_relay::rehydrate_owner_pane_for_tests(
            &self.shared,
            &self.rig.name,
            self.rig.channel,
            &self.home,
        );
        let spawned = pass.is_some();
        if let Some(pass) = pass {
            self.rig.runtime.block_on(pass).unwrap();
        }
        spawned
    }

    fn serve(&self) -> Result<(), String> {
        self.serve_before(|| {})
    }

    fn serve_before(&self, before_refusal: impl FnOnce()) -> Result<(), String> {
        let (front, settled) = self
            .fronts
            .recv_timeout(Duration::from_secs(10))
            .expect("the owner's drive hands the front once");
        let claim = self.rig.claim(&front).expect("the real front claim");
        let provider = ProviderKind::Claude;
        let mut owner = self.rig.registration(
            &provider,
            target::Phase::Idle,
            &self.rig.transcript(),
            Execution::LocalTui,
        );
        // The provider builds its real observer; only reading the ensuing AI turn is immediate.
        let observed = {
            let _entered = self.rig.runtime.enter();
            crate::services::claude_tui::hosting::owner_observer(
                (&self.rig.name, &front.1.text, self.rig.channel),
                (None, self.rig.f.root.path()),
                std::sync::mpsc::channel().0,
                Arc::new(CancelToken::new()),
            )
        };
        owner.until_done = observed.map(|_| Box::new(|| Ok(())) as super::super::UntilDone);
        before_refusal();
        self.rig
            .with_turn(&provider, Some(&self.rig.name), |turn, sender| {
                execute_front(turn, sender, claim, owner, settled)
            })
    }

    fn settled(&self) {
        self.finished_pass();
        assert!(self.fronts.try_recv().is_err(), "no additional handoff");
    }

    fn finished_pass(&self) {
        self.passes
            .recv_timeout(Duration::from_secs(10))
            .expect("the drive's settled callback completes the pass");
    }

    fn wakes(&self) -> usize {
        self.wakes.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn keys(&self) -> (usize, usize) {
        let trace = self
            .rig
            .f
            .root
            .path()
            .join(format!("{}-owner/trace", self.rig.name));
        let trace = fs::read_to_string(trace).unwrap_or_default();
        let paste = trace
            .lines()
            .filter(|line| line.contains("paste-buffer"))
            .count();
        let enter = trace
            .lines()
            .filter(|line| line.contains("send-keys") && line.contains(" Enter"))
            .count();
        (paste, enter)
    }
}

impl Drop for AttachedOwner {
    fn drop(&mut self) {
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_KICKOFF_FOR_TESTS
            .lock()
            .unwrap()
            .remove(&self.rig.channel);
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_WAITING_FOR_TESTS
            .lock()
            .unwrap()
            .remove(&self.rig.channel);
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&self.rig.name);
    }
}

/// A source attaching after an idle refusal is the one new event; it delivers the same front once.
#[test]
fn an_observer_attaching_after_idle_refusal_wakes_the_owner_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-late", 7_676_601, IDLE);
    let _trace = owner.rig.trace_driver();
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0), "the refusal sent no keys");
    assert!(owner.rig.snapshot()["head_injection"].is_null());
    assert_eq!(owner.wakes(), 0);

    owner.launch_source();
    assert!(
        owner.attach(),
        "the published idle source schedules its owner check"
    );
    assert_eq!(
        owner.wakes(),
        1,
        "one attachment produces exactly one wake; keys={:?}",
        owner.keys()
    );
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1), "one paste and one Enter");
    owner.rig.delivered();
    assert!(!owner.attach(), "an unchanged binding adds no wake");
    assert_eq!(owner.wakes(), 1);
}

/// The common publication event reaches the registered runtime without the rehydrate fallback.
#[test]
fn a_published_observer_wakes_its_registered_owner_once() {
    use crate::engine::ops::turn_ops::{health_registry, set_global_health_registry};
    use crate::services::discord::health::HealthRegistry;

    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-published", 7_676_605, IDLE);
    let _trace = owner.rig.trace_driver();
    let registry = Arc::new(HealthRegistry::new());
    owner
        .rig
        .runtime
        .block_on(registry.register("claude".into(), owner.shared.clone()));
    let _restore = RestoreObserverRegistry(health_registry());
    set_global_health_registry(&registry);
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0));

    owner.launch_source();
    assert!(
        !owner.attach(),
        "the registered runtime uses the publication event"
    );
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.wakes(), 1, "the common publication wakes once");
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();
}

/// A source the owner already observed adds no wake on reattachment or after the front settles.
#[test]
fn an_existing_observer_and_an_empty_front_add_no_attachment_wake() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-known", 7_676_602, IDLE);
    let _trace = owner.rig.trace_driver();
    let session = uuid::Uuid::new_v4().to_string();
    rehydrate(&owner.rig, &session);
    assert!(!owner.attach(), "the existing source is unchanged");
    assert_eq!(owner.wakes(), 0);
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();

    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&owner.rig.name);
    owner.launch_source();
    assert!(owner.attach(), "an attachment checks the now-empty owner");
    assert_eq!(owner.wakes(), 0, "no front means no wake");
    assert!(owner.fronts.try_recv().is_err());
}

/// Attaching to a busy source waits for that turn's existing Idle transition.
#[test]
fn a_busy_source_attachment_does_not_wake_the_owner() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-busy", 7_676_603, BUSY);
    owner.launch_source();
    assert!(!owner.attach(), "busy source adds no idle-submit event");
    assert_eq!(owner.wakes(), 0);
    assert!(owner.fronts.try_recv().is_err());
    assert_eq!(owner.keys(), (0, 0));
}

/// An attachment while the refused handoff is still outstanding coalesces into its next pass.
#[test]
fn an_observer_attaching_during_the_idle_refusal_preserves_its_wake() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-race", 7_676_604, IDLE);
    let _trace = owner.rig.trace_driver();
    owner.rig.wake(&owner.drive);
    owner.launch_source();
    let refused = owner.serve_before(|| {
        assert!(owner.attach());
        assert_eq!(owner.wakes(), 1);
    });
    assert_eq!(refused, Err("NotSent(\"idle_unobserved\")".into()));
    owner.finished_pass();
    assert_eq!(owner.keys(), (0, 0));
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1));
    assert_eq!(owner.wakes(), 1);
    owner.rig.delivered();
}

/// An idle source attaching before the refused turn releases its token wakes after that release.
#[test]
fn an_attached_observer_waits_for_the_refused_turn_token_to_finish() {
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    use poise::serenity_prelude::{MessageId, UserId};

    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-token", 7_676_606, IDLE);
    let _trace = owner.rig.trace_driver();
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0));

    let token = Arc::new(CancelToken::new());
    assert!(owner.rig.runtime.block_on(owner.rig.mailbox.try_start_turn(
        token.clone(),
        UserId::new(7),
        MessageId::new(9),
    )));
    let (waiting, observed_wait) = std::sync::mpsc::channel();
    let waiting = std::sync::Mutex::new(waiting);
    crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_WAITING_FOR_TESTS
        .lock()
        .unwrap()
        .insert(
            owner.rig.channel,
            Arc::new(move || {
                waiting.lock().unwrap().send(()).unwrap();
            }),
        );
    owner.launch_source();
    let pending = {
        let _entered = owner.rig.runtime.enter();
        crate::services::discord::tui_prompt_relay::rehydrate_owner_pane_for_tests(
            &owner.shared,
            &owner.rig.name,
            owner.rig.channel,
            &owner.home,
        )
        .expect("the attached source checks its active owner")
    };
    observed_wait
        .recv_timeout(Duration::from_secs(10))
        .expect("the attachment observed the active token before its finish latch wait");
    assert!(
        owner
            .rig
            .runtime
            .block_on(owner.rig.mailbox.has_active_turn())
            .unwrap()
    );
    assert_eq!(
        owner.wakes(),
        0,
        "the active turn has not released its token"
    );
    let released =
        owner
            .rig
            .runtime
            .block_on(owner.rig.mailbox.finish_turn(QueuePersistenceContext::new(
                &ProviderKind::Claude,
                "attached-owner",
                None,
            )));
    assert!(
        released
            .removed_token
            .is_some_and(|removed| Arc::ptr_eq(&removed, &token))
    );
    owner.rig.runtime.block_on(pending).unwrap();
    assert_eq!(owner.wakes(), 1);
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();
}

/// Resolving a previously missing exact source wakes once even though its binding key is unchanged.
#[test]
fn a_pending_observer_becoming_readable_wakes_after_the_refused_token_finishes() {
    pending_observer_becomes_readable(false);
}

/// A hook verifying the held exact source emits the same one owner-readiness event.
#[test]
fn a_hook_verifying_a_pending_observer_wakes_after_the_refused_token_finishes() {
    pending_observer_becomes_readable(true);
}

fn pending_observer_becomes_readable(via_hook: bool) {
    use crate::engine::ops::turn_ops::{health_registry, set_global_health_registry};
    use crate::services::discord::health::HealthRegistry;
    use crate::services::tui_prompt_dedupe::binding_events::{
        BindingTarget, CauseSource, HookSignal, PendingRecord, Proposal,
    };
    use crate::services::tui_prompt_dedupe::pending::{PendingRestore, last_restore_outcome};
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    use poise::serenity_prelude::{MessageId, UserId};

    struct BindingLogScope(Option<std::path::PathBuf>, u64);
    impl Drop for BindingLogScope {
        fn drop(&mut self) {
            binding_events::forget_channel_for_tests(self.1);
            binding_events::set_test_root(self.0.as_deref());
        }
    }

    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_676_607 + u64::from(via_hook);
    let name = if via_hook {
        "oi-attach-hook"
    } else {
        "oi-attach-pending"
    };
    let _binding_log = BindingLogScope(binding_events::test_root(), channel);
    binding_events::set_test_root(crate::config::runtime_root().as_deref());
    let owner = AttachedOwner::new(name, channel, IDLE);
    let _trace = owner.rig.trace_driver();
    let registry = Arc::new(HealthRegistry::new());
    owner
        .rig
        .runtime
        .block_on(registry.register("claude".into(), owner.shared.clone()));
    let _restore = RestoreObserverRegistry(health_registry());
    set_global_health_registry(&registry);
    let (session, path) = owner.launch_source();
    let command_session = if via_hook {
        let command = uuid::Uuid::new_v4().to_string();
        let script = crate::services::tmux_common::session_temp_path(
            &owner.rig.name,
            crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        );
        let old = fs::read_to_string(&script).unwrap();
        let launch = format!("'--session-id' '{session}'");
        assert!(old.contains(&launch));
        fs::write(
            &script,
            old.replacen(&launch, &format!("'--session-id' '{command}'"), 1),
        )
        .unwrap();
        command
    } else {
        session.clone()
    };
    fs::remove_file(&path).unwrap();
    let missing = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.clone()),
        last_offset: 0,
        relay_last_offset: None,
    };
    let proposal = Proposal::for_binding(
        Some(owner.rig.channel),
        &owner.rig.name,
        &missing,
        None,
        CauseSource::Observed,
    )
    .unwrap();
    assert_eq!(
        binding_events::record_pending(&proposal).unwrap(),
        PendingRecord::Recorded
    );
    assert!(
        !owner.attach(),
        "a missing exact source cannot start an observer"
    );
    let held = dedupe::runtime_binding_for_tmux_session(&owner.rig.name)
        .expect("the durable Pending publishes its still-missing exact binding");
    assert_eq!(held.output_path, missing.output_path);
    assert_eq!(
        dedupe::resolve_tmux_session_name("claude", &command_session).as_deref(),
        Some(owner.rig.name.as_str()),
        "the real restore kept the launch command's routing alias",
    );
    assert!(matches!(
        last_restore_outcome(&owner.rig.name),
        Some(PendingRestore::BoundFromLedger {
            exact_wait: Some(_),
            ..
        })
    ));
    assert_eq!(owner.wakes(), 0);

    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0));
    let token = Arc::new(CancelToken::new());
    assert!(owner.rig.runtime.block_on(owner.rig.mailbox.try_start_turn(
        token.clone(),
        UserId::new(7),
        MessageId::new(9),
    )));
    let (waiting, observed_wait) = std::sync::mpsc::channel();
    let waiting = std::sync::Mutex::new(waiting);
    crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_WAITING_FOR_TESTS
        .lock()
        .unwrap()
        .insert(
            owner.rig.channel,
            Arc::new(move || {
                waiting.lock().unwrap().send(()).unwrap();
            }),
        );
    // The same exact path now opens on its own native first record; its source key stays unchanged.
    fs::write(
        owner.rig.transcript(),
        format!("{{\"type\":\"mode\",\"sessionId\":\"{session}\"}}\n{IDLE}"),
    )
    .unwrap();
    std::os::unix::fs::symlink(owner.rig.transcript(), &path).unwrap();
    if via_hook {
        let hook = HookSignal::from_payload("stop", &json!({"transcript_path":path}));
        let adopted = {
            let _entered = owner.rig.runtime.enter();
            dedupe::adopt_claude_continuation_explained(&command_session, &session, &hook).unwrap()
        };
        assert_eq!(
            adopted,
            (
                Some((owner.rig.name.clone(), path.display().to_string())),
                None
            )
        );
    } else {
        assert!(
            !owner.attach(),
            "the registered runtime uses the resolved-source event"
        );
    }
    let ready = dedupe::runtime_binding_for_tmux_session(&owner.rig.name).unwrap();
    let source = |binding: &TuiRuntimeBinding| {
        (
            binding.runtime_kind,
            binding.session_id.clone(),
            binding.output_path.clone(),
        )
    };
    assert_eq!(
        source(&ready),
        source(&held),
        "readability changed without a new binding key"
    );
    match last_restore_outcome(&owner.rig.name) {
        Some(PendingRestore::BoundFromLedger { exact_wait, .. }) => {
            assert_eq!(
                exact_wait.is_some(),
                via_hook,
                "the hook resolves the durable log without running the restore pass"
            );
        }
        other => panic!("the held source's restore outcome changed: {other:?}"),
    }
    let resolved = || {
        binding_events::records_strict(owner.rig.channel)
            .unwrap()
            .unwrap()
            .into_iter()
            .filter(|event| {
                matches!(&event.new, BindingTarget::Resolved { source, .. }
            if source.session_id == session)
            })
            .count()
    };
    assert_eq!(resolved(), 1, "the durable Pending resolved once");
    assert_eq!(owner.wakes(), 0, "the old turn token is still held");
    assert!(
        observed_wait.recv_timeout(Duration::from_secs(10)).is_ok(),
        "ready source stranded; wake_count={} keys={:?}",
        owner.wakes(),
        owner.keys(),
    );
    let released =
        owner
            .rig
            .runtime
            .block_on(owner.rig.mailbox.finish_turn(QueuePersistenceContext::new(
                &ProviderKind::Claude,
                "attached-owner",
                None,
            )));
    assert!(
        released
            .removed_token
            .is_some_and(|removed| Arc::ptr_eq(&removed, &token))
    );
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.wakes(), 1);
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();
    assert!(
        !owner.attach(),
        "a settled exact source does not resolve again"
    );
    assert_eq!(resolved(), 1);
    assert_eq!(owner.wakes(), 1);
    assert!(matches!(
        last_restore_outcome(&owner.rig.name),
        Some(PendingRestore::BoundFromLedger {
            exact_wait: None,
            ..
        })
    ));
}

/// A boot re-adopts a channel's kept V2 queue and wakes its owner drive, which grants the kept front
/// and hands it to the idle start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_boot_recovery_hands_the_kept_front_to_the_owner_drive() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = poise::serenity_prelude::ChannelId::new(7_272_596);
    let persistence = crate::services::discord::queue_dispatch::persistence_context;
    let persisted = persistence(&shared, &shared.provider, channel);
    let queue = crate::services::turn_orchestrator::v2_queue(&persisted, channel).unwrap();
    let kept: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":72_725_961,"channel_id":channel.get(),"source_message_ids":[72_725_961],
        "queued_generation":1,"text":"kept"}))
    .unwrap();
    {
        let before = ChannelMailboxRegistry::default();
        let handle = before.handle(channel);
        handle.v2_adopt(queue.0, queue.1).await.unwrap();
        let _ = admit(&handle, &kept, AdmissionTicket::new()).await;
    }
    recover(&shared, channel).await;
    let mailbox = shared.mailbox(channel);
    let granted =
        || mailbox.v2_request(|slot| Ok(slot.as_ref().is_some_and(|o| o.front_outstanding())));
    let mut handed = false;
    for _ in 0..100 {
        handed = granted().await.unwrap();
        if handed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(handed, "the recovered owner's drive granted its kept front");
}

/// Stands in for the live path's driver after it replaced the stale pane: the pane runs again under
/// the channel's account, and the owner's hook writes the created execution's first prompt.
fn relaunch(
    rig: &Rig,
    (session, dir): (&str, &Path),
    sender: &Sender<StreamMessage>,
) -> ClaudeLegacyProbe {
    let (name, channel, transcript) = (rig.name.clone(), rig.channel, rig.transcript());
    let (real, socket) = (rig.f.real.clone(), rig.f.socket.clone());
    let (ready, events) = (rig.renderer.join("ready"), rig.renderer.join("events"));
    let (sender, session, dir) = (sender.clone(), session.to_string(), dir.to_path_buf());
    Box::new(move || {
        if events.exists() {
            return Err("a key reached the other account's pane".into());
        }
        let default = crate::services::provider_auth_profile::DEFAULT_PROFILE_ID;
        crate::services::tmux_common::write_tmux_session_auth_profile(&name, default)?;
        fs::remove_file(&ready).map_err(|e| e.to_string())?;
        let pane = format!("={name}:");
        let respawn = [
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "respawn-pane",
            "-k",
            "-t",
            &pane,
        ];
        let status = std::process::Command::new(&real).args(respawn).status();
        status
            .map_err(|e| e.to_string())?
            .success()
            .then_some(())
            .ok_or("respawn failed")?;
        wait("relaunched", || ready.exists());
        let hook = scope::launch_hook().ok_or("no owner hook")?;
        let target = scope::created_tmux_target("claude", &name)?;
        let source = json!({"kind":"pre_source","expected_session_id":null,
            "expected_transcript":null,"binding_watermark":null});
        let observe = crate::services::claude_tui::hosting::owner_observer;
        let cancel = Arc::new(CancelToken::new());
        let at = (name.as_str(), PROMPT, channel);
        let until_done = observe(at, (Some(&session), &dir), sender, cancel).ok_or("unobserved")?;
        let path = transcript.display().to_string();
        let submit: scope::CreatedSubmit = Box::new(move |frame| {
            crate::services::claude::first_input::submit_created(&path, 0, &name, &frame).map(drop)
        });
        hook(scope::Lineage::Created, target, source, None, submit)?;
        until_done()
    })
}

/// A live idle pane under another account than the channel's takes no key: the first front goes to
/// the driver's replacement under the channel's account, the second into that pane, each answered once.
#[test]
fn a_pane_under_another_account_is_replaced_before_any_front_is_written() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-idle-acct", 7_272_597, "normal", view("", false), BUSY);
    let second: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":rig.original.message_id + 1,"channel_id":rig.channel,
        "source_message_ids":[rig.original.message_id + 1],"queued_generation":1,
        "text":"second question"}))
    .unwrap();
    let admitted = admit(&rig.mailbox, &second, AdmissionTicket::new());
    rig.runtime.block_on(admitted);
    crate::services::tmux_common::write_tmux_session_auth_profile(&rig.name, "account-a").unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    let (fronts, inbox) = std::sync::mpsc::channel::<(Front, Settled)>();
    let fronts = std::sync::Mutex::new(fronts);
    let idle: Handoff = Arc::new(move |front| {
        let (settled, answer) = tokio::sync::oneshot::channel();
        let sent = fronts.lock().unwrap().send((front, settled));
        Box::pin(async move {
            sent.map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, rig.channel),
        idle,
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(rig.transcript())
        .unwrap();
    writeln!(file, "{}", IDLE.trim_end()).unwrap();
    let answering = answers(&rig);
    {
        let _entered = rig.runtime.enter();
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
    }
    let (mut handed, mut said) = (Vec::new(), Vec::new());
    while let Ok((front, settled)) = inbox.recv_timeout(Duration::from_secs(15)) {
        handed.push(front.1.text.clone());
        rehydrate(&rig, &session);
        let at = (session.as_str(), elsewhere.path());
        let first = handed.len() == 1;
        let driver = |sender: &Sender<StreamMessage>| first.then(|| relaunch(&rig, at, sender));
        let (result, sent) = idle_turn(&rig, at, (front, settled), driver);
        assert_eq!(result, Ok(()), "{handed:?}");
        assert!(
            CLAUDE_LEGACY_PROBE.lock().unwrap().is_none(),
            "the driver ran"
        );
        said.extend(sent.into_iter().filter_map(|m| match m {
            StreamMessage::Text { content } => Some(content),
            _ => None,
        }));
        if handed.len() == 2 {
            break;
        }
    }
    answering.join().unwrap();
    assert_eq!(handed, [PROMPT, "second question"], "in order, each once");
    let events = rig.events();
    let count = |key: &str| events.iter().filter(|e| *e == key).count();
    assert_eq!((count("paste"), count("Enter")), (2, 2), "{events:?}");
    let answered = |n: &str| said.iter().filter(|text| text.contains(n)).count();
    assert_eq!(
        (answered("answer 1"), answered("answer 2")),
        (1, 1),
        "{said:?}"
    );
    let snapshot = rig.snapshot();
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}
