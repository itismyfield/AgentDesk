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

fn composer(body: &str) -> String {
    format!(
        "────────────────────\n❯ {body}\n────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    )
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
