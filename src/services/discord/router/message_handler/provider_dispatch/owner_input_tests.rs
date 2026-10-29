//! The owner branch from `provider_dispatch::execute`: a real pane under a traced tmux, the
//! owner's actor, and what reached the pane read from the renderer's own record.
use super::super::{StreamingTurn, execute};
use super::*;
use crate::services::claude_tui::busy_inject::{StashChannelGuard, Timing};
use crate::services::claude_tui::input::submission::tests::{events, renderer_with, view, wait};
use crate::services::discord::queue_io::input_owner::admission::{
    AdmissionTicket, TurnBind, admit, exact_turn,
};
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
    runtime: tokio::runtime::Runtime,
    _registry: ChannelMailboxRegistry,
    mailbox: ChannelMailboxHandle,
    ticket: AdmissionTicket,
    original: PendingQueueItem,
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
         '{shim}' \"$@\"; rc=$?\ncase \"$*\" in *load-buffer*) step=load ;; *paste-buffer*) step=paste ;; *C-s*) step=C-s ;; *) step= ;; esac\n\
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
        register(
            self.channel,
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
            },
        );
        let host = TurnHost::Tmux;
        let turn = StreamingTurn {
            pool: None,
            provider: &provider,
            prompt: PROMPT,
            session_id: None,
            working_dir: "/tmp",
            system_prompt: None,
            allowed_tools: &[],
            cancel: Arc::new(CancelToken::new()),
            codex_observation: None,
            remote_profile: None,
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
        let start =
            crate::services::discord::router::intake_dispatch::replay_admission::dormant_start(
                "test",
            )
            .unwrap();
        execute(turn, start, sender)
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

/// A listed channel's busy pane parks a visible draft once, pastes once and enters once, and an
/// empty composer gets no C-s; a modal gets no key and the original goes back to custody.
#[test]
fn owner_busy_claude_submits_once_and_hands_back_on_veto() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (name, channel, before, expected) in [
        (
            "ob-draft",
            7_272_501,
            view("js", false),
            vec!["C-s", "paste", "Enter"],
        ),
        (
            "ob-empty",
            7_272_502,
            view("", false),
            vec!["paste", "Enter"],
        ),
    ] {
        let _listed = StashChannelGuard::new(channel);
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
    rig.held_not_sent("vetoed: Modal");
    assert_eq!(rig.bind(), TurnBind::NotArmed);
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

/// A swallowed Enter gets main's one finishing Enter and no second paste, and is delivered seen or
/// not; an unseen receipt is one observation, and nothing is retained past the deadline.
#[test]
fn owner_busy_claude_enter_delivers_with_or_without_receipt() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig_seen = rig("oc-seen", 7_272_505, "swallow", view("", false), BUSY);
    let result = rig_seen.dispatch(
        ProviderKind::Claude,
        target::Phase::Busy,
        &rig_seen.transcript(),
    );
    assert_eq!(result, Ok(()));
    assert_eq!(rig_seen.events(), ["paste", "Enter", "Enter"]);
    rig_seen.delivered();

    let rig = rig("oc-missing", 7_272_506, "swallow", view("", false), BUSY);
    let elsewhere = rig.f.root.path().join("oc-missing-unwritten.jsonl");
    fs::write(&elsewhere, BUSY).unwrap();
    let result = rig.dispatch(ProviderKind::Claude, target::Phase::Busy, &elsewhere);
    assert_eq!(result, Ok(()));
    assert_eq!(
        rig.events(),
        ["paste", "Enter", "Enter"],
        "one paste, one finishing Enter"
    );
    rig.delivered();
    let past_deadline = rig
        .mailbox
        .v2_request(|s| s.as_mut().unwrap().tick(10_000 + 31_000));
    rig.runtime.block_on(past_deadline).unwrap();
    rig.delivered();
    let alerts: Vec<_> = crate::services::observability::events::recent(usize::MAX)
        .into_iter()
        .filter(|event| {
            event.event_type == "busy_inject_unconfirmed" && event.channel_id == Some(rig.channel)
        })
        .map(|event| event.payload["detail"].clone())
        .collect();
    assert_eq!(alerts, [json!("not_observed")]);
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
