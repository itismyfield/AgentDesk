//! Codex busy-turn injection behind the real attempt entry: the channel gate, the turn vetoes read
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
/// Enter records its full input as the native response/completed pair of the open turn.
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
    python3 - "$d" <<'PY'
import json, pathlib, sys
d = pathlib.Path(sys.argv[1])
counter = d / "receipt.count"
number = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(number))
row = f"native-user-{number}"
(d / "receipt.id").write_text(row)
body = (d / "buffer").read_text()
response = {"type": "response_item", "payload": {"type": "message", "role": "user",
    "content": [{"type": "input_text", "text": body}],
    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-open"}}}
completed = {"type": "event_msg", "payload": {"type": "item_completed", "turn_id": "turn-open",
    "item": {"type": "UserMessage", "id": row,
        "content": [{"type": "text", "text": body, "text_elements": []}]}}}
with (d / "rollout.jsonl").open("a") as stream:
    for record in (response, completed):
        stream.write(json.dumps(record) + "\n")
PY
    ;;
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

/// A scripted Codex pane over `rollout`, its row/binding and a per-channel mode/program override.
/// This does not force the Codex capability.
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

    fn receipt_id(&self) -> String {
        fs::read_to_string(self.path("receipt.id")).expect("native input record id")
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

/// A launched Codex binding has a native rollout and a separate relay transcript.
#[tokio::test(flavor = "current_thread")]
async fn codex_injection_reads_native_with_a_separate_relay_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
    let ch = 5_845_231;
    let pane = CodexPane::new(ch, &steerable(), RuntimeHandoffKind::CodexTui);
    pane.set(
        "relay.jsonl",
        "{\"type\":\"assistant\",\"message\":{\"content\":\"relay only\"}}\n",
    );
    let session = pane.session();
    let mut binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session)
            .expect("native binding");
    binding.relay_output_path = Some(pane.path("relay.jsonl").display().to_string());
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&session, binding);
    test_hook::enable_codex(ch);
    assert_eq!(
        pane.attempt(&shared).await,
        InjectAttempt::Injected { turn_id: None }
    );
    assert_eq!(pane.keys(), "paste-buffer+send-keys");
    assert_eq!(
        crate::services::tui_prompt_dedupe::observe_codex_prompt_in_turn_at(
            &session,
            &pane.header(),
            Some(&pane.receipt_id()),
            Some(TURN),
            chrono::Utc::now(),
        ),
        crate::services::tui_prompt_dedupe::PromptObservation::InjectedSteer,
    );
    assert!(
        !fs::read_to_string(pane.path("relay.jsonl"))
            .unwrap()
            .contains(&pane.header())
    );
}

/// Claude keeps reading the relay accessor when the binding's other file is not a transcript.
#[tokio::test(flavor = "current_thread")]
async fn claude_injection_keeps_reading_a_separate_relay_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
    let ch = 5_845_232;
    let pane = super::inject_tests::InjectPane::new(ch, "all");
    pane.set("native.jsonl", &steerable());
    let session = pane.session();
    let mut binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session)
            .expect("Claude binding");
    binding.relay_output_path = Some(binding.output_path.clone());
    binding.output_path = pane.path("native.jsonl").display().to_string();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&session, binding);
    let request = HumanInputRequest {
        channel_id: ChannelId::new(ch),
        provider: ProviderKind::Claude,
        text: "status?".to_string(),
        author_id: 200,
        source: "imessage".to_string(),
        metadata: None,
        channel_name_hint: None,
    };
    assert_eq!(
        inject::attempt(&shared, &request, inject::Origin::External).await,
        InjectAttempt::Injected { turn_id: None }
    );
    assert_eq!(pane.keys(), vec!["paste-buffer", "send-keys"]);
    assert_eq!(pane.records().len(), 1);
}

/// The production switch and existing channel gate admit a turn without a Codex override.
#[tokio::test(flavor = "current_thread")]
async fn codex_listed_channel_uses_production_enablement_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
    let ch = 5_845_251;
    let _gate = crate::services::discord::router::busy_inject_support::open_gate(ch);
    let pane = CodexPane::new(ch, &steerable(), RuntimeHandoffKind::CodexTui);
    assert_eq!(
        pane.attempt(&shared).await,
        InjectAttempt::Injected { turn_id: None }
    );
    assert_eq!(pane.keys(), "paste-buffer+send-keys");
    let native = fs::read_to_string(pane.path("rollout.jsonl")).unwrap();
    assert_eq!(
        native.matches(&pane.header()).count(),
        2,
        "one native response/completed pair"
    );
    let completed: Vec<serde_json::Value> = native
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|record: &serde_json::Value| {
            record["type"] == "event_msg"
                && record["payload"]["type"] == "item_completed"
                && record["payload"]["item"]["content"][0]["text"] == pane.header()
        })
        .collect();
    assert_eq!(completed.len(), 1, "one canonical input receipt");
    assert_eq!(completed[0]["payload"]["item"]["id"], pane.receipt_id());
}

/// A listed Herdr-hosted channel opens the queued head gate, yet Codex input there stays refused.
#[tokio::test(flavor = "current_thread")]
async fn codex_input_on_a_listed_herdr_channel_stays_refused() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let ch = 5_340_851;
    let _hosts = crate::config::session_hosts::force_for_test(Some("node-a"), &[(ch, "node-a")]);
    let _gate = crate::services::discord::router::busy_inject_support::open_gate(ch);
    let queued = crate::services::discord::router::queued_head_inject_covers;
    assert!(queued(ChannelId::new(ch), None));
    let pane = CodexPane::new(ch, &steerable(), RuntimeHandoffKind::CodexTui);
    test_hook::set(ch, InjectMode::All, pane.path("tmux"));
    let transition = shared.session_transition_lock(ChannelId::new(ch));
    let _held = transition.try_lock_owned().expect("transition free");
    assert_eq!(
        (pane.attempt(&shared).await, pane.tmux_calls()),
        (InjectAttempt::NotSent("provider_unsupported"), 0)
    );
}

/// Unlisted channels retain the dormant result, even with an allowed parent and external mode.
#[tokio::test(flavor = "current_thread")]
async fn codex_input_is_refused_as_before_until_codex_is_enabled() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let parent = 5_845_252;
    let _gate = crate::services::discord::router::busy_inject_support::open_gate(parent);
    let covers = crate::services::discord::router::queued_head_inject_covers;
    for (ch, mode) in [
        (5_845_253, InjectMode::All),
        (5_845_254, InjectMode::External),
    ] {
        let pane = CodexPane::new(ch, &steerable(), RuntimeHandoffKind::CodexTui);
        test_hook::set(ch, mode, pane.path("tmux"));
        assert!(
            covers(ChannelId::new(ch), Some(ChannelId::new(parent))),
            "parent admits intake"
        );
        assert!(!covers(ChannelId::new(ch), None), "child is not listed");
        let transition = shared.session_transition_lock(ChannelId::new(ch));
        let _held = transition.try_lock_owned().expect("transition free");
        assert_eq!(
            (pane.attempt(&shared).await, pane.tmux_calls()),
            (InjectAttempt::NotSent("provider_unsupported"), 0)
        );
        let mailbox = crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(ch)).await;
        assert!(mailbox.active_user_message_id.is_none());
        assert!(mailbox.pending_user_dispatch.is_none());
        assert!(mailbox.intervention_queue.is_empty());
    }
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
            5_845_215,
            steerable().replace("danger-full-access", "read-only"),
            codex,
            "not_steerable",
        ),
        (
            5_845_216,
            steerable().replace("danger-full-access", "workspace-write"),
            codex,
            "not_steerable",
        ),
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
        Some(&pane.receipt_id()),
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
