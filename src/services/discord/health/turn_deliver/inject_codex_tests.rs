//! Codex busy-turn injection behind the real attempt entry: the off switch, the turn vetoes read
//! before any lock or pane call, and a scripted Codex pane taking one steer.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use poise::serenity_prelude::ChannelId;

use super::HumanInputRequest;
use super::inject::{self, InjectAttempt, InjectMode, test_hook};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::{InflightTurnState, TurnSource};
use crate::services::provider::ProviderKind;

const TURN: &str = "turn-open";

/// Scripted Codex `tmux` of 80x24, never attached: a paste draws the buffer into the composer and
/// Enter records its header as user input of the open turn.
const FAKE_TMUX: &str = r##"#!/bin/sh
d='@D@'
echo "$*" >> "$d/log"
case "$2" in
display-message) echo "0,,80,24" ;;
capture-pane) if [ -f "$d/pasted" ]; then cat "$d/cap.pasted"; else cat "$d/cap.before"; fi ;;
load-buffer) for last do :; done; cp "$last" "$d/buffer" ;;
if-shell)
  echo "$7" >> "$d/keys"
  case "$7" in
  paste-buffer*)
    touch "$d/pasted"
    { printf '\n\342\200\272 %s\n' "$(head -n 1 "$d/buffer")"
      awk 'NR > 1 { print "  " $0 }' "$d/buffer"
      printf '\n  tab to queue message    95%% context left\n'; } > "$d/cap.pasted" ;;
  send-keys*)
    printf '{"type":"event_msg","payload":{"type":"item_completed","turn_id":"turn-open","item":{"type":"UserMessage","content":[{"type":"text","text":"%s"}]}}}\n' "$(head -n 1 "$d/buffer")" >> "$d/rollout.jsonl" ;;
  esac ;;
esac
exit 0
"##;

fn record(json: serde_json::Value) -> String {
    format!("{json}\n")
}

fn started() -> String {
    record(serde_json::json!({"type": "event_msg",
        "payload": {"type": "task_started", "turn_id": TURN, "root_turn_id": TURN}}))
}

fn context() -> String {
    record(
        serde_json::json!({"type": "turn_context", "payload": {"turn_id": TURN,
        "root_turn_id": TURN, "sandbox_policy": {"type": "danger-full-access"}}}),
    )
}

fn user() -> String {
    record(
        serde_json::json!({"type": "event_msg", "payload": {"type": "item_completed",
        "turn_id": TURN, "item": {"type": "UserMessage", "content": [{"type": "text", "text": "go"}]}}}),
    )
}

/// An open model turn Enter steers into.
fn steerable() -> String {
    started() + &context() + &user()
}

/// A scripted Codex pane over `rollout`, the TUI-direct row and binding naming it, and the switch
/// forced open for its channel until dropped.
struct CodexPane {
    dir: tempfile::TempDir,
    channel: u64,
}

impl CodexPane {
    fn new(channel: u64, rollout: &str, runtime: RuntimeHandoffKind) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/busy-inject-tmp");
        fs::create_dir_all(&root).unwrap();
        let pane = Self {
            dir: tempfile::tempdir_in(root).unwrap(),
            channel,
        };
        pane.set("rollout.jsonl", rollout);
        pane.show("busy_empty.ansi");
        let program = pane.path("tmux");
        let script = FAKE_TMUX.replace("@D@", &pane.dir.path().display().to_string());
        pane.set("tmux", &script);
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let session = pane.session();
        let mut row = InflightTurnState::new(
            ProviderKind::Codex,
            channel,
            None,
            0,
            0,
            0,
            "typed over ssh".to_string(),
            None,
            Some(session.clone()),
            None,
            None,
            0,
        );
        row.turn_source = TurnSource::ExternalInput;
        crate::services::discord::inflight::save_inflight_state_create_new(&row)
            .expect("external turn row");
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: runtime,
            output_path: pane.path("rollout.jsonl").display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&session, binding);
        test_hook::set(channel, InjectMode::All, program);
        pane
    }

    fn session(&self) -> String {
        ProviderKind::Codex.build_tmux_session_name(&format!("inject-{}", self.channel))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn set(&self, name: &str, value: &str) {
        fs::write(self.path(name), value).unwrap();
    }

    /// The pane before any paste shows this captured Codex screen.
    fn show(&self, fixture: &str) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_busy_inject");
        self.set(
            "cap.before",
            &fs::read_to_string(dir.join(fixture)).unwrap(),
        );
    }

    fn tmux_calls(&self) -> usize {
        let log = fs::read_to_string(self.path("log")).unwrap_or_default();
        log.lines().count()
    }

    /// Paste and Enter commands the server applied.
    fn keys(&self) -> String {
        let keys = fs::read_to_string(self.path("keys")).unwrap_or_default();
        let first = keys
            .lines()
            .map(|line| line.split_whitespace().next().unwrap_or(""));
        first.collect::<Vec<_>>().join("+")
    }

    fn header(&self) -> String {
        let buffer = fs::read_to_string(self.path("buffer")).unwrap_or_default();
        buffer.lines().next().unwrap_or_default().to_string()
    }

    async fn attempt(
        &self,
        shared: &std::sync::Arc<crate::services::discord::SharedData>,
    ) -> InjectAttempt {
        let request = HumanInputRequest {
            channel_id: ChannelId::new(self.channel),
            provider: ProviderKind::Codex,
            text: "status?".to_string(),
            author_id: 200,
            source: "imessage".to_string(),
            metadata: None,
            channel_name_hint: None,
        };
        inject::attempt(shared, &request, inject::Origin::External).await
    }
}

impl Drop for CodexPane {
    fn drop(&mut self) {
        test_hook::clear(self.channel);
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&self.session());
    }
}

/// With the shipped constant off, a Codex input is refused exactly as before, first and before the
/// session transition or any pane call; enabling Codex lets the same input reach the transition.
#[tokio::test(flavor = "current_thread")]
async fn codex_input_is_refused_as_before_until_codex_is_enabled() {
    assert!(
        !inject::CODEX_BUSY_INJECT_ENABLED,
        "turning Codex injection on is its own change"
    );
    let _root = crate::config::TestRuntimeRootGuard::new();
    let ch = 5_845_201;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let pane = CodexPane::new(ch, &steerable(), RuntimeHandoffKind::CodexTui);
    let transition = shared.session_transition_lock(ChannelId::new(ch));
    let held = transition.try_lock_owned().expect("transition free");
    let refused = InjectAttempt::NotSent("provider_unsupported");
    assert_eq!(
        (pane.attempt(&shared).await, pane.tmux_calls()),
        (refused, 0)
    );
    test_hook::enable_codex(ch);
    let waits = InjectAttempt::NotSent("transition_busy");
    assert_eq!((pane.attempt(&shared).await, pane.tmux_calls()), (waits, 0));
    drop(held);
}

/// An enabled Codex channel refuses a turn Enter would not steer, and a session bound to another
/// runtime, before the session transition or any pane call.
#[tokio::test(flavor = "current_thread")]
async fn an_enabled_codex_channel_refuses_unsteerable_turns_before_the_lock() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let ended = record(serde_json::json!({"type": "event_msg",
        "payload": {"type": "task_complete", "turn_id": TURN}}));
    let codex = RuntimeHandoffKind::CodexTui;
    let cases = [
        (5_845_211, started() + &user(), codex, "not_steerable"),
        (5_845_212, steerable() + &ended, codex, "not_busy"),
        (5_845_213, started() + &context(), codex, "turn_unknown"),
        (
            5_845_214,
            steerable(),
            RuntimeHandoffKind::ClaudeTui,
            "session_unresolved",
        ),
    ];
    for (ch, rollout, runtime, veto) in cases {
        let pane = CodexPane::new(ch, &rollout, runtime);
        test_hook::enable_codex(ch);
        let transition = shared.session_transition_lock(ChannelId::new(ch));
        let _held = transition.try_lock_owned().expect("transition free");
        let refused = InjectAttempt::NotSent(veto);
        assert_eq!(
            (pane.attempt(&shared).await, pane.tmux_calls()),
            (refused, 0)
        );
    }
}

/// An enabled Codex channel pastes into the open model turn with one Enter, and the ledger it
/// filled keeps the observers from publishing the steer; a draft hands the input back unpasted.
#[tokio::test(flavor = "current_thread")]
async fn an_enabled_codex_channel_steers_the_open_turn_quietly_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
    let (ch, drafted) = (5_845_221, 5_845_222);
    let pane = CodexPane::new(ch, &steerable(), RuntimeHandoffKind::CodexTui);
    test_hook::enable_codex(ch);
    let outcome = pane.attempt(&shared).await;
    let injected = InjectAttempt::Injected { turn_id: None };
    assert_eq!(
        (outcome, pane.keys()),
        (injected, "paste-buffer+send-keys".into())
    );
    let observe = crate::services::tui_prompt_dedupe::observe_codex_prompt_in_turn_at;
    let seen = observe(
        &pane.session(),
        &pane.header(),
        None,
        Some(TURN),
        chrono::Utc::now(),
    );
    assert_eq!(
        seen,
        crate::services::tui_prompt_dedupe::PromptObservation::InjectedSteer
    );
    let draft = CodexPane::new(drafted, &steerable(), RuntimeHandoffKind::CodexTui);
    draft.show("busy_draft.ansi");
    test_hook::enable_codex(drafted);
    let outcome = draft.attempt(&shared).await;
    assert!(
        matches!(outcome, InjectAttempt::HandedBack { veto: "draft", .. }),
        "{outcome:?}"
    );
    let queued = super::inject_tests::queue_texts(&shared, drafted).await;
    assert_eq!(
        (draft.keys(), queued),
        (String::new(), vec!["status?".to_string()])
    );
}
