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

// Approval wording that does not adjoin the composer, so readiness alone accepts it.
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

    let end = world.user("deploy the fix");
    let opened = world.fact(open_turn());
    let step = actor.step(&mut ledger, Some(&opened), t0).await.unwrap();
    assert_eq!(step, Step::Moved(1, RowState::Running));

    // An Idle read before our own record does not close the turn it opened.
    let mut early = world.idle();
    early.through = end - 1;
    let step = actor.step(&mut ledger, Some(&early), t0).await.unwrap();
    assert_eq!(step, Step::Wait("turn_open"));
    assert_eq!(owner_of(&ledger, 1), Owner::Ledger);

    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Moved(1, RowState::Done(DoneReason::Completed)));
    assert_eq!(owner_of(&ledger, 1), Owner::Settled);
    assert_eq!(actor.pane_submitted(), ["deploy the fix"]);

    // Only then does the next input reach the pane.
    let step = actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    assert_eq!(step, Step::Moved(2, RowState::AwaitTurn));
    assert_eq!(
        actor.pane_submitted(),
        ["deploy the fix", "then run checks"]
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
    assert_eq!(actor.pane_submitted(), ["status?"]);

    // Silence for the whole accept window has the same outcome.
    let mut ledger = world.ledger_for(2, &[(1, "quiet")]);
    let mut actor = InputActor::new(world.binding.clone(), FakePane::new(CLAUDE_READY));
    actor
        .step(&mut ledger, Some(&world.idle()), t0)
        .await
        .unwrap();
    let before = t0 + ACCEPT_WINDOW - Duration::from_millis(1);
    let step = actor.step(&mut ledger, Some(&world.idle()), before).await;
    assert_eq!(step.unwrap(), Step::Wait("awaiting_user_record"));
    let late = t0 + ACCEPT_WINDOW;
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
    assert_eq!(actor.pane_submitted(), ["retry me", "retry me"]);
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
            PaneVerdict::NotReady,
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
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    // Wide enough that the non-hanging steps finish on a loaded host; only `sleep 30` times out.
    fn pane(&self) -> TmuxPane {
        TmuxPane::with_program("s", self.dir.path().join("tmux"), Duration::from_secs(3))
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
        ("paste-buffer) exit 1 ;;", "not-sent"),
        ("paste-buffer) exec sleep 30 ;;", "indeterminate"),
        ("send-keys) exit 1 ;;", "indeterminate"),
        ("send-keys) exec sleep 30 ;;", "indeterminate"),
        ("", "sent"),
    ];
    for (behavior, expected) in cases {
        let tmux = FakeTmux::new(CLAUDE_READY, behavior);
        let outcome = tmux.pane().submit("hello");
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
    let tmux = FakeTmux::new(CLAUDE_READY, "paste-buffer) exec sleep 30 ;;");
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
