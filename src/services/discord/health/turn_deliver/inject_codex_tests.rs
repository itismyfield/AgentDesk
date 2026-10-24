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
if (d / "defer.receipt").exists():
    sys.exit(0)
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
pub(super) struct CodexPane {
    pub(super) dir: tempfile::TempDir,
    pub(super) channel: u64,
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

    pub(super) fn session(&self) -> String {
        ProviderKind::Codex.build_tmux_session_name(&format!("inject-{}", self.channel))
    }

    pub(super) fn path(&self, name: &str) -> PathBuf {
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
    pub(super) fn keys(&self) -> String {
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

pub(super) struct QueuedCodex {
    _db: crate::db::auto_queue::test_support::TestPostgresDb,
    pub(super) shared: std::sync::Arc<crate::services::discord::SharedData>,
    pub(super) pane: CodexPane,
    pub(super) head: crate::services::turn_orchestrator::Intervention,
}

#[tokio::test(flavor = "current_thread")]
async fn queued_codex_v2_real_entry_uses_permit_adapter_and_native_receipt_pg() {
    use std::os::unix::fs::MetadataExt;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = QueuedCodex::new(7_272_301).await;
    let mailbox = f.shared.mailbox(ChannelId::new(f.pane.channel));
    mailbox
        .v2_adopt(f.disk_path(), "codex-real-v2".into())
        .await
        .unwrap();
    let transcript = f.pane.dir.path().join("rollout.jsonl");
    let meta = fs::metadata(&transcript).unwrap();
    let attempt = serde_json::json!({"nonce":"7272301a", "provider":"codex",
        "target":{"host":"tmux","owner":f.pane.session(),"root":"/tmp","execution_nonce":"turn-open"},
        "source":{"kind":"existing","session_id":format!("queued-codex-{}",f.pane.channel),
            "transcript":transcript.display().to_string(),"file":[meta.dev(),meta.ino()],"offset":meta.len()}});
    mailbox
        .v2_request(move |slot| {
            slot.as_mut().unwrap().reserve(
                serde_json::from_value(attempt).unwrap(),
                serde_json::from_value(
                    serde_json::json!({"frame":"prepared","digest":"digest","attribution":{}}),
                )
                .unwrap(),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let result = f.attempt().await;
    assert!(
        matches!(result, InjectAttempt::Injected { .. }),
        "V2 must traverse the actual queued origin, permit, adapter and rollout receipt: {result:?}; disk={}; keys={}",
        f.disk(),
        f.pane.keys()
    );
    let snapshot = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(snapshot["items"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["terminal_identities"].as_array().unwrap().len(), 1);
    let keys = f.pane.keys();
    let _ = f.attempt().await;
    assert_eq!(
        f.pane.keys(),
        keys,
        "a settled V2 source must not be typed twice"
    );
}

impl QueuedCodex {
    pub(super) async fn new(channel: u64) -> Self {
        use crate::services::turn_orchestrator::{Intervention, InterventionMode};
        use poise::serenity_prelude::{MessageId, UserId};
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
        let native = format!("queued-codex-{channel}");
        let meta = record(serde_json::json!({"type":"session_meta", "payload":{
            "id":native, "cwd":"/tmp"}}));
        let pane = CodexPane::new(
            channel,
            &(meta + &steerable()),
            RuntimeHandoffKind::CodexTui,
        );
        let mut binding =
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&pane.session())
                .unwrap();
        binding.session_id = Some(native);
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&pane.session(), binding);
        test_hook::enable_codex(channel);
        test_hook::set_queued_head(channel, true);
        let head = Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(channel * 100),
            queued_generation: 0,
            source_message_ids: vec![MessageId::new(channel * 100)],
            source_message_queued_generations: vec![
                crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                    MessageId::new(channel * 100),
                    0,
                ),
            ],
            source_text_segments: Vec::new(),
            text: "status?".into(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let result = crate::services::discord::mailbox_enqueue_intervention(
            &shared,
            &ProviderKind::Codex,
            ChannelId::new(channel),
            head,
        )
        .await;
        assert!(result.enqueued && result.persistence_error.is_none());
        let head = crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(channel))
            .await
            .intervention_queue
            .remove(0);
        assert!(crate::services::turn_orchestrator::prefix::eligible(&head));
        Self {
            _db: db,
            shared,
            pane,
            head,
        }
    }

    async fn attempt(&self) -> InjectAttempt {
        let request = HumanInputRequest {
            channel_id: ChannelId::new(self.pane.channel),
            provider: ProviderKind::Codex,
            text: self.head.text.clone(),
            author_id: 1,
            source: "queued".into(),
            metadata: None,
            channel_name_hint: None,
        };
        let origin = inject::Origin::QueuedHead {
            expected: crate::services::turn_orchestrator::QueueIdentity::of(&self.head),
            parent: None,
            gate: |_, _| true,
        };
        super::inject_human_input(&self.shared, &request, origin).await
    }

    fn persistence(&self) -> crate::services::turn_orchestrator::QueuePersistenceContext {
        crate::services::discord::queue_persistence_context(
            &self.shared,
            &ProviderKind::Codex,
            ChannelId::new(self.pane.channel),
        )
    }
}

#[tokio::test(flavor = "current_thread")]
async fn queued_codex_real_entry_settles_native_receipt_once_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = QueuedCodex::new(7_272_101).await;
    assert!(matches!(f.attempt().await, InjectAttempt::Injected { .. }));
    assert_eq!(f.pane.keys(), "paste-buffer+send-keys");
    let mailbox = f.shared.mailbox(ChannelId::new(f.pane.channel));
    assert!(mailbox.snapshot().await.intervention_queue.is_empty());
    let replay = crate::services::discord::mailbox_enqueue_intervention(
        &f.shared,
        &ProviderKind::Codex,
        ChannelId::new(f.pane.channel),
        f.head.clone(),
    )
    .await;
    assert!(
        !replay.enqueued,
        "settled source identity cannot be queued again"
    );
    assert_eq!(f.pane.keys(), "paste-buffer+send-keys");
}

impl QueuedCodex {
    pub(super) fn disk_path(&self) -> PathBuf {
        crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("codex")
            .join(self.persistence().token_hash)
            .join(format!("{}.json", self.pane.channel))
    }

    fn disk(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.disk_path()).unwrap()).unwrap()
    }

    fn request(&self) -> HumanInputRequest {
        HumanInputRequest {
            channel_id: ChannelId::new(self.pane.channel),
            provider: ProviderKind::Codex,
            text: self.head.text.clone(),
            author_id: 1,
            source: "queued".into(),
            metadata: None,
            channel_name_hint: None,
        }
    }

    fn origin(&self) -> inject::Origin {
        inject::Origin::QueuedHead {
            expected: crate::services::turn_orchestrator::QueueIdentity::of(&self.head),
            parent: None,
            gate: |_, _| true,
        }
    }

    async fn leave_unknown(&self) {
        self.pane.set("defer.receipt", "");
        let attempt = self.attempt().await;
        assert!(
            matches!(
                attempt,
                InjectAttempt::Unconfirmed {
                    detail: "queued_settle_unknown",
                    ..
                }
            ),
            "the actor retains an unconfirmed effect as Unknown: {attempt:?}"
        );
        assert_eq!(self.disk()["head_injection"]["phase"], "unknown");
        assert_eq!(self.pane.keys(), "paste-buffer+send-keys");
    }

    async fn restart(&mut self) {
        self.shared = crate::services::discord::make_shared_data_for_tests_with_storage(
            self.shared.pg_pool.clone(),
        );
        let restored = self
            .shared
            .mailbox(ChannelId::new(self.pane.channel))
            .hydrate_pending_queue_from_disk_or_refused(self.persistence())
            .await
            .expect("the replacement actor accepts restoration");
        assert!(restored.persistence_error.is_none(), "{restored:?}");
        assert_eq!(restored.queue_len_after, 1);
    }

    fn late_receipt(&self) -> String {
        let body = fs::read_to_string(self.pane.path("buffer")).unwrap();
        record(serde_json::json!({"type":"event_msg", "payload":{
            "type":"item_completed", "turn_id":TURN, "item":{
                "type":"UserMessage", "id": self.pane.receipt_id(),
                "content":[{"type":"text", "text":body, "text_elements":[]}]}}}))
    }

    fn append(&self, text: &str) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(self.pane.path("rollout.jsonl"))
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn queued_codex_unknown_restart_waits_for_complete_receipt_without_retyping_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut f = QueuedCodex::new(7_272_102).await;
    f.leave_unknown().await;
    let mut disk = f.disk();
    disk["head_injection"]["phase"] = serde_json::json!("may_have_sent");
    fs::write(f.disk_path(), disk.to_string()).unwrap();
    f.restart().await;
    assert_eq!(
        f.disk()["head_injection"]["phase"],
        "unknown",
        "restart must preserve the possibly sent attempt before any retry"
    );
    let calls = f.pane.tmux_calls();
    assert_eq!(
        f.attempt().await,
        InjectAttempt::HandbackFailed("queued_head_unknown")
    );
    assert_eq!(f.disk()["head_injection"]["phase"], "unknown");
    let before = fs::read(f.disk_path()).unwrap();
    let receipt = f.late_receipt();
    f.append(receipt.trim_end_matches('\n'));
    assert_eq!(
        f.attempt().await,
        InjectAttempt::HandbackFailed("queued_head_unknown")
    );
    assert_eq!(fs::read(f.disk_path()).unwrap(), before);
    assert_eq!(
        f.pane.tmux_calls(),
        calls,
        "Unknown never re-enters the pane"
    );
    f.append("\n");
    assert_eq!(
        f.attempt().await,
        InjectAttempt::NotSent("queued_head_reconciled")
    );
    assert_eq!(
        f.pane.tmux_calls(),
        calls,
        "receipt reconciliation never types"
    );
    assert!(
        f.shared
            .mailbox(ChannelId::new(f.pane.channel))
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );
    assert!(f.disk()["head_injection"].is_null());
    assert_eq!(f.disk()["terminal_identities"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn queued_codex_v2_compat_clear_keeps_original_and_marker_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = QueuedCodex::new(7_272_201).await;
    let original = f.disk()[0].clone();
    let snapshot = serde_json::json!({
        "version": 2, "items": [], "head_injection": null,
        "retained_inputs": [{
            "input": {"identity": {"primary": original["message_id"],
                "queued_generation": original["queued_generation"],
                "sources": [{"message_id": original["message_id"],
                    "queued_generation": original["queued_generation"]}]},
                "original": original, "phase": "unknown", "attempt": null,
                "payload": null, "confirmation_deadline_ms": 30_000},
            "reason": {"kind": "unconfirmed"},
            "notice": {"kind": "unconfirmed", "enqueued": false}}],
        "terminal_identities": []});
    let bytes = serde_json::to_vec_pretty(&snapshot).unwrap();
    fs::write(f.disk_path(), &bytes).unwrap();
    let marker = f.disk_path().with_extension("dispatch");
    fs::write(&marker, b"held dispatch bytes").unwrap();
    let calls = f.pane.tmux_calls();
    let _ = f.attempt().await;
    assert_eq!(
        f.pane.tmux_calls(),
        calls,
        "V2 read-only entry must not type"
    );
    assert_eq!(fs::read(f.disk_path()).unwrap(), bytes);
    let cleared = f
        .shared
        .mailbox(ChannelId::new(f.pane.channel))
        .clear(f.persistence())
        .await;
    assert_eq!(
        fs::read(f.disk_path()).unwrap(),
        bytes,
        "A24: compatibility clear must preserve retained original bytes"
    );
    assert_eq!(
        fs::read(marker).unwrap(),
        b"held dispatch bytes",
        "A24: read-only refusal precedes dispatch marker removal"
    );
    assert!(cleared.persistence.count_unknown);
    assert!(cleared.discarded_message_ids.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn queued_codex_proof_filters_recorded_identity_before_receipt_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut f = QueuedCodex::new(7_272_202).await;
    f.leave_unknown().await;
    f.append(&f.late_receipt());
    let original = f.disk();
    let calls = f.pane.tmux_calls();
    for field in ["session_id", "transcript", "file"] {
        let mut changed = original.clone();
        changed["head_injection"]["attempt"][field] = match field {
            "file" => serde_json::json!([0, 1]),
            _ => serde_json::json!("different-recorded-identity"),
        };
        fs::write(f.disk_path(), changed.to_string()).unwrap();
        f.restart().await;
        let before = fs::read(f.disk_path()).unwrap();
        assert_eq!(
            f.attempt().await,
            InjectAttempt::HandbackFailed("queued_head_unknown"),
            "Recorded::matches must reject only the changed {field}"
        );
        assert_eq!(fs::read(f.disk_path()).unwrap(), before);
    }
    fs::write(f.disk_path(), original.to_string()).unwrap();
    f.restart().await;
    assert_eq!(
        f.attempt().await,
        InjectAttempt::NotSent("queued_head_reconciled"),
        "the same complete receipt proves the unmodified recorded identity"
    );
    assert_eq!(f.pane.tmux_calls(), calls, "proof checks must never retype");
}

#[tokio::test(flavor = "current_thread")]
async fn queued_codex_receipt_rejects_session_file_and_legacy_attempt_mismatch_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut f = QueuedCodex::new(7_272_103).await;
    f.leave_unknown().await;
    f.append(&f.late_receipt());
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&f.pane.session())
            .unwrap();
    let mut changed = binding.clone();
    changed.session_id = Some("another-native-session".into());
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&f.pane.session(), changed);
    let calls = f.pane.tmux_calls();
    assert_eq!(
        f.attempt().await,
        InjectAttempt::HandbackFailed("queued_head_unknown")
    );
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&f.pane.session(), binding);
    let original = fs::read(f.pane.path("rollout.jsonl")).unwrap();
    fs::rename(f.pane.path("rollout.jsonl"), f.pane.path("original.jsonl")).unwrap();
    fs::write(f.pane.path("rollout.jsonl"), &original).unwrap();
    assert_eq!(
        f.attempt().await,
        InjectAttempt::HandbackFailed("queued_head_unknown")
    );
    fs::rename(f.pane.path("original.jsonl"), f.pane.path("rollout.jsonl")).unwrap();
    let mut disk = f.disk();
    disk["head_injection"]
        .as_object_mut()
        .unwrap()
        .remove("attempt");
    fs::write(f.disk_path(), disk.to_string()).unwrap();
    f.restart().await;
    assert_eq!(
        f.attempt().await,
        InjectAttempt::HandbackFailed("queued_head_unknown")
    );
    assert_eq!(f.pane.tmux_calls(), calls);
    assert_eq!(f.disk()["items"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn queued_codex_same_owner_serializes_two_inputs_across_effect_pg() {
    use crate::services::turn_orchestrator::{QueueIdentity, SourceMessageQueuedGeneration};
    use poise::serenity_prelude::MessageId;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = QueuedCodex::new(7_272_104).await;
    let channel = ChannelId::new(f.pane.channel);
    let mut second = f.head.clone();
    second.message_id = MessageId::new(f.head.message_id.get() + 1);
    second.source_message_ids = vec![second.message_id];
    second.source_message_queued_generations =
        vec![SourceMessageQueuedGeneration::user_instruction(
            second.message_id,
            second.queued_generation,
        )];
    second.source_text_segments.clear();
    second.text = "second input".into();
    let sent = crate::services::discord::mailbox_enqueue_intervention(
        &f.shared,
        &ProviderKind::Codex,
        channel,
        second,
    )
    .await;
    assert!(sent.enqueued && sent.persistence_error.is_none());
    let second = f
        .shared
        .mailbox(channel)
        .snapshot()
        .await
        .intervention_queue[1]
        .clone();
    let (reached, resume) = test_hook::park_queued(f.pane.channel, "may_have_sent");
    let (shared, request, origin) = (f.shared.clone(), f.request(), f.origin());
    let first =
        tokio::spawn(async move { super::inject_human_input(&shared, &request, origin).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached.notified())
        .await
        .unwrap();
    assert_eq!(f.disk()["head_injection"]["phase"], "may_have_sent");
    assert!(
        f.disk()["head_injection"]["attempt"]["offset"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        f.pane.tmux_calls(),
        0,
        "attempt is durable before any pane I/O"
    );
    let mut request = f.request();
    request.text = second.text.clone();
    let second_origin = inject::Origin::QueuedHead {
        expected: QueueIdentity::of(&second),
        parent: None,
        gate: |_, _| true,
    };
    assert_eq!(
        super::inject_human_input(&f.shared, &request, second_origin).await,
        InjectAttempt::NotSent("transition_busy")
    );
    resume.notify_one();
    assert!(matches!(
        first.await.unwrap(),
        InjectAttempt::Injected { .. }
    ));
    let queue = f
        .shared
        .mailbox(channel)
        .snapshot()
        .await
        .intervention_queue;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].message_id, second.message_id);
    fs::remove_file(f.pane.path("pasted")).unwrap();
    let second_origin = inject::Origin::QueuedHead {
        expected: QueueIdentity::of(&second),
        parent: None,
        gate: |_, _| true,
    };
    assert!(matches!(
        super::inject_human_input(&f.shared, &request, second_origin).await,
        InjectAttempt::Injected { .. }
    ));
    assert_eq!(
        f.pane.keys(),
        "paste-buffer+send-keys+paste-buffer+send-keys"
    );
    assert!(
        f.shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn queued_codex_preserves_each_gate_and_live_discord_catchup_scope_pg() {
    use poise::serenity_prelude::MessageId;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = QueuedCodex::new(7_272_105).await;
    test_hook::set_queued_head(f.pane.channel, false);
    assert_eq!(f.attempt().await, InjectAttempt::NotSent("queued_gate_off"));
    test_hook::set_queued_head(f.pane.channel, true);
    test_hook::set(f.pane.channel, InjectMode::Off, f.pane.path("tmux"));
    assert_eq!(f.attempt().await, InjectAttempt::NotSent("queued_gate_off"));
    test_hook::clear(f.pane.channel);
    test_hook::set(f.pane.channel, InjectMode::All, f.pane.path("tmux"));
    test_hook::set_queued_head(f.pane.channel, true);
    assert_eq!(f.attempt().await, InjectAttempt::NotSent("queued_gate_off"));
    test_hook::enable_codex(f.pane.channel);
    let guard = crate::services::discord::inject_disposition::claim_source(
        &ProviderKind::Codex,
        f.head.message_id,
        std::time::Instant::now(),
    )
    .unwrap();
    let live = inject::Origin::Discord {
        message: MessageId::new(f.head.message_id.get()),
        handback: Box::new((f.head.clone(), "queued-codex-live".into())),
        name: "system".into(),
        guard,
    };
    assert_eq!(
        super::inject_human_input(&f.shared, &f.request(), live).await,
        InjectAttempt::NotSent("queue_nonempty")
    );
    assert_eq!(f.pane.tmux_calls(), 0);
    assert_eq!(
        f.disk().as_array().unwrap().len(),
        1,
        "live input did not reserve a queued attempt"
    );
}

#[test]
fn queued_codex_production_origin_gate_stays_closed_without_cfg_test() {
    let source = include_str!("inject.rs");
    let origin = source
        .split_once("impl Origin {")
        .unwrap()
        .1
        .split_once("/// Tests never read the process env")
        .unwrap()
        .0;
    let dir = tempfile::tempdir().unwrap();
    let probe = format!(
        r#"
#[derive(PartialEq, Eq)]
enum ProviderKind {{ Claude, Codex }}
#[derive(Clone, Copy)]
struct ChannelId(u64);
impl ChannelId {{ fn get(self) -> u64 {{ self.0 }} }}
#[derive(PartialEq, Eq)]
enum InjectMode {{ Off, All }}
fn mode(_: u64) -> InjectMode {{ InjectMode::All }}
fn queued_head_enabled(_: u64) -> bool {{ true }}
struct Origin;
impl Origin {{{origin}
fn main() {{
    let channel = ChannelId(7272);
    assert!(Origin::queued_head_ready(&ProviderKind::Claude, channel));
    assert!(!Origin::queued_head_ready(&ProviderKind::Codex, channel),
        "production Codex queued gate must stay closed even with mode and queued gate open");
    assert!(!Origin::queued_head_gate_enabled(&ProviderKind::Codex, channel, None, |_, _| true));
}}
"#
    );
    let path = dir.path().join("gate.rs");
    let binary = dir.path().join("gate");
    fs::write(&path, probe).unwrap();
    let compiled = std::process::Command::new("rustc")
        .args(["--edition=2024", "--crate-name=queued_gate_probe"])
        .arg(&path)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "non-test gate probe must compile: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let result = std::process::Command::new(binary).output().unwrap();
    assert!(
        result.status.success(),
        "cfg(not(test)) gate failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
