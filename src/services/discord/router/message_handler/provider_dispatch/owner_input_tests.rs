//! The owner branch from `provider_dispatch::execute`: a real pane under a traced tmux, the
//! owner's actor, and what reached the pane read from the renderer's own record.
use super::super::{StreamingTurn, execute};
use super::*;
use crate::services::claude_tui::busy_inject::{StashChannelGuard, Timing};
use crate::services::claude_tui::input::submission::tests::{events, renderer_with, view, wait};
use crate::services::discord::queue_io::input_owner::admission::{
    AdmissionTicket, TurnBind, admit, exact_turn,
};
use crate::services::discord::queue_io::input_owner::drive::{Front, claim_turn};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub(super) const PROMPT: &str = "owner busy question";
pub(super) const AUTHOR: &str = "7";
pub(super) const NAME: &str = "tester";
const BUSY: &str = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
    {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";
const IDLE: &str = "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n";
/// Short confirm windows; a lock retry list, without which the busy entry never takes the lock.
static OWNER_TIMING: Timing = Timing {
    lock_retries: &[Duration::ZERO, Duration::from_millis(20)],
    settle: Duration::from_millis(30),
    confirm_window: Duration::from_millis(500),
    confirm_poll: Duration::from_millis(10),
};

pub(super) struct Rig {
    pub(super) f: Fixture,
    pub(super) name: String,
    channel: u64,
    pub(super) nonce: String,
    pub(super) renderer: std::path::PathBuf,
    program: std::path::PathBuf,
    pub(super) runtime: tokio::runtime::Runtime,
    _registry: ChannelMailboxRegistry,
    mailbox: ChannelMailboxHandle,
    ticket: AdmissionTicket,
    original: PendingQueueItem,
    /// The database an idle turn's execution capacity is leased in, when a test gives one.
    pub(super) pool: std::sync::OnceLock<sqlx::PgPool>,
}

/// A known pane over `transcript` and an owner with one admitted original.
fn rig(name: &str, channel: u64, mode: &str, before: String, transcript: &str) -> Rig {
    let f = Fixture::after_shared_test_env_lock();
    let nonce = format!("{:032x}", channel);
    let shown =
        crate::services::claude_tui::busy_inject::frame("discord", AUTHOR, NAME, &nonce, PROMPT);
    let renderer = renderer_with(&f, name, mode, before, transcript, &shown);
    owned(f, name, channel, renderer, "claude")
}

/// An owner with one admitted original over the `provider` pane in session `name`. Its tmux fails a
/// load on `fail-load`, respawns after the step in `swap-after`, and runs `after-<step>` once.
pub(super) fn owned(
    f: Fixture,
    name: &str,
    channel: u64,
    renderer: std::path::PathBuf,
    provider: &str,
) -> Rig {
    let nonce = format!("{:032x}", channel);
    let pane = f.tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={name}:"),
        "#{pane_id}",
    ]);
    let program = f.root.path().join(format!("{name}-owner-tmux"));
    let dir = f.root.path().join(format!("{name}-owner"));
    fs::create_dir_all(&dir).unwrap();
    let script = format!(
        "#!/bin/sh\nd='{dir}'\nprintf '%s\\n' \"$*\" >> \"$d/trace\"\n\
         case \"$*\" in *load-buffer*) [ ! -f \"$d/fail-load\" ] || exit 1 ;; esac\n\
         case \"$*\" in *paste-buffer*) [ ! -f \"$d/before-paste\" ] || {{ sh \"$d/before-paste\"; rm \"$d/before-paste\"; }} ;; esac\n\
         '{shim}' \"$@\"; rc=$?\ncase \"$*\" in *load-buffer*) step=load ;; *paste-buffer*) step=paste ;; *C-s*) step=C-s ;; *send-keys*Enter*) step=Enter ;; *) step= ;; esac\n\
         if [ -n \"$step\" ] && [ \"$(cat \"$d/swap-after\" 2>/dev/null)\" = \"$step\" ]; then\n\
           rm \"$d/swap-after\"; '{real}' -L '{socket}' -f /dev/null respawn-pane -k -t '{pane}'\nfi\n\
         if [ -n \"$step\" ] && [ -f \"$d/after-$step\" ]; then sh \"$d/after-$step\"; rm \"$d/after-$step\"; fi\nexit $rc\n",
        dir = dir.display(),
        shim = f.root.path().join("tmux").display(),
        real = f.real.display(),
        socket = f.socket,
    );
    fs::write(&program, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let context = json!({"schema":1,"provider":provider,"created_at":chrono::Utc::now(),
        "execution_nonce":&nonce,"tmux_session":name,"channel_id":channel,
        "owner_runtime_root":root,"host":"owner-dispatch-test",
        "expected_native_session_id":null,"launch_mode":"fresh","provider_root":null});
    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
        serde_json::from_value(context).unwrap(),
    )
    .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
    fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    fs::write(marker, &nonce).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let id = 8_000_000_000_000_005_000 + channel % 1000;
    let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":id,"channel_id":channel,"source_message_ids":[id],
        "queued_generation":1,"text":PROMPT}))
    .unwrap();
    let ticket = AdmissionTicket::new();
    runtime.block_on(async {
        let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude/input-owner-dispatch");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{channel}.json"));
        mailbox.v2_adopt(path, "input-owner".into()).await.unwrap();
        let admitted = admit(&mailbox, &original, ticket.clone()).await;
        assert!(matches!(
            admitted,
            crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
        ));
    });
    drop(entered);
    Rig {
        f,
        name: name.into(),
        channel,
        nonce,
        renderer,
        program,
        runtime,
        _registry: registry,
        mailbox,
        ticket,
        original,
        pool: std::sync::OnceLock::new(),
    }
}

impl Rig {
    pub(super) fn mark(&self, file: &str, value: &str) {
        let dir = self.f.root.path().join(format!("{}-owner", self.name));
        fs::write(dir.join(file), value).unwrap();
    }

    /// One turn through `execute` with this rig's owner registered for its channel.
    pub(super) fn dispatch(
        &self,
        provider: ProviderKind,
        phase: target::Phase,
        transcript: &Path,
    ) -> Result<(), String> {
        self.dispatch_as(
            provider,
            phase,
            transcript,
            Execution::LocalTui,
            Some(&self.name),
        )
    }

    /// As `dispatch`, on `execution` and with or without a pane name on the turn.
    fn dispatch_as(
        &self,
        provider: ProviderKind,
        phase: target::Phase,
        transcript: &Path,
        execution: Execution,
        pane: Option<&str>,
    ) -> Result<(), String> {
        register(
            self.channel,
            self.registration(&provider, phase, transcript, execution),
        );
        self.with_turn(&provider, pane, |turn, sender| {
            let start =
                crate::services::discord::router::intake_dispatch::replay_admission::dormant_start(
                    "test",
                )
                .unwrap();
            execute(turn, start, sender, None)
        })
    }

    /// The owner's turn claim on `front`, as the turn that starts it makes it.
    pub(super) fn claim(&self, front: &Front) -> Option<FrontClaim> {
        let claimed = claim_turn(&self.mailbox, front.1.message_id, front);
        self.runtime.block_on(claimed).unwrap()
    }

    /// The turn's claim on `front`, then the drive's handoff entry with the owner as its argument.
    pub(super) fn handoff(
        &self,
        front: Front,
        provider: ProviderKind,
        phase: target::Phase,
        transcript: &Path,
    ) -> Result<(), String> {
        let claim = self.claim(&front).ok_or("NotSent(\"front_not_granted\")")?;
        self.execute(claim, provider, phase, transcript)
    }

    pub(super) fn execute(
        &self,
        claim: FrontClaim,
        provider: ProviderKind,
        phase: target::Phase,
        transcript: &Path,
    ) -> Result<(), String> {
        let (settled, _) = tokio::sync::oneshot::channel();
        self.execute_until(claim, (provider, phase, transcript), settled, None)
    }

    /// As `execute`, answering the drive on `settled` and observing the AI turn with `until_done`.
    pub(super) fn execute_until(
        &self,
        claim: FrontClaim,
        (provider, phase, transcript): (ProviderKind, target::Phase, &Path),
        settled: Settled,
        until_done: Option<super::UntilDone>,
    ) -> Result<(), String> {
        let mut owner = self.registration(&provider, phase, transcript, Execution::LocalTui);
        if until_done.is_some() {
            owner.until_done = until_done;
        }
        self.with_turn(&provider, Some(&self.name), |turn, sender| {
            execute_front(turn, sender, claim, owner, settled)
        })
    }

    /// The owner's unattempted front, granted in the actor turn that read it.
    pub(super) fn granted_front(&self) -> Front {
        let granted = self.mailbox.v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            owner.grant_front(&front)?;
            Ok(front)
        });
        self.runtime.block_on(granted).unwrap()
    }

    fn registration(
        &self,
        provider: &ProviderKind,
        phase: target::Phase,
        transcript: &Path,
        execution: Execution,
    ) -> Registration {
        let root = crate::services::tmux_common::current_tmux_owner_marker();
        // A live pane's attempt names the transcript it was observed on.
        let source = if phase == target::Phase::Busy {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::metadata(transcript).unwrap();
            json!({"kind":"existing","session_id":format!("sid-{}", self.channel),
                "transcript":transcript.display().to_string(),
                "file":[meta.dev(), meta.ino()],"offset":meta.len()})
        } else {
            json!({"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null})
        };
        Registration {
            mailbox: self.mailbox.clone(),
            execution,
            origin: InputOrigin::Discord,
            phase,
            attempt: json!({"nonce":self.nonce,"provider":provider.as_str(),
                "target":{"host":"tmux","owner":self.name,"root":root,"execution_nonce":self.nonce},
                "source":source}),
            sources: vec![self.original.message_id.to_string()],
            transcript: transcript.to_path_buf(),
            program: self.program.clone(),
            herdr: None,
            author: AUTHOR.into(),
            name: NAME.into(),
            clock: Arc::new(|| 10_000),
            timing: &OWNER_TIMING,
            // The rig's AI turn ends as soon as it is read.
            until_done: Some(Box::new(|| Ok(()))),
        }
    }

    fn with_turn(
        &self,
        provider: &ProviderKind,
        pane: Option<&str>,
        run: impl FnOnce(
            StreamingTurn<'_>,
            std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        let host = TurnHost::Tmux;
        let turn = StreamingTurn {
            pool: self.pool.get(),
            shared: None,
            provider,
            prompt: PROMPT,
            session_id: None,
            working_dir: "/tmp",
            system_prompt: None,
            allowed_tools: &[],
            cancel: Arc::new(CancelToken::new()),
            codex_observation: None,
            tmux_session_name: pane,
            teardown: None,
            host: &host,
            channel_id: self.channel,
            model: None,
            native_fast_mode: None,
            codex_goals: None,
            compact_percent: None,
            compact_lower_bound_tokens: 0,
            compact_token_limit: None,
            cache_ttl_minutes: None,
            dispatch_type: None,
            force_fresh: false,
            submission: None,
        };
        let (sender, _receiver) = std::sync::mpsc::channel();
        let _entered = self.runtime.enter();
        run(turn, sender)
    }

    pub(super) fn transcript(&self) -> std::path::PathBuf {
        self.renderer.join("transcript")
    }

    pub(super) fn snapshot(&self) -> Value {
        self.runtime
            .block_on(self.mailbox.v2_request(|s| s.as_ref().unwrap().json()))
            .unwrap()
    }

    pub(super) fn bind(&self) -> TurnBind {
        self.runtime
            .block_on(exact_turn(&self.mailbox, &self.ticket, &self.original))
            .unwrap()
    }

    /// The pane's record once the last key has had time to land.
    pub(super) fn events(&self) -> Vec<String> {
        std::thread::sleep(Duration::from_millis(300));
        events(&self.renderer)
    }

    pub(super) fn delivered(&self) {
        let snapshot = self.snapshot();
        assert!(snapshot["head_injection"].is_null(), "{snapshot}");
        assert_eq!(
            snapshot["retained_inputs"],
            json!([]),
            "nothing re-held: {snapshot}"
        );
        assert!(
            snapshot["items"].as_array().unwrap().is_empty(),
            "{snapshot}"
        );
        assert!(matches!(self.bind(), TurnBind::Settled(turn) if turn.attempt == self.nonce));
    }

    /// The original back in custody with the owner's not-sent proof, and no key sent.
    pub(super) fn held_not_sent(&self, reason: &str) {
        let snapshot = self.snapshot();
        let retained = &snapshot["retained_inputs"];
        assert_eq!(retained.as_array().map(Vec::len), Some(1), "{snapshot}");
        assert_eq!(retained[0]["disposition"], "known_not_sent", "{snapshot}");
        let why = retained[0]["reason"]["reason"].as_str().unwrap_or_default();
        assert!(why.contains(reason), "{why}");
        assert_eq!(retained[0]["input"]["original"]["text"], PROMPT);
        assert!(self.events().is_empty(), "no key: {:?}", self.events());
    }

    /// Content may have gone: the head stays, Unknown, and nothing is queued or sent again.
    pub(super) fn uncertain(&self) {
        let snapshot = self.snapshot();
        assert_eq!(snapshot["head_injection"]["phase"], "unknown", "{snapshot}");
        assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    }
}

/// A busy pane parks a visible draft once, pastes and enters once, and an empty composer gets no
/// C-s; a modal gets no key and the original stays unretained at the queue front.
#[test]
fn owner_busy_claude_submits_once_and_holds_a_modal_front() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (name, channel, before, listed, expected) in [
        (
            "ob-draft",
            7_272_501,
            view("js", false),
            true,
            vec!["C-s", "paste", "Enter"],
        ),
        (
            "ob-unlisted",
            7_272_509,
            view("js", false),
            false,
            vec!["C-s", "paste", "Enter"],
        ),
        (
            "ob-empty",
            7_272_502,
            view("", false),
            true,
            vec!["paste", "Enter"],
        ),
    ] {
        let _listed = listed.then(|| StashChannelGuard::new(channel));
        let rig = rig(name, channel, "normal", before, BUSY);
        let result = rig.dispatch(ProviderKind::Claude, target::Phase::Busy, &rig.transcript());
        assert_eq!(result, Ok(()), "{name}");
        assert_eq!(rig.events(), expected, "{name}");
        let shown = crate::services::claude_tui::busy_inject::frame(
            "discord", AUTHOR, NAME, &rig.nonce, PROMPT,
        );
        assert_eq!(
            fs::read_to_string(rig.renderer.join("body")).unwrap(),
            shown
        );
        rig.delivered();
    }
    let modal = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n";
    let rig = rig("ob-modal", 7_272_503, "normal", modal.into(), BUSY);
    let result = rig.dispatch(ProviderKind::Claude, target::Phase::Busy, &rig.transcript());
    assert_eq!(result, Err("NotSent(\"Modal\")".into()));
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(
        snapshot["retained_inputs"],
        json!([]),
        "held, not retained: {snapshot}"
    );
    assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
    assert!(rig.events().is_empty(), "no key: {:?}", rig.events());
    assert_eq!(rig.bind(), TurnBind::NotArmed);
}

/// The drive's handoff runs the owner's front from its arguments alone, with nothing registered
/// for the channel: a front it was not granted reserves nothing, the granted one is delivered once.
#[test]
fn the_front_handoff_runs_from_its_arguments_and_never_an_ungranted_front() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("of-front", 7_272_510, "normal", view("", false), BUSY);
    let front = rig.granted_front();
    assert_eq!(front.0.as_deref(), Some(rig.ticket.as_str()));
    let mut other = front.clone();
    other.0 = Some("another-ticket".into());
    let transcript = rig.transcript();
    let refused = rig.handoff(
        other,
        ProviderKind::Claude,
        target::Phase::Busy,
        &transcript,
    );
    assert_eq!(refused, Err("NotSent(\"front_not_granted\")".into()));
    assert!(rig.events().is_empty(), "{:?}", rig.events());
    assert!(rig.snapshot()["head_injection"].is_null());
    let registered = REGISTERED
        .lock()
        .unwrap()
        .iter()
        .any(|(id, _)| *id == rig.channel);
    assert!(!registered, "nothing travels through the static");
    let result = rig.handoff(
        front.clone(),
        ProviderKind::Claude,
        target::Phase::Busy,
        &transcript,
    );
    assert_eq!(result, Ok(()), "the refused front left the grant in place");
    let again = rig.handoff(
        front,
        ProviderKind::Claude,
        target::Phase::Busy,
        &transcript,
    );
    assert_eq!(again, Err("NotSent(\"front_not_granted\")".into()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    rig.delivered();
}

/// The turn's claim carries the granted front to one reserve with no second grant; a claim the queue
/// cancelled before its reserve sends nothing.
#[test]
fn a_turn_claim_carries_its_front_to_one_reserve() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oc-claim", 7_272_536, "normal", view("", false), BUSY);
    let front = rig.granted_front();
    let claim = rig.claim(&front).expect("the granted front's claim");
    assert!(rig.claim(&front).is_none(), "one claim per grant");
    let delivered = rig.execute(
        claim,
        ProviderKind::Claude,
        target::Phase::Busy,
        &rig.transcript(),
    );
    assert_eq!(delivered, Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    rig.delivered();

    let rig = rig_cancelled_after_claim();
    assert!(rig.events().is_empty(), "{:?}", rig.events());
    assert!(rig.snapshot()["head_injection"].is_null());
}

fn rig_cancelled_after_claim() -> Rig {
    let rig = rig(
        "oc-claim-cancel",
        7_272_537,
        "normal",
        view("", false),
        BUSY,
    );
    let front = rig.granted_front();
    let claim = rig.claim(&front).unwrap();
    let ctx = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        &ProviderKind::Claude,
        "claim",
        None,
    );
    let id = poise::serenity_prelude::MessageId::new(front.1.message_id);
    let cancelled = rig.mailbox.cancel_queued_primary_message(id, ctx);
    assert!(rig.runtime.block_on(cancelled).removed.is_some());
    let result = rig.execute(
        claim,
        ProviderKind::Claude,
        target::Phase::Busy,
        &rig.transcript(),
    );
    assert_eq!(result, Err("NotSent(\"front_not_granted\")".into()));
    rig
}

/// An idle front's handoff ends at its local settlement while its AI turn runs on: the next front
/// is delivered busy before that turn ends, and the turn's end is observed once.
#[test]
fn an_idle_handoff_ends_at_settlement_while_its_ai_turn_runs() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-turn", 7_272_538, "normal", view("", false), BUSY);
    let mut next = rig.original.clone();
    next.message_id += 1;
    next.source_message_ids = vec![next.message_id];
    let admitted = rig
        .runtime
        .block_on(admit(&rig.mailbox, &next, AdmissionTicket::new()));
    assert!(matches!(
        admitted,
        crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
    ));
    let done = Arc::new(Mutex::new(0));
    let (settled, answer) = tokio::sync::oneshot::channel();
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let transcript = rig.transcript();
    std::thread::scope(|scope| {
        // Inside the scope, so a failed assertion drops it and ends the held AI turn.
        let (release, turn) = std::sync::mpsc::channel::<()>();
        let observed = done.clone();
        let until_done: super::UntilDone = Box::new(move || {
            let _ = turn.recv();
            *observed.lock().unwrap() += 1;
            Ok(())
        });
        let first = scope.spawn(|| {
            let how = (
                ProviderKind::Claude,
                target::Phase::Idle,
                transcript.as_path(),
            );
            rig.execute_until(claim, how, settled, Some(until_done))
        });
        let wait = async { tokio::time::timeout(Duration::from_secs(20), answer).await };
        let answered = rig.runtime.block_on(wait);
        let busy = rig.handoff(
            rig.granted_front(),
            ProviderKind::Claude,
            target::Phase::Busy,
            &transcript,
        );
        let before_end = rig.events();
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), Ok(()));
        assert_eq!(
            answered.map(Result::ok),
            Ok(Some(Ok(()))),
            "settled before the AI turn ended"
        );
        assert_eq!(busy, Ok(()));
        // The fake pane still draws A's text, so B may park it first; each input is pasted once.
        let pastes = before_end.iter().filter(|event| *event == "paste").count();
        assert_eq!(pastes, 2, "{before_end:?}");
        assert_eq!(before_end[..2], ["paste", "Enter"], "{before_end:?}");
        assert!(before_end.ends_with(&["paste".to_string(), "Enter".to_string()]));
    });
    assert_eq!(*done.lock().unwrap(), 1, "the turn's end is observed once");
}

/// An idle pane showing a modal sends no key and holds the front with its ticket, unretained; once
/// the modal is gone the same front is delivered with no new input.
#[test]
fn owner_idle_claude_holds_a_modal_front_until_the_modal_is_gone() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let modal = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n";
    let rig = rig("oi-modal", 7_272_547, "normal", modal.into(), IDLE);
    let (idle, transcript) = (target::Phase::Idle, rig.transcript());
    let held = rig.handoff(rig.granted_front(), ProviderKind::Claude, idle, &transcript);
    assert_eq!(held, Err("NotSent(\"Modal\")".into()));
    assert!(rig.events().is_empty(), "no key: {:?}", rig.events());
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(
        snapshot["retained_inputs"],
        json!([]),
        "held, not retained: {snapshot}"
    );
    let front = rig.granted_front();
    assert_eq!(front.0.as_deref(), Some(rig.ticket.as_str()), "same ticket");
    // C-u makes the fake pane draw its empty composer and the history goes: the modal is gone.
    let pane = format!("={}:", rig.name);
    rig.f.tmux(&["send-keys", "-t", &pane, "C-u"]);
    wait("modal-closed", || rig.events() == ["clear"]);
    rig.f.tmux(&["clear-history", "-t", &pane]);
    let delivered = rig.handoff(front, ProviderKind::Claude, idle, &transcript);
    assert_eq!(delivered, Ok(()));
    assert_eq!(rig.events(), ["clear", "paste", "Enter"]);
    rig.delivered();
}

/// A legacy draft guard does not stop the owner's transaction: the guard is legacy state.
#[test]
fn legacy_guard_value_does_not_block_owner_a() {
    use crate::services::claude_tui::composer_lock::{DraftGuard, guard_draft};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("ob-guard", 7_272_504, "normal", view("", false), BUSY);
    guard_draft(&rig.name, DraftGuard::RecoveryRequired);
    let result = rig.dispatch(ProviderKind::Claude, target::Phase::Busy, &rig.transcript());
    assert_eq!(result, Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    rig.delivered();
}

/// Enter ends the owner's transaction: a swallowed Enter gets no finishing Enter and no second
/// paste, the head is delivered whether or not the receipt shows, and no alert follows.
#[test]
fn owner_busy_claude_enter_ends_the_transaction_with_or_without_receipt() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oc-missing", 7_272_506, "swallow", view("", false), BUSY);
    let elsewhere = rig.f.root.path().join("oc-missing-unwritten.jsonl");
    fs::write(&elsewhere, BUSY).unwrap();
    let result = rig.dispatch(ProviderKind::Claude, target::Phase::Busy, &elsewhere);
    assert_eq!(result, Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"], "one paste, one Enter");
    rig.delivered();
    let past_deadline = rig
        .mailbox
        .v2_request(|s| s.as_mut().unwrap().tick(10_000 + 31_000));
    rig.runtime.block_on(past_deadline).unwrap();
    rig.delivered();
    std::thread::sleep(OWNER_TIMING.confirm_window);
    let alerts: Vec<_> = crate::services::observability::events::recent(usize::MAX)
        .into_iter()
        .filter(|event| {
            event.event_type == "busy_inject_unconfirmed" && event.channel_id == Some(rig.channel)
        })
        .collect();
    assert!(alerts.is_empty(), "{alerts:?}");
}

/// A load that never became a buffer sends nothing and proves it: the original and its ticket
/// stay with the owner.
#[test]
fn load_failed_is_definitely_not_submitted() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("ob-load", 7_272_507, "normal", view("", false), BUSY);
    rig.mark("fail-load", "");
    let result = rig.dispatch(ProviderKind::Claude, target::Phase::Busy, &rig.transcript());
    assert_eq!(result, Err("NotSent(\"LoadFailed\")".into()));
    rig.held_not_sent("load_failed");
    assert_eq!(rig.bind(), TurnBind::NotArmed);
}

/// The pane's process replaced before ready sends nothing and proves it; after C-s it is a stash
/// failure and after the paste a write that may have landed: both stay Unknown, never resent.
#[test]
fn pane_pid_swap_stages_map_to_not_sent_or_write_uncertain() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig_ready = rig("ps-ready", 7_272_508, "normal", view("", false), BUSY);
    rig_ready.mark("swap-after", "load");
    let result = rig_ready.dispatch(
        ProviderKind::Claude,
        target::Phase::Busy,
        &rig_ready.transcript(),
    );
    assert_eq!(result, Err("NotSent(\"PaneUnavailable\")".into()));
    rig_ready.held_not_sent("owner_refused");
    for (name, channel, step, reached, outcome) in [
        (
            "ps-cs",
            7_272_509,
            "C-s",
            vec!["C-s"],
            "Failed(\"stash_failed\")",
        ),
        (
            "ps-paste",
            7_272_510,
            "paste",
            vec!["C-s", "paste"],
            "WriteUncertain(\"enter_failed\")",
        ),
    ] {
        let _listed = StashChannelGuard::new(channel);
        let rig = rig(name, channel, "normal", view("js", false), BUSY);
        rig.mark("swap-after", step);
        let result = rig.dispatch(ProviderKind::Claude, target::Phase::Busy, &rig.transcript());
        assert_eq!(result, Err(outcome.into()), "{name}");
        wait("swapped-pane-ready", || rig.renderer.join("ready").exists());
        assert_eq!(rig.events(), reached, "{name}");
        rig.uncertain();
        assert!(matches!(rig.bind(), TurnBind::Armed(_)), "{name}");
    }
}

/// An idle pane takes main's warm submission inside the owner scope, which also admits the
/// listed channel's stash: one scope, no second install.
#[test]
fn owner_idle_claude_parks_a_listed_draft_under_the_owner_scope() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _listed = StashChannelGuard::new(7_272_511);
    let rig = rig("oi-draft", 7_272_511, "normal", view("js", false), IDLE);
    let result = rig.dispatch(ProviderKind::Claude, target::Phase::Idle, &rig.transcript());
    assert_eq!(result, Ok(()));
    assert_eq!(rig.events(), ["C-s", "paste", "Enter"]);
    rig.delivered();
}

/// The owner parks a visible draft before its idle submit on any channel, listed or not.
#[test]
fn owner_idle_claude_parks_an_unlisted_draft() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-unlisted", 7_272_560, "normal", view("js", false), IDLE);
    let result = rig.dispatch(ProviderKind::Claude, target::Phase::Idle, &rig.transcript());
    assert_eq!(result, Ok(()));
    assert_eq!(rig.events(), ["C-s", "paste", "Enter"]);
    rig.delivered();
}

/// An idle submit starts an AI turn only its provider's observer reads: without one the front is
/// refused before any key and no head is reserved.
#[test]
fn owner_idle_front_without_an_observer_sends_nothing() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-unobserved", 7_272_561, "normal", view("", false), IDLE);
    let (provider, transcript) = (ProviderKind::Claude, rig.transcript());
    let mut owner = rig.registration(
        &provider,
        target::Phase::Idle,
        &transcript,
        Execution::LocalTui,
    );
    owner.until_done = None;
    register(rig.channel, owner);
    let result = rig.with_turn(&provider, Some(&rig.name), |turn, sender| {
        let start =
            crate::services::discord::router::intake_dispatch::replay_admission::dormant_start;
        execute(turn, start("test").unwrap(), sender, None)
    });
    assert_eq!(result, Err("NotSent(\"idle_unobserved\")".into()));
    assert!(rig.events().is_empty(), "no key: {:?}", rig.events());
    assert!(rig.snapshot()["head_injection"].is_null());
}

/// A refusal that writes nothing needs no pane: a remote placement and an unsupported provider
/// each keep the original queued, with no head and no key.
#[test]
fn owner_refusals_before_a_pane_need_no_session() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("ob-nopane", 7_272_513, "normal", view("", false), BUSY);
    for (provider, phase, execution, reason) in [
        (
            ProviderKind::Claude,
            target::Phase::Busy,
            Execution::RemoteTui,
            "remote_placement",
        ),
        (
            ProviderKind::Gemini,
            target::Phase::Busy,
            Execution::LocalTui,
            "provider_unsupported",
        ),
    ] {
        let result = rig.dispatch_as(provider, phase, &rig.transcript(), execution, None);
        assert_eq!(result, Err(format!("NotSent({reason:?})")), "{reason}");
        let snapshot = rig.snapshot();
        assert!(snapshot["head_injection"].is_null(), "{snapshot}");
        assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
        assert_eq!(rig.bind(), TurnBind::NotArmed);
    }
    assert!(rig.events().is_empty(), "{:?}", rig.events());
}

/// The prepared attempt as dispatch holds it before any launch: its target names no execution
/// the launch will create.
fn prepared_before_launch(rig: &Rig) -> Registration {
    Registration {
        mailbox: rig.mailbox.clone(),
        execution: Execution::LocalTui,
        origin: InputOrigin::Discord,
        phase: target::Phase::Absent,
        attempt: json!({"nonce":rig.nonce,"provider":"claude",
            "target":{"host":"tmux","owner":rig.name,"root":"/prepared/root",
                "execution_nonce":"0".repeat(32)},
            "source":{"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null}}),
        sources: vec![rig.original.message_id.to_string()],
        transcript: rig.transcript(),
        program: rig.program.clone(),
        herdr: None,
        author: AUTHOR.into(),
        name: NAME.into(),
        clock: Arc::new(|| 10_000),
        timing: &OWNER_TIMING,
        until_done: None,
    }
}

/// The created pane's first prompt through the owner's launch hook: the driver's submit on the
/// target read from the created pane.
fn created_first_prompt(rig: &Rig) -> Result<(), String> {
    let target = scope::created_tmux_target("claude", &rig.name)?;
    let source = json!({"kind":"pre_source","expected_session_id":null,
        "expected_transcript":null,"binding_watermark":null});
    let (transcript, name) = (rig.transcript().display().to_string(), rig.name.clone());
    let submit: scope::CreatedSubmit = Box::new(move |frame| {
        crate::services::claude::first_input::submit_created(&transcript, 0, &name, &frame)
            .map(drop)
    });
    let hook = created(
        prepared_before_launch(rig),
        PROMPT.into(),
        scope::Lineage::Created,
        (None, reply_once(None)),
    );
    let _entered = rig.runtime.enter();
    hook(scope::Lineage::Created, target, source, None, submit)
}

/// The owner reserves a created execution's first prompt on the identity read from the created
/// pane, not on the target it prepared before the launch: one paste, one Enter, delivered.
#[test]
fn created_first_prompt_reserves_the_created_target_not_the_prepared_one() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oc-created", 7_272_514, "normal", view("", false), IDLE);
    assert_eq!(created_first_prompt(&rig), Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    assert_eq!(
        fs::read_to_string(rig.renderer.join("body")).unwrap(),
        PROMPT
    );
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    let bind = rig.bind();
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    assert!(
        matches!(&bind, TurnBind::Settled(turn) if turn.attempt == rig.nonce),
        "{bind:?}"
    );
    assert_eq!(
        scope::created_tmux_target("claude", &rig.name).unwrap(),
        json!({"host":"tmux","owner":rig.name,"root":root,"execution_nonce":rig.nonce})
    );
}

/// A startup dialog's Enter is the attempt's first key: the owner's ready holds before it and no
/// refusal after it proves the attempt unsent, so the head stays Unknown and nothing is re-held.
#[test]
fn startup_enter_is_the_attempts_first_key() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dialog = "Resume from summary\n❯ 1. Resume from summary\n  2. Resume full session\n\
        Enter to confirm · Esc to cancel\n";
    let rig = rig("oc-dialog", 7_272_515, "normal", dialog.into(), IDLE);
    fs::write(rig.renderer.join("body"), "").unwrap();
    let result = created_first_prompt(&rig);
    assert!(result.is_err(), "{result:?}");
    let events = rig.events();
    assert_eq!(
        events.first().map(String::as_str),
        Some("Enter"),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| e == "paste"), "{events:?}");
    let snapshot = rig.snapshot();
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    assert!(!snapshot["head_injection"].is_null(), "{snapshot}");
}

/// A fresh launch hands its first prompt to the owner waiting on its thread: the hook gets the
/// created pane's own target before any key, and the launch writes nothing itself.
#[test]
fn fresh_launch_hands_its_first_prompt_to_the_waiting_owner() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oc-wired", 7_272_516, "normal", view("", false), IDLE);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    let _hook = scope::install_launch(Box::new(
        move |_lineage, target, source, _writer, _submit| {
            record.lock().unwrap().push((target, source));
            Err("refused by the test".into())
        },
    ));
    let owner = rig.f.root.path().join("oc-wired-marker");
    fs::write(&owner, "owner").unwrap();
    let result =
        crate::services::claude::fresh_hold_for_tests(&rig.transcript(), &rig.name, &owner, PROMPT);
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.contains("refused by the test")),
        "{result:?}"
    );
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one hand-over");
    assert_eq!(
        seen[0].0,
        json!({"host":"tmux","owner":rig.name,"root":root,"execution_nonce":rig.nonce})
    );
    assert_eq!(seen[0].1["expected_session_id"], "held-sid");
    assert!(rig.events().is_empty(), "{:?}", rig.events());
    rig.kept(&owner);
}

impl Rig {
    /// A fresh launch of this rig's pane whose first prompt the dispatch owner takes; `writer`
    /// replaces the physical writer the launch handed over.
    fn fresh_owned(&self, owner: &Path, writer: Option<&str>) -> Result<(), String> {
        let hook = created(
            prepared_before_launch(self),
            PROMPT.into(),
            scope::Lineage::Created,
            (None, reply_once(None)),
        );
        let writer = writer.map(str::to_owned);
        let _hook = scope::install_launch(match writer {
            Some(writer) => Box::new(move |lineage, target, source, _, submit| {
                hook(lineage, target, source, Some(writer), submit)
            }),
            None => hook,
        });
        let _entered = self.runtime.enter();
        crate::services::claude::fresh_hold_for_tests(&self.transcript(), &self.name, owner, PROMPT)
    }

    /// The created session and its owner marker outlive an owner's settlement.
    fn kept(&self, owner: &Path) {
        self.f
            .tmux(&["has-session", "-t", &format!("={}", self.name)]);
        assert!(owner.exists(), "the owner marker stays");
    }
}

/// An owner refusal before any key (a tmux target binds no Herdr writer) keeps the original and
/// its proof; the launch neither observes nor tears the session or its marker down.
#[test]
fn owned_refusal_keeps_the_created_session_and_its_marker() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oc-refused", 7_272_517, "normal", view("", false), IDLE);
    let owner = rig.f.root.path().join("oc-refused-marker");
    fs::write(&owner, "owner").unwrap();
    let result = rig.fresh_owned(&owner, Some("/elsewhere.sock#w1:p1"));
    assert!(
        result.as_ref().is_err_and(|e| e.contains("NotSent")),
        "{result:?}"
    );
    rig.held_not_sent("only a Herdr owner binds a physical writer");
    assert_eq!(rig.bind(), TurnBind::NotArmed);
    rig.kept(&owner);
}

/// A created first prompt that fails after its first key stays Unknown and is never sent again;
/// the created session and its marker stay with the owner.
#[test]
fn owned_failure_after_a_key_keeps_the_created_session() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dialog = "Resume from summary\n❯ 1. Resume from summary\n  2. Resume full session\n\
        Enter to confirm · Esc to cancel\n";
    let rig = rig("oc-keyed", 7_272_518, "normal", dialog.into(), IDLE);
    fs::write(rig.renderer.join("body"), "").unwrap();
    let owner = rig.f.root.path().join("oc-keyed-marker");
    fs::write(&owner, "owner").unwrap();
    let result = rig.fresh_owned(&owner, None);
    assert!(result.is_err(), "{result:?}");
    assert_eq!(rig.events(), ["Enter"]);
    rig.uncertain();
    assert!(matches!(rig.bind(), TurnBind::Armed(_)));
    rig.kept(&owner);
}

/// A driver that ends well without handing its launch's first prompt over wrote nothing owned:
/// the turn is refused unsent and the hook does not outlive it; a driver's own error stays.
#[test]
fn unconsumed_launch_hook_is_not_owned() {
    use super::super::CLAUDE_LEGACY_PROBE;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oc-unowned", 7_272_519, "normal", view("", false), IDLE);
    for (driver, expected) in [
        (Ok(()), "NotSent(\"launch_not_owned\")"),
        (Err("driver failed".to_string()), "driver failed"),
    ] {
        *CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((rig.channel, Box::new(move || driver)));
        let result = rig.dispatch(
            ProviderKind::Claude,
            target::Phase::Absent,
            &rig.transcript(),
        );
        assert_eq!(result, Err(expected.to_string()));
        assert_eq!(scope::launch_refuses_live(), Ok(()), "hook slot cleared");
        let snapshot = rig.snapshot();
        assert!(snapshot["head_injection"].is_null(), "{snapshot}");
        assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
        assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
        assert_eq!(rig.bind(), TurnBind::NotArmed);
    }
    assert!(rig.events().is_empty(), "{:?}", rig.events());
}

/// Through `execute`, a create route's owner takes only a created execution: a driver handing over
/// an existing one is refused before any reservation and nothing is written.
#[test]
fn a_create_route_refuses_an_existing_hand_over() {
    use super::super::CLAUDE_LEGACY_PROBE;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oc-existing", 7_272_520, "normal", view("", false), IDLE);
    let handed: super::super::ClaudeLegacyProbe = Box::new(|| {
        let hook = scope::launch_hook().ok_or("no owner hook")?;
        let submit: scope::CreatedSubmit = Box::new(|_| Ok(()));
        hook(scope::Lineage::Existing, json!({}), json!({}), None, submit)
    });
    *CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((rig.channel, handed));
    let result = rig.dispatch(
        ProviderKind::Claude,
        target::Phase::Absent,
        &rig.transcript(),
    );
    assert_eq!(result, Err("NotSent(\"lineage_refused: Existing\")".into()));
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    assert_eq!(rig.bind(), TurnBind::NotArmed);
    assert!(rig.events().is_empty(), "{:?}", rig.events());
}

/// The turn's inflight row as the running turn records it: its pane and live transcript.
fn running_row(rig: &Rig, pane: &str) {
    let transcript = rig.transcript().display().to_string();
    let row = crate::services::discord::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        rig.channel,
        None,
        7,
        1,
        2,
        "go".into(),
        Some(format!("sid-{}", rig.channel)),
        Some(pane.into()),
        Some(transcript),
        None,
        0,
    );
    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
}

/// A front handed into the running turn reaches the pane its inflight row names, once; without a
/// live busy pane the grant stays unspent and no key is sent.
#[test]
fn a_busy_front_is_written_into_the_pane_its_running_turn_names() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("hb-busy", 7_272_521, "normal", view("", false), BUSY);
    let hand = |front: Front| {
        let owner = (rig.mailbox.clone(), None);
        let handed = hand_busy(owner, ProviderKind::Claude, (rig.channel, front));
        rig.runtime.block_on(handed)
    };
    let unobserved = Err("NotSent(\"busy_turn_unobserved\")".to_string());
    assert_eq!(hand(rig.granted_front()), unobserved, "no running turn row");
    running_row(&rig, "hb-gone");
    assert_eq!(hand(rig.granted_front()), unobserved, "its pane is gone");
    assert!(rig.events().is_empty(), "no key: {:?}", rig.events());
    let outstanding = rig
        .mailbox
        .v2_request(|slot| Ok(slot.as_ref().unwrap().front_outstanding()));
    assert_eq!(
        rig.runtime.block_on(outstanding),
        Ok(true),
        "the grant is unspent"
    );
    running_row(&rig, &rig.name);
    assert_eq!(hand(rig.granted_front()), Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    let pasted = fs::read_to_string(rig.renderer.join("body")).unwrap();
    assert!(pasted.contains(PROMPT), "{pasted}");
}

/// A busy front carries its whole original into the running turn: its upload and reply context
/// reach the pane once with its text, and uploads that cannot be prepared leave it queued, unsent.
#[test]
fn a_busy_front_carries_its_uploads_and_reply_into_the_pane() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("hb-whole", 7_272_595, "normal", view("", false), BUSY);
    running_row(&rig, &rig.name);
    let hand = |front: Front| {
        let owner = (rig.mailbox.clone(), None);
        let handed = hand_busy(owner, ProviderKind::Claude, (rig.channel, front));
        rig.runtime.block_on(handed)
    };
    assert_eq!(hand(rig.granted_front()), Ok(()));
    let next = |offset: u64, uploads: Vec<&str>| {
        let mut item = rig.original.clone();
        item.message_id += offset;
        item.source_message_ids = vec![item.message_id];
        item.reply_context = Some("REPLY-CONTEXT".into());
        item.pending_uploads = uploads.into_iter().map(Into::into).collect();
        let admitted = rig
            .runtime
            .block_on(admit(&rig.mailbox, &item, AdmissionTicket::new()));
        assert!(matches!(
            admitted,
            crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
        ));
    };
    next(1, vec!["UPLOAD-RECORD"]);
    assert_eq!(hand(rig.granted_front()), Ok(()));
    let pasted = fs::read_to_string(rig.renderer.join("body")).unwrap();
    for part in ["UPLOAD-RECORD", "REPLY-CONTEXT", PROMPT] {
        assert_eq!(pasted.matches(part).count(), 1, "{part}: {pasted}");
    }
    let too_many = crate::services::cluster::attachment_transfer::store::MAX_FILES + 1;
    next(2, vec!["UPLOAD-RECORD"; too_many]);
    let events = rig.events().len();
    let failed = hand(rig.granted_front());
    assert_eq!(
        failed,
        Err("NotSent(\"upload_materialization_failed\")".into())
    );
    assert_eq!(rig.events().len(), events, "no key");
    assert_eq!(
        rig.snapshot()["items"].as_array().unwrap().len(),
        1,
        "it stays queued"
    );
}

/// One author's consecutive input at the front goes into a busy turn as one prompt in order and one
/// attempt; input that never merges, or arrives after the claim, waits for the next.
#[test]
fn consecutive_input_goes_into_a_busy_turn_as_one_prompt() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = &rig("hb-unit", 7_272_596, "normal", view("", false), BUSY);
    running_row(rig, &rig.name);
    let hand = |front: Front| {
        let owner = (rig.mailbox.clone(), None);
        let handed = hand_busy(owner, ProviderKind::Claude, (rig.channel, front));
        rig.runtime.block_on(handed)
    };
    assert_eq!(hand(rig.granted_front()), Ok(()));
    let queue = |offset: u64, text: &str, merge: bool| {
        let mut item = rig.original.clone();
        item.message_id += offset;
        item.source_message_ids = vec![item.message_id];
        (item.text, item.merge_consecutive) = (text.into(), merge);
        let ticket = AdmissionTicket::new();
        let admitted = rig
            .runtime
            .block_on(admit(&rig.mailbox, &item, ticket.clone()));
        assert!(matches!(
            admitted,
            crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
        ));
        move || {
            rig.runtime
                .block_on(exact_turn(&rig.mailbox, &ticket, &item))
                .unwrap()
        }
    };
    let (a, b) = (queue(1, "UNIT-A", true), queue(2, "UNIT-B", true));
    let _never_merges = queue(3, "UNIT-C", false);
    assert_eq!(hand(rig.granted_front()), Ok(()));
    let pasted = fs::read_to_string(rig.renderer.join("body")).unwrap();
    let at = |part: &str| (pasted.matches(part).count(), pasted.find(part));
    assert!(
        matches!((at("UNIT-A"), at("UNIT-B")), ((1, Some(x)), (1, Some(y))) if x < y),
        "{pasted}"
    );
    assert!(!pasted.contains("UNIT-C"), "{pasted}");
    assert!(matches!(a(), TurnBind::Settled(_)), "{:?}", a());
    assert_eq!(a(), b(), "one attempt settles both");
    assert_eq!(
        hand(rig.granted_front()),
        Ok(()),
        "the unmerged input goes next"
    );
    let f = queue(4, "UNIT-F", true);
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let g = queue(5, "UNIT-G", true);
    let transcript = rig.transcript();
    let busy = target::Phase::Busy;
    assert_eq!(
        rig.execute(claim, ProviderKind::Claude, busy, &transcript),
        Ok(())
    );
    assert!(matches!(f(), TurnBind::Settled(_)), "{:?}", f());
    assert_eq!(
        g(),
        TurnBind::NotArmed,
        "input after the claim waits for the next unit"
    );
    assert_eq!(rig.snapshot()["items"].as_array().unwrap().len(), 1);
}

/// With no turn running the drive hands each front to the idle start, never into a pane.
#[test]
fn an_idle_channel_hands_its_front_to_the_turn_start() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("hb-idle", 7_272_522, "normal", view("", false), IDLE);
    let fronts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = fronts.clone();
    let idle: crate::services::discord::queue_io::input_owner::drive::Handoff =
        Arc::new(move |front: Front| {
            seen.lock().unwrap().push(front);
            Box::pin(async { Err("left queued".into()) })
        });
    let owner = (rig.mailbox.clone(), None);
    let drive = owner_drive(owner, (ProviderKind::Claude, rig.channel), idle);
    let _entered = rig.runtime.enter();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    for _ in 0..200 {
        if !fronts.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    let fronts = fronts.lock().unwrap();
    assert_eq!(fronts.len(), 1, "one look while the front stays put");
    assert_eq!(fronts[0].1.text, PROMPT);
    assert!(rig.events().is_empty(), "no key: {:?}", rig.events());
}

/// An owner idle turn binds its fresh cancel token to the pane it submits into, as the warm
/// follow-up does: a non-force stop reads only that binding to reach the native AI.
#[test]
fn an_owner_idle_turn_binds_its_cancel_token_to_its_pane() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let providers = [
        (ProviderKind::Claude, 7_272_591),
        (ProviderKind::Codex, 7_272_592),
    ];
    for (provider, channel) in providers {
        let rig = rig(
            &format!("oi-bind-{channel}"),
            channel,
            "normal",
            view("", false),
            IDLE,
        );
        let claim = rig.claim(&rig.granted_front()).unwrap();
        let (settled, _) = tokio::sync::oneshot::channel();
        let mailbox = rig.mailbox.clone();
        let owner = OwnerTurn {
            mailbox,
            claim,
            settled,
        };
        let mut bound = None;
        let _ = rig.with_turn(&provider, Some(&rig.name), |turn, sender| {
            let cancel = Arc::clone(&turn.cancel);
            assert_eq!(cancel.tmux_session_name(), None, "a fresh token");
            let _ = execute_owned(turn, sender, owner);
            bound = cancel.tmux_session_name();
            Ok(())
        });
        assert_eq!(bound.as_deref(), Some(rig.name.as_str()), "{provider:?}");
    }
}

/// A routine, scheduled or HTTP start completes only on its own turn: on a busy channel the drive
/// leaves it unclaimed at the front for the idle start, while a Discord front goes to the busy turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_busy_channel_leaves_a_start_for_its_own_turn() {
    use crate::services::discord::queue_io::input_owner::admission::admit_with_context;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let mut handed = Vec::new();
    for (channel, start) in [(7_272_593_u64, true), (7_272_594, false)] {
        let mailbox = registry.handle(ChannelId::new(channel));
        let path = dir.join(format!("claude/owner-start/{channel}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        mailbox.v2_adopt(path, "input-owner".into()).await.unwrap();
        let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
            "message_id":channel,"channel_id":channel,"source_message_ids":[channel],
            "queued_generation":1,"text":PROMPT}))
        .unwrap();
        let ticket = AdmissionTicket::new();
        let admitted = match start {
            true => admit_with_context(&mailbox, &original, ticket, json!({"routine":1})).await,
            false => admit(&mailbox, &original, ticket).await,
        };
        assert!(matches!(
            admitted,
            crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
        ));
        let token = Arc::new(CancelToken::new());
        mailbox
            .restore_active_turn(token, UserId::new(7), MessageId::new(9))
            .await;
        let idle: crate::services::discord::queue_io::input_owner::drive::Handoff =
            Arc::new(|_| Box::pin(async { Err("idle".into()) }));
        let drive = owner_drive(
            (mailbox.clone(), None),
            (ProviderKind::Claude, channel),
            idle,
        );
        let front = mailbox.v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            owner.grant_front(&front)?;
            Ok(front)
        });
        let result = (drive.handoff)(front.await.unwrap()).await;
        let outstanding = mailbox.v2_request(|slot| Ok(slot.as_ref().unwrap().front_outstanding()));
        handed.push((result, outstanding.await));
    }
    let waits = Err("NotSent(\"synthetic_waits_idle\")".to_string());
    assert_eq!(
        handed[0],
        (waits, Ok(true)),
        "the start stays granted, unclaimed"
    );
    let busy = Err("NotSent(\"busy_turn_unobserved\")".to_string());
    assert_eq!(
        handed[1].0, busy,
        "a Discord front is handed into the busy turn"
    );
}

#[path = "owner_input_not_sent_tests.rs"]
mod not_sent;

#[path = "owner_provenance_tests.rs"]
mod provenance;
