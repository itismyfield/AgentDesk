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

const PROMPT: &str = "owner busy question";
const AUTHOR: &str = "7";
const NAME: &str = "tester";
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

struct Rig {
    f: Fixture,
    name: String,
    channel: u64,
    nonce: String,
    renderer: std::path::PathBuf,
    program: std::path::PathBuf,
    runtime: tokio::runtime::Runtime,
    _registry: ChannelMailboxRegistry,
    mailbox: ChannelMailboxHandle,
    ticket: AdmissionTicket,
    original: PendingQueueItem,
}

/// A known pane over `transcript` and an owner with one admitted original; its tmux fails a load
/// on `fail-load` and replaces the pane's process after the step named in `swap-after`.
fn rig(name: &str, channel: u64, mode: &str, before: String, transcript: &str) -> Rig {
    let f = Fixture::after_shared_test_env_lock();
    let nonce = format!("{:032x}", channel);
    let shown =
        crate::services::claude_tui::busy_inject::frame("discord", AUTHOR, NAME, &nonce, PROMPT);
    let renderer = renderer_with(&f, name, mode, before, transcript, &shown);
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
           rm \"$d/swap-after\"; '{real}' -L '{socket}' -f /dev/null respawn-pane -k -t '{pane}'\nfi\nexit $rc\n",
        dir = dir.display(),
        shim = f.root.path().join("tmux").display(),
        real = f.real.display(),
        socket = f.socket,
    );
    fs::write(&program, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let context = json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),
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
    fn mark(&self, file: &str, value: &str) {
        let dir = self.f.root.path().join(format!("{}-owner", self.name));
        fs::write(dir.join(file), value).unwrap();
    }

    /// One turn through `execute` with this rig's owner registered for its channel.
    fn dispatch(
        &self,
        provider: ProviderKind,
        phase: target::Phase,
        transcript: &Path,
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
                execution: Execution::LocalTui,
                origin: InputOrigin::Discord,
                phase,
                attempt: json!({"nonce":self.nonce,"provider":provider.as_str(),
                    "target":{"host":"tmux","owner":self.name,"root":root,"execution_nonce":self.nonce},
                    "source":source}),
                sources: vec![self.original.message_id.to_string()],
                transcript: transcript.to_path_buf(),
                program: self.program.clone(),
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
            tmux_session_name: Some(&self.name),
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

    fn transcript(&self) -> std::path::PathBuf {
        self.renderer.join("transcript")
    }

    fn snapshot(&self) -> Value {
        self.runtime
            .block_on(self.mailbox.v2_request(|s| s.as_ref().unwrap().json()))
            .unwrap()
    }

    fn bind(&self) -> TurnBind {
        self.runtime
            .block_on(exact_turn(&self.mailbox, &self.ticket, &self.original))
            .unwrap()
    }

    /// The pane's record once the last key has had time to land.
    fn events(&self) -> Vec<String> {
        std::thread::sleep(Duration::from_millis(300));
        events(&self.renderer)
    }

    fn delivered(&self) {
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
    fn held_not_sent(&self, reason: &str) {
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
    fn uncertain(&self) {
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

/// A swallowed Enter on the owner's own paste gets main's one finishing Enter and no second
/// paste. Seen in the transcript it is delivered; never seen it is unconfirmed as main reports it:
/// one alert, the head left Unknown, nothing re-held or sent again.
#[test]
fn owner_busy_claude_confirms_once_without_requeue() {
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
    assert_eq!(result, Err("WriteUncertain(\"not_observed\")".into()));
    assert_eq!(
        rig.events(),
        ["paste", "Enter", "Enter"],
        "one paste, one finishing Enter"
    );
    rig.uncertain();
    assert!(matches!(rig.bind(), TurnBind::Armed(turn) if turn.attempt == rig.nonce));
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

/// Codex busy has no owner transaction before 4d-2: the owner keeps the original queued and
/// nothing, the Codex driver included, sends it.
#[test]
fn codex_busy_held_until_4d2() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("ob-codex", 7_272_512, "normal", view("", false), BUSY);
    let result = rig.dispatch(ProviderKind::Codex, target::Phase::Busy, &rig.transcript());
    assert_eq!(result, Err("NotSent(\"provider_unsupported\")".into()));
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
    assert!(rig.events().is_empty());
    assert_eq!(rig.bind(), TurnBind::NotArmed);
}
