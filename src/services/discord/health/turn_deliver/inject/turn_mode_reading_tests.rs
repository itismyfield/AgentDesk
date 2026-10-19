use super::*;
use crate::services::discord::turn_presence::activity::tests as presence;
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::turn_orchestrator::QueueIdentity;
use serde_json::json;
use std::io::Write;

struct Fixture {
    registry: Arc<HealthRegistry>,
    shared: Arc<SharedData>,
    pane: InjectPane,
    channel: u64,
    _confirmation: Option<TestConfirmation>,
    _db: crate::db::auto_queue::test_support::TestPostgresDb,
    _binding: presence::BindingRoot,
}

impl Fixture {
    async fn new(channel: u64, turn_mode: bool) -> Self {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let registry = Arc::new(HealthRegistry::new());
        let shared = register_inject_runtime(&registry, &[channel], Some(pool)).await;
        let binding = presence::BindingRoot::enter(&crate::config::runtime_root().unwrap());
        let pane = InjectPane::new(channel, "all");
        let path = pane.path(InjectPane::TRANSCRIPT);
        pane.set(
            InjectPane::TRANSCRIPT,
            &format!(
                "{}\n",
                json!({"type":"user","uuid":"first","message":{"content":"initial turn"}})
            ),
        );
        presence::log_turn_source(&ProviderKind::Claude, channel, &pane.session(), "bp", &path);
        presence::companion_attach_log_watcher(&shared, channel, &pane.session(), &path);
        let confirmation = turn_mode.then(|| TestConfirmation::new(channel));
        assert_eq!(
            presence::settled_reason(&shared, &ProviderKind::Claude, channel).await,
            "open"
        );
        Self {
            registry,
            shared,
            pane,
            channel,
            _confirmation: confirmation,
            _db: db,
            _binding: binding,
        }
    }

    fn append(&self, record: serde_json::Value) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(self.pane.path(InjectPane::TRANSCRIPT))
            .unwrap();
        writeln!(file, "{record}").unwrap();
    }

    async fn rollover(&self, tail: &str) {
        let first = deliver(&self.registry, self.channel).await;
        assert!(
            first.contains("Injected"),
            "initial busy injection: {first}"
        );
        let frame = self.pane.read("buffer").unwrap();
        self.append(json!({"type":"system","subtype":"turn_duration"}));
        self.append(json!({"type":"user","uuid":"next-native","message":{"content":frame}}));
        self.append(json!({"type":"assistant","uuid":"next-response","message":{"content":[{"type":"text","text":"working"}]}}));
        match tail {
            "init" => self.append(json!({"type":"system","subtype":"init"})),
            "large" => self.append(json!({"type":"user","uuid":"tool-result","message":{"content":[{"type":"tool_result","tool_use_id":"tool","content":"x".repeat(100_000)}]}})),
            _ => self.append(json!({"type":"permission-mode","mode":"bypassPermissions"})),
        }
        fs::remove_file(self.pane.path("pasted")).unwrap();
        let legacy = crate::services::tui_turn_state::observe_claude_jsonl_turn_state(
            &self.pane.path(InjectPane::TRANSCRIPT),
        );
        assert_eq!(
            legacy.is_busy(),
            tail == "large",
            "legacy fixture: {legacy:?}"
        );
    }

    fn assert_submissions(&self, count: usize) {
        let keys = self.pane.keys();
        assert_eq!(
            keys.iter().filter(|key| *key == "paste-buffer").count(),
            count,
            "{keys:?}"
        );
        assert_eq!(
            keys.iter().filter(|key| *key == "send-keys").count(),
            count,
            "{keys:?}"
        );
        let receipts = self
            .pane
            .read(InjectPane::TRANSCRIPT)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|row| {
                (row["type"] == "queue-operation" && row["operation"] == "enqueue")
                    || row["uuid"] == "bb-submitted-user"
            })
            .count();
        assert_eq!(receipts, count);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bb_live_rollover_bypasses_legacy_unknown_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_108_101, true).await;
    f.rollover("permission").await;
    let result = deliver_text(&f.registry, f.channel, "next Discord input").await;
    assert!(
        result.contains("Injected"),
        "live deliver must join, not queue: {result}"
    );
    assert!(queue_texts(&f.shared, f.channel).await.is_empty());
    f.assert_submissions(2);
}

#[tokio::test(flavor = "current_thread")]
async fn bb_queued_rollover_bypasses_legacy_idle_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_108_102, true).await;
    f.rollover("init").await;
    send_meanwhile(&f.shared, f.channel, f.channel + 20, "queued Discord input").await;
    inject_hook::set_queued_head(f.channel, true);
    let head = crate::services::discord::mailbox_snapshot(&f.shared, ChannelId::new(f.channel))
        .await
        .intervention_queue[0]
        .clone();
    let request = HumanInputRequest {
        channel_id: ChannelId::new(f.channel),
        provider: ProviderKind::Claude,
        text: head.text.clone(),
        author_id: head.author_id.get(),
        source: "discord".into(),
        metadata: None,
        channel_name_hint: None,
    };
    let result = inject::attempt(
        &f.shared,
        &request,
        inject::Origin::QueuedHead {
            expected: QueueIdentity::of(&head),
            parent: None,
            gate: |_, _| true,
        },
    )
    .await;
    assert!(
        matches!(result, inject::InjectAttempt::Injected { .. }),
        "queued-head admission must reach the pane: {result:?}"
    );
    assert!(queue_texts(&f.shared, f.channel).await.is_empty());
    f.assert_submissions(2);
}

#[tokio::test(flavor = "current_thread")]
async fn bb_large_single_record_keeps_injection_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_108_103, true).await;
    f.rollover("large").await;
    let result = deliver_text(&f.registry, f.channel, "after oversized tool result").await;
    assert!(result.contains("Injected"), "{result}");
    f.assert_submissions(2);
}

#[tokio::test(flavor = "current_thread")]
async fn bb_non_turn_mode_preserves_legacy_nonbusy_veto_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_108_106, false).await;
    f.rollover("permission").await;
    let result = deliver_text(&f.registry, f.channel, "legacy remains queued").await;
    assert!(
        result.starts_with("queued") && result.contains("not_busy"),
        "{result}"
    );
    f.assert_submissions(1);
}

#[tokio::test(flavor = "current_thread")]
async fn bb_idle_user_receipt_is_relayed_once_by_o_without_new_inflight_pg() {
    use crate::services::discord::tui_prompt_relay::relay_e2e::{
        discord_mock, n1a_turn_mode::claude_idle_o_transition::fixture as output,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(discord_mock::CHANNEL_ID, true).await;
    f.append(json!({"type":"system","subtype":"turn_duration"}));
    f.pane.idle();
    f.pane.drop_row(&f.shared).await;
    f.pane.set("receipt", &json!({"type":"user","uuid":"bb-submitted-user","sessionId":"parent","message":{"role":"user","content":"@BODY@"}}).to_string());
    let root = crate::config::runtime_root().unwrap();
    let mut o = output::Fixture::new(&root).await;
    o.pane.path = f.pane.path(InjectPane::TRANSCRIPT);
    o.pane.a = "parent".into();
    o.pane.tmux = f.pane.session();
    presence::log_turn_source(
        &ProviderKind::Claude,
        f.channel,
        &f.pane.session(),
        "bb-output",
        &o.pane.path,
    );
    crate::services::tui_prompt_dedupe::binding_events::forget_channel_for_tests(f.channel);
    o.initialize(0);
    o.gate.acquired();
    let result = deliver_text(&f.registry, f.channel, "idle native question").await;
    assert!(
        result.contains("Injected"),
        "idle TUI accepts the same input: {result}"
    );
    f.assert_submissions(1);
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            f.channel
        )
        .is_none()
    );
    output::append(
        &o.pane.path,
        &output::record(
            "parent",
            "bb-answer",
            "2026-10-18T03:00:00Z",
            "BB_NATIVE_REPLY",
            false,
        ),
    );
    let mut engine = o.engine();
    engine.capture().unwrap();
    engine.deliver(100).await;
    assert_eq!(o.posts(), ["BB_NATIVE_REPLY"]);
    engine.capture().unwrap();
    engine.deliver(100).await;
    assert_eq!(
        o.posts(),
        ["BB_NATIVE_REPLY"],
        "re-observation must not duplicate the native reply"
    );
    assert!(queue_texts(&f.shared, f.channel).await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn bb_modal_and_person_draft_keep_zero_paste_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, modal) in [(7_108_110, false), (7_108_111, true)] {
        let f = Fixture::new(channel, true).await;
        f.append(json!({"type":"system","subtype":"turn_duration"}));
        if modal {
            f.pane.set("cap.before", &format!("Allow this tool?\n1. Yes\n2. No\nEnter to confirm\n{BORDER}\n❯ \n{BORDER}\n{FOOTER}"));
        } else {
            f.pane.draft();
        }
        let result = deliver_text(&f.registry, f.channel, "blocked by safety gate").await;
        let veto = if modal { "veto=modal" } else { "veto=draft" };
        assert!(
            result.starts_with("queued") && result.contains(veto),
            "{result}"
        );
        f.assert_submissions(0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bb_configured_herdr_preserves_legacy_nonbusy_veto_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_108_112, true).await;
    f.rollover("permission").await;
    let _hosts = crate::config::session_hosts::force_for_test(None, &[(f.channel, "fixture")]);
    let result = deliver_text(&f.registry, f.channel, "Herdr remains queued").await;
    assert!(
        result.starts_with("queued") && result.contains("not_busy"),
        "{result}"
    );
    f.assert_submissions(1);
}
