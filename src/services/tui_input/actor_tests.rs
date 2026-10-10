#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::actor::gate::{PaneVerdict, judge_pane};
use super::actor::pane::{MAX_PROMPT_BYTES, Pane, SendOutcome, TmuxPane};
use super::actor::{ACCEPT_WINDOW, InputActor, READY_WINDOW, Step};
use super::ledger::Ledger;
use super::rows::{DoneReason, Entry, HeldReason, Owner, RowState};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};
use crate::services::tui_o::writer::input_facts::{ChannelFact, TurnState};

const CHANNEL: u64 = 77;

const CLAUDE_READY: &str = "\
⏺ Done.

────────────────────────────────────────────────────────────
❯\u{00a0}
────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)";

// The /effort overlay over a composer that otherwise reads as ready.
const CLAUDE_EFFORT_OVERLAY: &str = "\
⏺ Done.

  Effort: medium
  ←/→ to adjust
────────────────────────────────────────────────────────────
❯\u{00a0}
────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)";

const CLAUDE_UPDATE_SCREEN: &str = "\
 ✻ Claude Code v9.9.9 is available

   A new version was installed. Restart to use it.

   Press Enter to continue";

const CODEX_READY: &str = "\
• previous response

›

  gpt-5.5 · gpt-5.5 xhigh · ~/.adk/release/workspaces/agentdesk · agentdesk · main";

// Current approval controls directly adjoin the composer.
const CODEX_APPROVAL: &str = "\
• previous response

Approval required
  rm -rf build
  [y] yes  [n] no
›

  gpt-5.5 · gpt-5.5 xhigh · ~/.adk/release/workspaces/agentdesk · agentdesk · main";

const CODEX_UPDATE_SCREEN: &str = "\
✨ Update available! 0.46.0 -> 0.47.0

› 1. Update now (runs `npm install -g @openai/codex`)
  2. Skip
  3. Skip until next version

  Press enter to continue";

// A start screen no detector knows: no composer and no modal wording.
const CODEX_UNKNOWN_START: &str = "\
>_ OpenAI Codex (v9.9.9)

  Pick how this workspace starts
› 1. Continue
  2. Exit

  Press enter to continue";

struct World {
    _dir: TempDir,
    runtime: PathBuf,
    transcript: PathBuf,
    binding: SourceBinding,
}

impl World {
    fn new(provider: ShadowProvider) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("actor-")
            .tempdir_in(root)
            .unwrap();
        let runtime = dir.path().join("runtime");
        let transcript = dir.path().join("session.jsonl");
        fs::write(&transcript, "{\"type\":\"summary\"}\n").unwrap();
        let source = source_id_for("session", &transcript).unwrap();
        let binding = SourceBinding {
            channel_id: CHANNEL,
            provider,
            source,
        };
        Self {
            _dir: dir,
            runtime,
            transcript,
            binding,
        }
    }

    fn ledger(&self, inputs: &[(u64, &str)]) -> Ledger {
        self.ledger_for(CHANNEL, inputs)
    }

    fn ledger_for(&self, channel: u64, inputs: &[(u64, &str)]) -> Ledger {
        let mut ledger = Ledger::open(&self.runtime, channel).unwrap();
        for (key, text) in inputs {
            let input = json!({ "text": text });
            ledger
                .append_entry(&Entry::Received { key: *key, input }, &[])
                .unwrap();
        }
        ledger
    }

    fn append(&self, record: Value) -> u64 {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.transcript)
            .unwrap();
        writeln!(file, "{record}").unwrap();
        fs::metadata(&self.transcript).unwrap().len()
    }

    fn user(&self, text: &str) -> u64 {
        self.append(match self.binding.provider {
            ShadowProvider::Claude => json!({
                "type": "user",
                "uuid": uuid::Uuid::new_v4().to_string(),
                "message": { "role": "user", "content": text },
            }),
            ShadowProvider::Codex => json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": text }],
                },
            }),
        })
    }

    fn fact(&self, state: TurnState) -> ChannelFact {
        ChannelFact {
            binding: self.binding.clone(),
            through: fs::metadata(&self.transcript).unwrap().len(),
            state,
        }
    }

    fn idle(&self) -> ChannelFact {
        self.fact(TurnState::Idle)
    }
}

fn open_turn() -> TurnState {
    TurnState::Open {
        native_turn_id: None,
    }
}

fn state_of(ledger: &Ledger, key: u64) -> RowState {
    ledger.rows().unwrap().row(key).unwrap().state
}

fn owner_of(ledger: &Ledger, key: u64) -> Owner {
    ledger.rows().unwrap().owner(key)
}

// A scripted pane; `on_submit` stands in for the TUI writing the user record.
struct FakePane {
    screen: String,
    outcomes: VecDeque<SendOutcome>,
    submitted: Vec<String>,
}

impl FakePane {
    fn new(screen: &str) -> Self {
        Self {
            screen: screen.to_string(),
            outcomes: VecDeque::new(),
            submitted: Vec::new(),
        }
    }
}

impl Pane for FakePane {
    fn execution_nonce(&self) -> Option<String> {
        Some("test-nonce".into())
    }
    fn capture(&mut self) -> Result<String, String> {
        Ok(self.screen.clone())
    }

    fn submit(&mut self, text: &str) -> SendOutcome {
        self.submitted.push(text.to_string());
        self.outcomes.pop_front().unwrap_or(SendOutcome::Sent)
    }
}

#[tokio::test]
async fn input_responsibility_is_released_only_after_its_user_record_and_idle() {
    let world = World::new(ShadowProvider::Claude);
    let mut ledger = world.ledger(&[(1, "deploy the fix"), (2, "then run checks")]);
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    let t0 = Instant::now();

    let step = actor.step(&mut ledger, Some(&world.idle()), t0).await;
    assert_eq!(step.unwrap(), Step::Moved(1, RowState::AwaitTurn));

    // A stale Idle, an anonymous turn and a foreign prompt never release the input.
    let foreign_turn = world.fact(open_turn());
    let other = world.binding.source.clone();
    let mut rebound = world.idle();
    rebound.binding.source.session_id = format!("{}-next", other.session_id);
    for fact in [Some(world.idle()), Some(foreign_turn), Some(rebound)] {
        world.user("someone typed in the pane");
        let step = actor.step(&mut ledger, fact.as_ref(), t0).await.unwrap();
        assert_eq!(step, Step::Wait("awaiting_user_record"));
        assert_eq!(state_of(&ledger, 1), RowState::AwaitTurn);
        assert_eq!(owner_of(&ledger, 1), Owner::Ledger);
    }

    let end = world.user(&actor.pane_submitted()[0]);
    let opened = world.fact(open_turn());
    let step = actor.step(&mut ledger, Some(&opened), t0).await.unwrap();
    assert_eq!(step, Step::Moved(1, RowState::Running));

    // An Idle read before our own record does not close the turn it opened.
    let mut early = world.idle();
    early.through = end - 1;
    let step = actor.step(&mut ledger, Some(&early), t0).await.unwrap();
    assert_eq!(step, Step::Wait("turn_open"));
    assert_eq!(owner_of(&ledger, 1), Owner::Ledger);

    world.append(json!({"type":"system","subtype":"turn_duration"}));
    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Moved(1, RowState::Done(DoneReason::Completed)));
    assert_eq!(owner_of(&ledger, 1), Owner::Settled);
    assert_eq!(actor.pane_submitted(), [frame(1, "deploy the fix")]);

    // Only then does the next input reach the pane.
    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Moved(2, RowState::AwaitTurn));
    assert_eq!(
        actor.pane_submitted(),
        [frame(1, "deploy the fix"), frame(2, "then run checks")]
    );
}

#[tokio::test]
async fn unconfirmed_or_indeterminate_inputs_are_never_injected_again() {
    let world = World::new(ShadowProvider::Claude);
    let t0 = Instant::now();

    // A turn that opens and closes after Enter without our record leaves the input Unaccepted.
    let mut ledger = world.ledger_for(1, &[(1, "status?")]);
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    let opened = world.fact(open_turn());
    actor.step(&mut ledger, Some(&opened), t0).await.unwrap();
    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Moved(1, RowState::Unaccepted));
    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Blocked(1, RowState::Unaccepted));
    assert_eq!(actor.pane_submitted(), [frame(1, "status?")]);

    // Silence for the whole accept window has the same outcome.
    let mut ledger = world.ledger_for(2, &[(1, "quiet")]);
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    let before = actor.entered_at() + ACCEPT_WINDOW - Duration::from_millis(1);
    let step = actor.step(&mut ledger, Some(&world.idle()), before).await;
    assert_eq!(step.unwrap(), Step::Wait("awaiting_user_record"));
    let late = actor.entered_at() + ACCEPT_WINDOW;
    let step = actor.step(&mut ledger, Some(&world.idle()), late).await;
    assert_eq!(step.unwrap(), Step::Moved(1, RowState::Unaccepted));
    assert_eq!(owner_of(&ledger, 1), Owner::Ledger);

    // A not-sent input is offered again; an indeterminate one is held for good.
    let mut ledger = world.ledger_for(3, &[(1, "retry me")]);
    let mut pane = FakePane::new(CLAUDE_READY);
    pane.outcomes = VecDeque::from([
        SendOutcome::NotSent("load-buffer failed".into()),
        SendOutcome::Indeterminate("paste timed out".into()),
    ]);
    let mut actor = InputActor::new(world.binding.clone(), pane);
    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Moved(1, RowState::Ready));
    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Moved(1, RowState::Held(HeldReason::Ambiguous)));
    for _ in 0..3 {
        let step = actor
            .step(&mut ledger, Some(&world.idle()), t0)
            .await
            .unwrap();
        assert_eq!(
            step,
            Step::Blocked(1, RowState::Held(HeldReason::Ambiguous))
        );
    }
    assert_eq!(
        actor.pane_submitted(),
        [frame(1, "retry me"), frame(1, "retry me")]
    );
    assert_eq!(owner_of(&ledger, 1), Owner::Ledger);

    // A restarted actor holds no paste anchor for an in-flight row, so it holds the row.
    for (channel, state) in [(4, RowState::Injecting), (5, RowState::AwaitTurn)] {
        let mut ledger = world.ledger_for(channel, &[(1, "in flight")]);
        ledger
            .append_entry(
                &Entry::Transition {
                    key: 1,
                    state,
                    attempt: None,
                },
                &[],
            )
            .unwrap();
        let mut restarted = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
        let step = restarted.step(&mut ledger, Some(&world.idle()), t0).await;
        let held = RowState::Held(HeldReason::Ambiguous);
        assert_eq!(step.unwrap(), Step::Moved(1, held));
        assert!(restarted.pane_submitted().is_empty());
    }
}

async fn idle_step(
    actor: &mut InputActor<FakePane>,
    ledger: &mut Ledger,
    world: &World,
    at: Instant,
) -> Step {
    actor.step(ledger, Some(&world.idle()), at).await.unwrap()
}

#[tokio::test]
async fn modal_and_unknown_screens_never_receive_input() {
    let t0 = Instant::now();
    // Modal screens are held at once; unknown screens wait and are held only after READY_WINDOW.
    let cases = [
        (
            ShadowProvider::Claude,
            CLAUDE_EFFORT_OVERLAY,
            PaneVerdict::Modal,
        ),
        (ShadowProvider::Codex, CODEX_APPROVAL, PaneVerdict::Modal),
        (
            ShadowProvider::Codex,
            CODEX_UPDATE_SCREEN,
            PaneVerdict::Modal,
        ),
        (
            ShadowProvider::Claude,
            CLAUDE_UPDATE_SCREEN,
            PaneVerdict::NotReady,
        ),
        (
            ShadowProvider::Codex,
            CODEX_UNKNOWN_START,
            PaneVerdict::Modal,
        ),
    ];
    for (provider, screen, verdict) in cases {
        assert_eq!(judge_pane(provider, screen), verdict, "{screen}");
        let world = World::new(provider);
        let mut ledger = world.ledger(&[(1, "hello")]);
        let mut actor = InputActor::new(world.binding.clone(), FakePane::new(screen));
        let held = if verdict == PaneVerdict::Modal {
            assert_eq!(
                idle_step(&mut actor, &mut ledger, &world, t0).await,
                Step::Moved(1, RowState::Held(HeldReason::Modal))
            );
            HeldReason::Modal
        } else {
            for at in [t0, t0 + READY_WINDOW / 2] {
                assert_eq!(
                    idle_step(&mut actor, &mut ledger, &world, at).await,
                    Step::Wait("pane_not_ready"),
                    "{screen}"
                );
            }
            let at = t0 + READY_WINDOW;
            let held = RowState::Held(HeldReason::NotReady);
            assert_eq!(
                idle_step(&mut actor, &mut ledger, &world, at).await,
                Step::Moved(1, held),
                "{screen}"
            );
            HeldReason::NotReady
        };
        let blocked = Step::Blocked(1, RowState::Held(held));
        assert_eq!(
            idle_step(&mut actor, &mut ledger, &world, t0).await,
            blocked,
            "{screen}"
        );
        assert!(actor.pane_submitted().is_empty(), "{screen}");
        assert_eq!(owner_of(&ledger, 1), Owner::Ledger);
    }
    // The ready screens the modal fixtures extend do accept input.
    assert_eq!(
        judge_pane(ShadowProvider::Claude, CLAUDE_READY),
        PaneVerdict::Ready
    );
    assert_eq!(
        judge_pane(ShadowProvider::Codex, CODEX_READY),
        PaneVerdict::Ready
    );
}

#[tokio::test]
async fn codex_busy_and_unknown_without_controls_wait_before_being_held() {
    for screen in [
        "unrecognized screen",
        "• Working (1s • esc to interrupt)

›

  gpt-5.5 · gpt-5.5 xhigh · /tmp · repo · main",
    ] {
        assert_eq!(
            judge_pane(ShadowProvider::Codex, screen),
            PaneVerdict::NotReady
        );
        let world = World::new(ShadowProvider::Codex);
        let mut ledger = world.ledger(&[(1, "hello")]);
        let mut actor = InputActor::new(world.binding.clone(), FakePane::new(screen));
        let t0 = Instant::now();
        for at in [t0, t0 + READY_WINDOW / 2] {
            assert_eq!(
                idle_step(&mut actor, &mut ledger, &world, at).await,
                Step::Wait("pane_not_ready")
            );
        }
        assert_eq!(
            idle_step(&mut actor, &mut ledger, &world, t0 + READY_WINDOW).await,
            Step::Moved(1, RowState::Held(HeldReason::NotReady))
        );
        assert!(actor.pane_submitted().is_empty());
    }
}

struct FakeTmux {
    dir: TempDir,
}

impl FakeTmux {
    // `behavior` maps a tmux subcommand to a shell action; the pane screen is `screen`.
    fn new(screen: &str, behavior: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        fs::write(dir.path().join("screen"), screen).unwrap();
        let log = dir.path().join("log");
        let script = format!(
            "#!/bin/sh\necho \"$2\" >> '{log}'\ncase \"$2\" in\n{behavior}\n\
             capture-pane) cat '{screen}' ;;\nesac\nexit 0\n",
            log = log.display(),
            screen = dir.path().join("screen").display(),
        );
        let script = script.replace("capture-pane)", &format!("load-buffer) for last do :; done; cp \"$last\" '{}/buffer' ;;\npaste-buffer) {{ printf '────────────────────\n❯ '; cat '{}/buffer'; printf '\n────────────────────\n'; }} > '{}/screen' ;;\ncapture-pane)", dir.path().display(), dir.path().display(), dir.path().display()));
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    // Wide enough that the non-hanging steps finish on a loaded host; only `sleep 30` times out.
    fn pane(&self) -> TmuxPane {
        let mut pane = TmuxPane::with_program(
            &format!("s-{}", self.dir.path().display()),
            self.dir.path().join("tmux"),
            Duration::from_secs(3),
        );
        pane.attest_test_nonce("test-nonce");
        pane
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[tokio::test]
async fn bounded_tmux_separates_not_sent_from_indeterminate() {
    let cases = [
        ("load-buffer) exit 1 ;;", "not-sent"),
        ("paste-buffer) exit 1 ;;", "indeterminate"),
        ("paste-buffer) exec sleep 30 ;;", "indeterminate"),
        ("send-keys) exit 1 ;;", "indeterminate"),
        ("send-keys) exec sleep 30 ;;", "indeterminate"),
        ("", "sent"),
    ];
    for (behavior, expected) in cases {
        let tmux = FakeTmux::new(CLAUDE_READY, behavior);
        let outcome = tmux
            .pane()
            .with_composer(|pane| pane.submit("hello"))
            .unwrap();
        let kind = match outcome {
            SendOutcome::Sent => "sent",
            SendOutcome::NotSent(_) => "not-sent",
            SendOutcome::Indeterminate(_) => "indeterminate",
            SendOutcome::Refused(_) => "refused",
        };
        assert_eq!(kind, expected, "{behavior}: {outcome:?}");
    }

    let missing = PathBuf::from("/nonexistent/agentdesk-tmux");
    let mut pane = TmuxPane::with_program("s", missing, Duration::from_millis(300));
    assert!(matches!(pane.submit("hello"), SendOutcome::NotSent(_)));
    assert!(pane.capture().is_err());

    // An oversized prompt is refused before any tmux call.
    let tmux = FakeTmux::new(CLAUDE_READY, "");
    let oversized = "x".repeat(MAX_PROMPT_BYTES + 1);
    assert!(matches!(
        tmux.pane().submit(&oversized),
        SendOutcome::Refused(_)
    ));
    assert!(tmux.calls().is_empty());
}

#[tokio::test]
async fn indeterminate_tmux_paste_is_held_and_never_pasted_again() {
    let world = World::new(ShadowProvider::Claude);
    let tmux = FakeTmux::new(
        CLAUDE_READY,
        "paste-buffer) printf foreign > \"$(dirname \"$0\")/screen\"; exit 1 ;;",
    );
    let mut ledger = world.ledger(&[(1, "hello")]);
    let mut actor = InputActor::new(world.binding.clone(), tmux.pane());
    let t0 = Instant::now();
    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Moved(1, RowState::Held(HeldReason::Ambiguous)));
    for _ in 0..2 {
        actor
            .step(&mut ledger, Some(&world.idle()), t0)
            .await
            .unwrap();
    }
    let pastes = tmux.calls().iter().filter(|c| *c == "paste-buffer").count();
    assert_eq!(pastes, 1);
    assert_eq!(owner_of(&ledger, 1), Owner::Ledger);
}

impl InputActor<FakePane> {
    fn pane_submitted(&self) -> Vec<String> {
        self.pane().submitted.clone()
    }
}

fn frame(key: u64, text: &str) -> String {
    format!("[adk:source:{key}]\n{text}\n[adk:end]")
}

#[test]
fn actual_binding_context_mismatch_and_post_paste_replacement_veto_enter() {
    use crate::services::tui_prompt_dedupe::binding_context::{
        PreparedIncarnation, tests::fixture,
    };
    let (_root, _env) = fixture();
    let world = World::new(ShadowProvider::Claude);
    let tmux = FakeTmux::new(CLAUDE_READY, "");
    let session = format!("binding-input-{}", uuid::Uuid::new_v4().simple());
    let prepared =
        PreparedIncarnation::prepare("claude", &session, Some(CHANNEL), Some("session"), false)
            .unwrap();
    crate::services::discord::stamp_spawn_markers(&session, Some(&prepared)).unwrap();
    let mut pane = TmuxPane::with_program(
        &session,
        tmux.dir.path().join("tmux"),
        Duration::from_secs(3),
    );
    assert_eq!(
        pane.binding_nonce(&world.binding),
        Some(prepared.context.execution_nonce.clone())
    );
    for field in [
        "schema",
        "provider",
        "execution_nonce",
        "tmux_session",
        "channel_id",
        "expected_native_session_id",
    ] {
        let mut context = serde_json::to_value(&prepared.context).unwrap();
        context[field] = match field {
            "schema" => json!(2),
            "channel_id" => json!(CHANNEL + 1),
            _ => json!("wrong"),
        };
        fs::write(&prepared.path, serde_json::to_vec(&context).unwrap()).unwrap();
        assert!(pane.binding_nonce(&world.binding).is_none(), "{field}");
    }
    fs::write(
        &prepared.path,
        serde_json::to_vec(&prepared.context).unwrap(),
    )
    .unwrap();
    let program = tmux.dir.path().join("tmux");
    let script = fs::read_to_string(&program).unwrap().replace(
        "paste-buffer) {",
        &format!(
            "paste-buffer) cp '{}/replacement' '{}' ; {{",
            tmux.dir.path().display(),
            prepared.path.display()
        ),
    );
    fs::write(&program, script).unwrap();
    let mut replaced = prepared.context.clone();
    replaced.expected_native_session_id = Some("next-session".into());
    fs::write(
        tmux.dir.path().join("replacement"),
        serde_json::to_vec(&replaced).unwrap(),
    )
    .unwrap();
    let outcome = pane
        .with_composer(|pane| pane.submit_for_binding("hello", &world.binding))
        .unwrap();
    assert!(matches!(outcome, SendOutcome::Indeterminate(_)));
    assert_eq!(
        tmux.calls()
            .iter()
            .filter(|call| *call == "paste-buffer")
            .count(),
        1
    );
    assert!(!tmux.calls().iter().any(|call| call == "send-keys"));
}

#[tokio::test]
async fn merged_sources_require_the_whole_exact_persisted_frame() {
    let world = World::new(ShadowProvider::Claude);
    let mut ledger = world.ledger(&[]);
    let rendered = "[adk:source:99]\n[adk:source:2]\nfirst\nsecond\n[adk:end]";
    ledger.append_entry(&Entry::Received { key: 99, input: json!({
        "text":"first\nsecond", "source_message_ids":[99,2], "rendered_prompt":rendered,
        "source_text_segments":[{"message_id":99,"text":"first"},{"message_id":2,"text":"second"}],
    }) }, &[]).unwrap();
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    let at = Instant::now();
    actor
        .step(&mut ledger, Some(&world.idle()), at)
        .await
        .unwrap();
    assert_eq!(actor.pane_submitted(), [rendered]);
    world.user(&frame(99, "first\nsecond"));
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.idle()), at)
            .await
            .unwrap(),
        Step::Wait("awaiting_user_record")
    );
    world.user(rendered);
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.idle()), at)
            .await
            .unwrap(),
        Step::Moved(99, RowState::Running)
    );
    assert_eq!(
        ledger
            .rows()
            .unwrap()
            .row(99)
            .unwrap()
            .attempt
            .as_ref()
            .unwrap()
            .source_ids,
        [99, 2]
    );
}

// Claude draws continuation rows two columns in.
fn composer(body: &str) -> String {
    let body = body.replace('\n', "\n  ");
    format!(
        "────────────────────\n❯ {body}\n────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    )
}

// A Claude pane captured after a busy-turn paste was left unentered: the second row drawn two
// columns in, the earlier submitted prompt above in scrollback.
const CLAUDE_TWO_ROW_PASTE: &str = "\
❯ E2E PR1 direct hold. First output exactly [E2E:PR1:pb1-c-s5d-pr1-074645:HOLD]
  then run in the foreground python3 -c 'import time; time.sleep(60)' then
  output exactly [E2E:PR1:pb1-c-s5d-pr1-074645:DONE]

⏺ [E2E:PR1:pb1-c-s5d-pr1-074645:DONE]

✻ Churned for 1m 4s · done 7:47 AM

────────────────────────────────────────────────────────────────────────────────
❯\u{00a0}[📱 adk-e2e-phase-b · 343742347365974026 · b9bf9a71]
  응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.
────────────────────────────────────────────────────────────────────────────────
  ⏱ 34m │ █░░░░░░░░░ │ 11% │ 105K/1.0M │ 📦️ 100% │ $0.43
  MCP: 2 │ Tools: 2 done
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

#[test]
fn a_flat_claude_paste_is_owned_only_as_drawn() {
    use super::actor::gate::own_draft;
    let frame = "[📱 adk-e2e-phase-b · 343742347365974026 · b9bf9a71]\n\
                 응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.";
    let owns = |capture: &str, frame: &str| own_draft(ShadowProvider::Claude, capture, frame, true);
    assert!(owns(CLAUDE_TWO_ROW_PASTE, frame));
    let ansi = CLAUDE_TWO_ROW_PASTE
        .replace("❯\u{00a0}", "\u{1b}[39m❯\u{00a0}")
        .replace("\n───", "\n\u{1b}[38;5;244m───");
    assert!(owns(&ansi, frame));
    let row = "\n  응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.";
    let second = |with: &str| CLAUDE_TWO_ROW_PASTE.replace(row, with);
    // A complete body differing only in whitespace uses the same wrap ownership proof.
    assert!(owns(&second(&row.replace("\n  ", "\n   ")), frame));
    let scrollback_only = CLAUDE_TWO_ROW_PASTE
        .replace(
            "❯\u{00a0}[📱 adk-e2e-phase-b · 343742347365974026 · b9bf9a71]",
            "❯\u{00a0}",
        )
        .replace(row, "")
        .replace(
            "\n\n✻",
            &format!("\n\n❯ {}\n\n✻", frame.replace('\n', "\n  ")),
        );
    for (case, capture) in [
        (
            "one character",
            second(&row.replace("출력해줘.", "출력해줘!")),
        ),
        ("typed after", second(&format!("{row}x"))),
        ("typed row", second(&format!("{row}\n  x"))),
        ("one-column indent", second(&row.replace("\n  ", "\n "))),
        ("unindented", second(&row.replace("\n  ", "\n"))),
        ("one row", second("")),
        ("scrollback only", scrollback_only),
    ] {
        assert!(!owns(&capture, frame), "{case}");
    }
    assert!(!owns(CLAUDE_TWO_ROW_PASTE, &format!("{frame}\nmore")));
    assert!(!own_draft(
        ShadowProvider::Claude,
        CLAUDE_TWO_ROW_PASTE,
        frame,
        false
    ));

    // Rows captured from Claude Code 2.1.293 at 80 columns: a line's own leading spaces stay
    // ahead of the indent, a blank line is an empty row, and three rows still render flat.
    let header = "[📱 s · a · n1]";
    let drawn = |rows: &[&str]| {
        let border = "─".repeat(80);
        format!(
            "{border}\n❯\u{00a0}{header}\n{}\n{border}\n",
            rows.join("\n")
        )
    };
    for (rows, lines) in [
        (
            vec!["     three leading spaces"],
            vec!["   three leading spaces"],
        ),
        (vec!["   one leading space"], vec![" one leading space"]),
        (vec!["", "  third"], vec!["", "third"]),
        (
            vec!["  line two", "  line three"],
            vec!["line two", "line three"],
        ),
    ] {
        let frame = format!("{header}\n{}", lines.join("\n"));
        assert!(owns(&drawn(&rows), &frame), "{frame}");
    }
    let lead = format!("{header}\nthree leading spaces");
    assert!(owns(&drawn(&["     three leading spaces"]), &lead));
    // Whole-body proof tolerates visual wrap boundaries while rejecting extra nonwhite text.
    for (first, rest, gap) in [
        (
            format!("{}abcdef", "abcdefghij".repeat(7)),
            format!("ghij{}", "abcdefghij".repeat(2)),
            "",
        ),
        (
            format!("{}가나다라마바사아", "가나다라마바사아자차".repeat(3)),
            format!("자차{}", "가나다라마바사아자차"),
            "",
        ),
        ("😀a".repeat(25), "😀a".repeat(15), ""),
        (
            ["word"; 15].join(" "),
            "word word word word word end".to_string(),
            " ",
        ),
    ] {
        let frame = format!("{header}\n{first}{gap}{rest}");
        let rows = [format!("  {first}"), format!("  {rest}")];
        assert!(owns(&drawn(&[&rows[0], &rows[1]]), &frame), "{frame}");
        let foreign = format!("{}x", rows[1]);
        assert!(!owns(&drawn(&[&rows[0], &foreign]), &frame), "{frame}");
    }
}

#[test]
fn folded_own_draft_requires_exact_k_empty_attestation_and_no_other_text() {
    use super::actor::gate::own_draft;
    for lines in [2, 20, 200, 2000] {
        let text = vec!["line"; lines].join("\n");
        let folded = format!("[Pasted text #42 +{} lines]", lines - 1);
        assert!(own_draft(
            ShadowProvider::Claude,
            &composer(&folded),
            &text,
            true
        ));
        assert!(!own_draft(
            ShadowProvider::Claude,
            &composer(&folded),
            &text,
            false
        ));
        for body in [
            format!("[Pasted text #42 +{lines} lines]"),
            format!("{folded}foreign"),
            format!("{folded}\n[Pasted text #43 +1 lines]"),
        ] {
            assert!(
                !own_draft(ShadowProvider::Claude, &composer(&body), &text, true),
                "{body}"
            );
        }
    }
    for count in [799, 800, 801] {
        let text = "x".repeat(count);
        assert_eq!(
            own_draft(
                ShadowProvider::Claude,
                &composer("[Pasted text #7]"),
                &text,
                true
            ),
            count > 800
        );
    }
    assert!(!own_draft(
        ShadowProvider::Codex,
        "› [Pasted Content 900 chars]",
        "x",
        true
    ));
}

#[test]
fn actual_tmux_adapter_folded_frames_and_800_boundary_enter_once() {
    for text in [
        vec!["line"; 2].join("\n"),
        "x".repeat(800),
        "x".repeat(801),
        vec!["line"; 20].join("\n"),
        vec!["line"; 200].join("\n"),
        vec!["line"; 2000].join("\n"),
    ] {
        let count = text.bytes().filter(|b| *b == b'\n').count();
        let body = if count > 0 {
            format!("[Pasted text #9 +{count} lines]")
        } else if text.len() > 800 {
            "[Pasted text #9]".into()
        } else {
            text.clone()
        };
        let tmux = FakeTmux::new(
            CLAUDE_READY,
            "paste-buffer) cp \"$(dirname \"$0\")/after\" \"$(dirname \"$0\")/screen\" ;;",
        );
        fs::write(tmux.dir.path().join("after"), composer(&body)).unwrap();
        let outcome = tmux
            .pane()
            .with_composer(|pane| pane.submit(&text))
            .unwrap();
        assert_eq!(
            outcome,
            SendOutcome::Sent,
            "lines={count} chars={}",
            text.len()
        );
        assert_eq!(
            tmux.calls()
                .iter()
                .filter(|call| *call == "send-keys")
                .count(),
            1
        );
    }
}

#[test]
fn actual_tmux_adapter_foreign_draft_modal_mixed_and_lock_contention_enter_zero() {
    for body in [
        "[Pasted text #1 +99 lines]",
        "[Pasted text #1 +1 lines] foreign",
        "[Pasted text #1 +1 lines]\n[Pasted text #2 +1 lines]",
        "foreign draft",
    ] {
        let tmux = FakeTmux::new(
            CLAUDE_READY,
            "paste-buffer) cp \"$(dirname \"$0\")/after\" \"$(dirname \"$0\")/screen\" ;;",
        );
        fs::write(tmux.dir.path().join("after"), composer(body)).unwrap();
        assert!(matches!(
            tmux.pane()
                .with_composer(|pane| pane.submit("one\ntwo"))
                .unwrap(),
            SendOutcome::Indeterminate(_)
        ));
        assert!(!tmux.calls().iter().any(|call| call == "send-keys"));
    }
    for screen in [
        composer("foreign draft"),
        CODEX_APPROVAL.into(),
        "┌───────┐\n│ foreign draft │\n└───────┘\n› foreign draft".into(),
        "› \u{1b}[2mforeign draft\u{1b}[0m".into(),
    ] {
        let provider = if screen.contains('›') {
            ShadowProvider::Codex
        } else {
            ShadowProvider::Claude
        };
        assert_ne!(judge_pane(provider, &screen), PaneVerdict::Ready);
    }
    let tmux = FakeTmux::new(CLAUDE_READY, "");
    let mut pane = tmux.pane();
    let session = format!("s-{}", tmux.dir.path().display());
    let (sent, received) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        crate::services::claude_tui::composer_lock::with_composer_mutation_lock(&session, || {
            sent.send(()).unwrap();
            released.recv().unwrap();
        })
    });
    received.recv().unwrap();
    let blocked = pane.with_composer(|pane| pane.submit("hello"));
    release.send(()).unwrap();
    holder.join().unwrap();
    assert!(blocked.is_none());
    assert!(
        tmux.calls().is_empty(),
        "contending transaction must perform no mutation/capture"
    );
}

#[tokio::test]
async fn arrival_order_and_restart_missing_witness_cannot_be_guessed() {
    let world = World::new(ShadowProvider::Claude);
    let mut ledger = world.ledger(&[]);
    let first = ledger
        .append_entry(
            &Entry::Staged {
                key: 99,
                input: json!({"text":"oldest"}),
                state: RowState::Received,
            },
            &[],
        )
        .unwrap();
    ledger
        .append_entry(
            &Entry::Staged {
                key: 2,
                input: json!({"text":"younger"}),
                state: RowState::Received,
            },
            &[],
        )
        .unwrap();
    ledger
        .append_entry(
            &Entry::MoveCommitted {
                first_staged_seq: first,
                ids: vec![2, 99],
            },
            &[],
        )
        .unwrap();
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.idle()), Instant::now())
            .await
            .unwrap(),
        Step::Moved(99, RowState::AwaitTurn)
    );
    assert_eq!(actor.pane_submitted(), [frame(99, "oldest")]);
    let mut ledger = world.ledger_for(91, &[(1, "missing")]);
    ledger
        .append_entry(
            &Entry::Transition {
                key: 1,
                state: RowState::Running,
                attempt: None,
            },
            &[],
        )
        .unwrap();
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.idle()), Instant::now())
            .await
            .unwrap(),
        Step::Moved(1, RowState::Held(HeldReason::Ambiguous))
    );
    assert_eq!(owner_of(&ledger, 1), Owner::Ledger);
}

#[tokio::test]
async fn exact_source_frame_and_restart_witness_survive_only_ordered_closer() {
    let world = World::new(ShadowProvider::Claude);
    let mut ledger = world.ledger(&[(1, "keep Exact Spaces")]);
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    let at = Instant::now();
    actor
        .step(&mut ledger, Some(&world.idle()), at)
        .await
        .unwrap();
    for wrong in [
        "keep Exact Spaces".to_string(),
        frame(2, "keep Exact Spaces"),
        frame(1, "keep exact spaces"),
    ] {
        world.user(&wrong);
        assert_eq!(
            actor
                .step(&mut ledger, Some(&world.idle()), at)
                .await
                .unwrap(),
            Step::Wait("awaiting_user_record")
        );
    }
    world.user(&actor.pane_submitted()[0]);
    let mut restarted = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    assert_eq!(
        restarted
            .step(&mut ledger, Some(&world.idle()), at)
            .await
            .unwrap(),
        Step::Moved(1, RowState::Running)
    );
    assert_eq!(
        restarted
            .step(&mut ledger, Some(&world.idle()), at)
            .await
            .unwrap(),
        Step::Wait("turn_open")
    );
    world.append(json!({"type":"system","subtype":"turn_duration"}));
    assert_eq!(
        restarted
            .step(&mut ledger, Some(&world.idle()), at)
            .await
            .unwrap(),
        Step::Moved(1, RowState::Done(DoneReason::Completed))
    );
    assert!(restarted.pane_submitted().is_empty());
}

#[tokio::test]
async fn slow_success_starts_accept_window_after_enter_not_offer_time() {
    let world = World::new(ShadowProvider::Claude);
    let mut ledger = world.ledger(&[(1, "slow")]);
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    let started = Instant::now() - ACCEPT_WINDOW - Duration::from_secs(5);
    actor
        .step(&mut ledger, Some(&world.idle()), started)
        .await
        .unwrap();
    assert_eq!(
        actor
            .step(&mut ledger, Some(&world.idle()), Instant::now())
            .await
            .unwrap(),
        Step::Wait("awaiting_user_record")
    );
    assert_eq!(
        actor
            .step(
                &mut ledger,
                Some(&world.idle()),
                Instant::now() + ACCEPT_WINDOW
            )
            .await
            .unwrap(),
        Step::Moved(1, RowState::Unaccepted)
    );
}

#[tokio::test]
async fn optional_attempt_roundtrip_and_terminal_immutability() {
    let world = World::new(ShadowProvider::Claude);
    let mut ledger = world.ledger(&[(1, "roundtrip")]);
    let old: Entry = serde_json::from_value(
        json!({"kind":"transition","payload":{"key":1,"state":{"state":"ready"}}}),
    )
    .unwrap();
    assert!(matches!(old, Entry::Transition { attempt: None, .. }));
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    actor
        .step(&mut ledger, Some(&world.idle()), Instant::now())
        .await
        .unwrap();
    let evidence = ledger
        .rows()
        .unwrap()
        .row(1)
        .unwrap()
        .attempt
        .clone()
        .unwrap();
    ledger.checkpoint_rows().unwrap();
    let mut reopened = world.ledger(&[]);
    assert_eq!(
        reopened.rows().unwrap().row(1).unwrap().attempt.as_ref(),
        Some(&evidence)
    );
    reopened
        .append_entry(
            &Entry::Transition {
                key: 1,
                state: RowState::Done(DoneReason::Completed),
                attempt: None,
            },
            &[],
        )
        .unwrap();
    let mut changed = evidence.clone();
    changed.rendered_prompt = "different".into();
    reopened
        .append_entry(
            &Entry::Transition {
                key: 1,
                state: RowState::Done(DoneReason::Completed),
                attempt: Some(changed),
            },
            &[],
        )
        .unwrap();
    assert_eq!(
        reopened.rows().unwrap().row(1).unwrap().attempt.as_ref(),
        Some(&evidence)
    );
}

fn codex_world() -> World {
    let world = World::new(ShadowProvider::Codex);
    let meta = json!({"type":"session_meta","payload":{"id":"session","cwd":"/workspace"}});
    world.append(meta);
    world
}

fn codex_turn(world: &World, kind: &str, turn: &str) -> u64 {
    world.append(json!({"type":"event_msg","payload":{"type":kind,"turn_id":turn}}))
}

/// Submits key 1 on `world` and records its prompt inside turn `t-ours`, leaving the row Running.
async fn running(world: &World, channel: u64, pane: &str) -> (Ledger, InputActor<FakePane>) {
    let mut ledger = world.ledger_for(channel, &[(1, "long task")]);
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(pane));
    let at = Instant::now();
    let step = actor.step(&mut ledger, Some(&world.idle()), at).await;
    assert_eq!(step.unwrap(), Step::Moved(1, RowState::AwaitTurn));
    if world.binding.provider == ShadowProvider::Codex {
        codex_turn(world, "task_started", "t-ours");
    }
    world.user(&actor.pane_submitted()[0]);
    let opened = world.fact(open_turn());
    let step = actor.step(&mut ledger, Some(&opened), at).await;
    assert_eq!(step.unwrap(), Step::Moved(1, RowState::Running));
    (ledger, actor)
}

#[tokio::test]
async fn completion_holds_after_foreign_input_and_closes_on_its_own_named_abort() {
    let held = Step::Moved(1, RowState::Held(HeldReason::Ambiguous));
    // A prompt typed before our turn closed: no later closer can be tied to our turn alone.
    let claude = World::new(ShadowProvider::Claude);
    let (mut ledger, mut actor) = running(&claude, 1, CLAUDE_READY).await;
    claude.user("someone else typed this");
    claude.append(json!({"type":"system","subtype":"turn_duration"}));
    let step = idle_step(&mut actor, &mut ledger, &claude, Instant::now());
    assert_eq!(step.await, held);

    let codex = codex_world();
    let (mut ledger, mut actor) = running(&codex, 2, CODEX_READY).await;
    codex_turn(&codex, "task_started", "t-other");
    codex_turn(&codex, "task_complete", "t-other");
    let step = idle_step(&mut actor, &mut ledger, &codex, Instant::now());
    assert_eq!(step.await, held);

    // An interrupted turn of ours still ends the input's lifecycle.
    let (mut ledger, mut actor) = running(&codex, 3, CODEX_READY).await;
    codex_turn(&codex, "turn_aborted", "t-ours");
    let step = idle_step(&mut actor, &mut ledger, &codex, Instant::now());
    let done = Step::Moved(1, RowState::Done(DoneReason::Completed));
    assert_eq!(step.await, done);
    assert_eq!(owner_of(&ledger, 1), Owner::Settled);
}

#[tokio::test]
async fn a_not_sent_input_is_offered_afresh_under_the_next_binding() {
    let world = World::new(ShadowProvider::Claude);
    let mut ledger = world.ledger(&[(1, "after rotate")]);
    let mut pane = FakePane::new(CLAUDE_READY);
    pane.outcomes = VecDeque::from([SendOutcome::NotSent("load-buffer failed".into())]);
    let mut actor = InputActor::new(world.binding.clone(), pane);
    let step = idle_step(&mut actor, &mut ledger, &world, Instant::now());
    assert_eq!(step.await, Step::Moved(1, RowState::Ready));

    let rotated = world.transcript.with_file_name("rotated.jsonl");
    fs::write(&rotated, "{\"type\":\"summary\"}\n").unwrap();
    let binding = SourceBinding {
        source: source_id_for("rotated", &rotated).unwrap(),
        ..world.binding.clone()
    };
    let idle = ChannelFact {
        binding: binding.clone(),
        through: fs::metadata(&rotated).unwrap().len(),
        state: TurnState::Idle,
    };
    let mut next = InputActor::new(binding.clone(), FakePane::new(CLAUDE_READY));
    let step = next.step(&mut ledger, Some(&idle), Instant::now()).await;
    assert_eq!(step.unwrap(), Step::Moved(1, RowState::AwaitTurn));
    assert_eq!(next.pane_submitted(), [frame(1, "after rotate")]);
    let attempt = ledger.rows().unwrap().row(1).unwrap().attempt.clone();
    assert_eq!(attempt.map(|attempt| attempt.binding), Some(binding));
}

// Tracked frames: exact parsing, provider witnesses, merged records, crash cuts and turn closes.
mod tracked {
    use std::fs;
    use std::time::Instant;

    use serde_json::{Value, json};

    use super::super::actor::token::{self, Framed};
    use super::super::actor::witness::{Tracked, scan_tracked};
    use super::super::actor::{InputActor, Step};
    use super::super::attempt::{
        AttemptMeta, Disposition, Effect, Tracking, WitnessKind, fresh_token,
    };
    use super::super::durability_tests::supported::Recording;
    use super::super::ledger::Ledger;
    use super::super::rows::{AttemptEvidence, DoneReason, Entry, Row, RowState};
    use super::{CHANNEL, CLAUDE_READY, CODEX_READY, FakePane, World, codex_turn, codex_world};
    use crate::services::tui_o::shadow::ShadowProvider;

    // Registers a sent attempt for `key` under `token`, anchored at the transcript's current end.
    fn tracked(world: &World, ledger: &mut Ledger, key: u64, token: &str) -> String {
        let frame = token::render(token, &format!("input {key}"));
        let profile = token::profile(world.binding.provider);
        let anchor = fs::metadata(&world.transcript).unwrap().len();
        let meta = AttemptMeta {
            generation: 1,
            token: token.into(),
            frame_digest: token::digest(profile, &frame).unwrap(),
            frame_profile: Some(profile.into()),
            execution_nonce: "test-nonce".into(),
            source: world.binding.source.clone(),
            anchor,
            effect: Effect::Intent,
            incarnation: None,
            queue_end: None,
        };
        let evidence = AttemptEvidence {
            binding: world.binding.clone(),
            execution_nonce: meta.execution_nonce.clone(),
            eof: anchor,
            rendered_prompt: frame.clone(),
            source_ids: vec![key],
            record_end: None,
            native_turn_id: None,
        };
        let tracking = Tracking {
            attempt: Some(meta),
            ..Tracking::default()
        };
        let injecting = RowState::Injecting;
        (ledger.append_tracked(key, injecting, Some(evidence), &tracking)).unwrap();
        let sent = Entry::Transition {
            key,
            state: RowState::AwaitTurn,
            attempt: None,
        };
        ledger.append_entry(&sent, &[]).unwrap();
        frame
    }

    fn inputs(keys: &[u64]) -> Vec<(u64, String)> {
        keys.iter()
            .map(|key| (*key, format!("input {key}")))
            .collect()
    }

    fn ledger_of(world: &World, keys: &[u64]) -> Ledger {
        let inputs = inputs(keys);
        let inputs: Vec<(u64, &str)> = inputs.iter().map(|(k, t)| (*k, t.as_str())).collect();
        world.ledger(&inputs)
    }

    fn kinds(seen: &Tracked) -> Vec<(u64, WitnessKind)> {
        (seen.witnesses.iter())
            .map(|(key, w)| (*key, w.kind))
            .collect()
    }

    fn row(ledger: &Ledger, key: u64) -> Row {
        ledger.rows().unwrap().row(key).unwrap().clone()
    }

    fn claude_user(uuid: &str, content: Value) -> Value {
        json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": content}})
    }

    fn codex_user(text: &str) -> Value {
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text}]}})
    }

    // Only a whole start line, a body and the same token's end line make a frame.
    #[test]
    fn frames_parse_only_complete_marker_lines() {
        let claude = token::profile(ShadowProvider::Claude);
        let codex = token::profile(ShadowProvider::Codex);
        let (a, b) = (fresh_token(), fresh_token());
        let frame = token::render(&a, "첫 줄\n\tindented\n[adk:end]");
        let digest = token::digest(claude, &frame).unwrap();
        let own = Framed {
            token: a.clone(),
            digest: digest.clone(),
        };
        assert_eq!(token::frames(claude, &frame), vec![own]);
        // Claude stores a tab as four spaces and either provider may store CRLF.
        let stored = frame.replace('\t', "    ").replace('\n', "\r\n");
        assert_eq!(token::frames(claude, &stored)[0].digest, digest);
        let expanded = &token::frames(codex, &frame.replace('\t', "    "))[0];
        assert_ne!(
            Some(&expanded.digest),
            token::digest(codex, &frame).as_ref()
        );
        assert_eq!(token::digest("unknown-profile", &frame), None);

        let merged = format!("leader\n{frame}\n{}\ntail", token::render(&b, "second"));
        let tokens: Vec<String> = (token::frames(claude, &merged).into_iter())
            .map(|found| found.token)
            .collect();
        assert_eq!(tokens, [a.clone(), b.clone()]);
        let interrupted = format!("[adk:tok={a}]\npartial\n{}", token::render(&b, "inner"));
        let found = token::frames(claude, &interrupted);
        assert_eq!((found.len(), found[0].token.as_str()), (1, b.as_str()));

        let end = format!("[adk:end={a}]");
        for text in [
            format!("> {frame}"),
            frame.replace(&end, ""),
            frame.replace(&end, &format!("[adk:end={b}]")),
            format!("[adk:tok={a}]\n{end}"),
            format!("[adk:tok={0}]\nbody\n[adk:end={0}]", &a[..31]),
            format!(
                "[adk:tok={}]\nbody\n[adk:end={}]",
                a.to_uppercase(),
                a.to_uppercase()
            ),
            format!("[adk:tok={a}] \nbody\n{end}"),
        ] {
            assert!(token::frames(claude, &text).is_empty(), "{text}");
        }
    }

    // Q, a delivered remove and a tool result keep a row queued; only a prompt attachment runs it.
    #[test]
    fn claude_queue_records_and_a_prompt_attachment_witness_in_order() {
        let world = World::new(ShadowProvider::Claude);
        let mut ledger = ledger_of(&world, &[1]);
        let frame = tracked(&world, &mut ledger, 1, &fresh_token());
        let attachment = |mode: &str| {
            json!({"type": "attachment", "uuid": format!("a-{mode}"), "attachment":
                {"type": "queued_command", "commandMode": mode, "prompt": frame}})
        };
        for record in [
            json!({"type": "queue-operation", "operation": "enqueue", "content": frame}),
            json!({"type": "queue-operation", "operation": "dequeue"}),
            json!({"type": "queue-operation", "operation": "remove",
                "reason": "absorbed_mid_turn", "content": frame}),
            claude_user(
                "u-tool",
                json!([{"type": "tool_result", "tool_use_id": "call-1", "content": frame}]),
            ),
            attachment("task-notification"),
            json!({"type": "user", "uuid": "u-meta", "isMeta": true, "message": {"content": frame}}),
            attachment("prompt"),
        ] {
            world.append(record);
        }
        let seen = scan_tracked(&world.binding, &ledger.rows().unwrap()).unwrap();
        use WitnessKind::{Attachment, Queued, Removed, Tool};
        assert_eq!(
            kinds(&seen),
            [(1, Queued), (1, Removed), (1, Tool), (1, Attachment)]
        );
        assert!(seen.complete && seen.foreign == 0 && seen.altered.is_empty());
        let mut states = Vec::new();
        for (key, witness) in seen.witnesses {
            ledger.append_witness(key, witness).unwrap();
            states.push(row(&ledger, key).state);
        }
        let queued = RowState::Queued;
        assert_eq!(states, [queued, queued, queued, RowState::Running]);
    }

    // Shared prompt ids join nothing: a row is named only by its own exact frame after its anchor.
    #[test]
    fn only_an_exact_registered_frame_after_its_anchor_names_a_row() {
        let world = World::new(ShadowProvider::Claude);
        let mut ledger = ledger_of(&world, &[1, 2, 3, 4]);
        let queued = |uuid: &str, text: &str| {
            json!({"type": "user", "uuid": uuid, "promptId": "p-shared", "promptSource": "queued",
                "message": {"role": "user", "content": text}})
        };
        let one = tracked(&world, &mut ledger, 1, &fresh_token());
        let two = tracked(&world, &mut ledger, 2, &fresh_token());
        world.append(queued("u-2", &two));
        let early = fresh_token();
        world.append(claude_user(
            "u-early",
            json!(token::render(&early, "input 3")),
        ));
        tracked(&world, &mut ledger, 3, &early);
        let four = tracked(&world, &mut ledger, 4, &fresh_token());
        world.append(queued("u-1", &one));
        world.append(claude_user(
            "u-4",
            json!(four.replace("input 4", "input 4!")),
        ));
        let stranger = token::render(&fresh_token(), "input 9");
        world.append(claude_user("u-9", json!(stranger)));

        let seen = scan_tracked(&world.binding, &ledger.rows().unwrap()).unwrap();
        let user = WitnessKind::User;
        assert_eq!(kinds(&seen), [(2, user), (1, user)]);
        assert_eq!((seen.foreign, seen.altered.as_slice()), (2, [4].as_slice()));
    }

    // One Codex user carrying ten frames confirms ten rows once each and completes them together.
    #[tokio::test]
    async fn a_merged_codex_user_confirms_every_framed_row_once() {
        let world = codex_world();
        let keys: Vec<u64> = (1..=10).collect();
        let mut ledger = ledger_of(&world, &keys);
        let frames: Vec<String> = (keys.iter())
            .map(|key| tracked(&world, &mut ledger, *key, &fresh_token()))
            .collect();
        codex_turn(&world, "task_started", "t-merged");
        world.append(
            json!({"type": "response_item", "payload": {"type": "message",
            "role": "user", "content": [{"type": "input_text", "text": "leader typed this"},
            {"type": "input_text", "text": frames.join("\n")}]}}),
        );
        let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CODEX_READY));
        let now = Instant::now();
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Wait("turn_open"));
        let rows = ledger.rows().unwrap();
        for key in &keys {
            let row = rows.row(*key).unwrap();
            assert_eq!((row.state, row.witnesses.len()), (RowState::Running, 1));
            let turn = row.witnesses[0].witness.turn_ref.as_deref();
            assert_eq!(turn, Some("t-merged"));
        }
        assert_eq!(
            rows.open_rows().count(),
            keys.len(),
            "a frame never makes a row"
        );
        let records = ledger.records().len();
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Wait("turn_open"));
        assert_eq!(ledger.records().len(), records, "a re-read appends nothing");

        codex_turn(&world, "task_complete", "t-merged");
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Idle);
        for key in &keys {
            let done = row(&ledger, *key);
            assert_eq!(done.state, RowState::Done(DoneReason::Completed));
            assert!(done.dispositions.is_empty());
        }
        assert!(actor.pane_submitted().is_empty());
    }

    // Three rows confirmed by one merged Claude user, ready for a step.
    fn merged() -> (World, Ledger) {
        let world = World::new(ShadowProvider::Claude);
        let mut ledger = ledger_of(&world, &[1, 2, 3]);
        let frames: Vec<String> = (1..=3)
            .map(|key| tracked(&world, &mut ledger, key, &fresh_token()))
            .collect();
        world.append(claude_user("u-merged", json!(frames.join("\n"))));
        (world, ledger)
    }

    // A failed append stops the step; the next read from the oldest anchor records only the rest.
    #[tokio::test]
    async fn a_failed_append_inside_a_merged_record_resumes_without_another_enter() {
        let now = Instant::now();
        let per_append = {
            let (world, mut ledger) = merged();
            let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
            let recording = Recording::start(&world.runtime);
            recording.arm(None);
            actor
                .step(&mut ledger, Some(&world.idle()), now)
                .await
                .unwrap();
            let events = recording.events().len();
            assert_eq!(events % 3, 0, "three appends, one shape");
            events / 3
        };
        for adopted in [true, false] {
            let (world, mut ledger) = merged();
            let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
            let recording = Recording::start(&world.runtime);
            recording.arm(Some(per_append + 1));
            assert!(
                actor
                    .step(&mut ledger, Some(&world.idle()), now)
                    .await
                    .is_err()
            );
            drop((recording, ledger));
            if !adopted {
                cut_tail(&world, 3);
            }

            let mut ledger = Ledger::open(&world.runtime, CHANNEL).unwrap();
            let running = |ledger: &Ledger, key| row(ledger, key).state == RowState::Running;
            assert!(running(&ledger, 1));
            assert_eq!(running(&ledger, 2), adopted);
            assert!(!running(&ledger, 3));
            let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
            let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
            assert_eq!(step.unwrap(), Step::Wait("turn_open"));
            for key in 1..=3 {
                let row = row(&ledger, key);
                assert_eq!((row.state, row.witnesses.len()), (RowState::Running, 1));
            }
            assert!(actor.pane_submitted().is_empty(), "no Enter after the cut");
        }
    }

    // Cuts the last WAL line mid-record, as a power loss during append would.
    fn cut_tail(world: &World, bytes: u64) {
        let dir = world.runtime.join("input_ledger").join(CHANNEL.to_string());
        let wal = (fs::read_dir(&dir).unwrap())
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .unwrap();
        let file = fs::OpenOptions::new().write(true).open(&wal).unwrap();
        let len = file.metadata().unwrap().len();
        file.set_len(len - bytes).unwrap();
    }

    // A close ends only the rows confirmed in its own turn; an abort says so.
    #[tokio::test]
    async fn a_tracked_row_settles_only_at_its_own_turn_close() {
        let now = Instant::now();
        let world = World::new(ShadowProvider::Claude);
        let mut ledger = ledger_of(&world, &[1]);
        let frame = tracked(&world, &mut ledger, 1, &fresh_token());
        let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
        world.append(claude_user("u-1", json!(frame)));
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Wait("turn_open"));
        world.append(claude_user(
            "u-stop",
            json!("[Request interrupted by user]"),
        ));
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Idle);
        let stopped = row(&ledger, 1);
        assert_eq!(stopped.state, RowState::Done(DoneReason::Completed));
        assert_eq!(stopped.dispositions, [Disposition::Interrupted]);

        // T1 was open before the anchors: its abort closes row 1 only; row 2 waits for T2.
        let world = codex_world();
        let mut ledger = ledger_of(&world, &[1, 2]);
        codex_turn(&world, "task_started", "t1");
        let early = tracked(&world, &mut ledger, 1, &fresh_token());
        let late = tracked(&world, &mut ledger, 2, &fresh_token());
        world.append(codex_user(&early));
        codex_turn(&world, "turn_aborted", "t1");
        codex_turn(&world, "task_started", "t2");
        world.append(codex_user(&late));
        let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CODEX_READY));
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Wait("turn_open"));
        let aborted = row(&ledger, 1);
        assert_eq!(aborted.state, RowState::Done(DoneReason::Completed));
        assert_eq!(aborted.dispositions, [Disposition::Interrupted]);
        assert_eq!(row(&ledger, 2).state, RowState::Running);
        codex_turn(&world, "task_complete", "foreign");
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Wait("turn_open"));
        codex_turn(&world, "task_complete", "t2");
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Idle);
        let completed = row(&ledger, 2);
        assert_eq!(completed.state, RowState::Done(DoneReason::Completed));
        assert!(completed.dispositions.is_empty());
        assert!(actor.pane_submitted().is_empty());

        // An unnamed abort is no close: T2's close does not end the row confirmed in T1.
        let world = codex_world();
        let mut ledger = ledger_of(&world, &[1]);
        codex_turn(&world, "task_started", "t1");
        let frame = tracked(&world, &mut ledger, 1, &fresh_token());
        world.append(codex_user(&frame));
        world.append(json!({"type": "event_msg", "payload": {"type": "turn_aborted"}}));
        codex_turn(&world, "task_started", "t2");
        codex_turn(&world, "task_complete", "t2");
        let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CODEX_READY));
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Wait("turn_open"));
        assert_eq!(row(&ledger, 1).state, RowState::Running);
    }

    // A rewritten frame holds its unconfirmed row; a stranger's frame stops any new offer.
    #[tokio::test]
    async fn an_altered_frame_holds_its_row_and_a_foreign_one_stops_new_offers() {
        let now = Instant::now();
        let world = World::new(ShadowProvider::Claude);
        let mut ledger = ledger_of(&world, &[1, 2]);
        let frame = tracked(&world, &mut ledger, 2, &fresh_token());
        world.append(claude_user(
            "u-2",
            json!(frame.replace("input 2", "input two")),
        ));
        world.append(claude_user(
            "u-x",
            json!(token::render(&fresh_token(), "x")),
        ));
        let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
        let step = actor.step(&mut ledger, Some(&world.idle()), now).await;
        assert_eq!(step.unwrap(), Step::Wait("foreign_frame"));
        let held = RowState::Held(super::super::rows::HeldReason::Ambiguous);
        assert_eq!(row(&ledger, 2).state, held);
        assert_eq!(row(&ledger, 1).state, RowState::Received);
        assert!(actor.pane_submitted().is_empty());
    }
}
