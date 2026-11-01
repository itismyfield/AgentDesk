//! The owner's busy Codex effect on a real Herdr socket rig: the adapter takes main's Codex busy
//! order on the execution's gate, so one paste and one Enter reach the pane or no key does.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::discord::queue_io::input_owner::effect::PreparedPayload;
use crate::services::discord::queue_io::input_owner::pump;
use crate::services::herdr_admission::{Admission, force_for_test};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

const PROMPT: &str = "owner herdr codex question";
/// The open model turn of the captured rollout's first eleven records.
const T: &str = "01a11d88-ef31-7860-9d48-88926c4b1809";

/// A busy Codex screen whose composer shows `draft`, as Herdr reads the pane.
fn screen(draft: &str) -> Value {
    let text = format!(
        "• Working (3s • esc to interrupt)\n\n› {draft}\n\n  gpt-5.4 xhigh · 87% context left"
    );
    json!({"type": "pane_read", "read": {
        "pane_id": PANE, "workspace_id": "w1", "tab_id": "w1:1", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 3, "truncated": false
    }})
}

/// Every thread admits Herdr input, as an unstopped node does.
fn open() {
    let stop = std::env::temp_dir().join(format!("adk-och-none-{}", uuid::Uuid::new_v4()));
    std::mem::forget(force_for_test(Admission::new(true, Some(stop))));
}

/// A Bound Codex execution on a real rig's pane, owned by a logical session that is not its pane
/// id, over a rollout whose open model turn is `T`.
struct Pane {
    rig: HerdrRig,
    channel: u64,
    logical: String,
    execution: String,
    target: crate::services::session_host::HerdrTarget,
    rollout: std::path::PathBuf,
    context: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Pane {
    fn start(channel: u64) -> Self {
        let rig = HerdrRig::start();
        let execution = uuid::Uuid::new_v4().simple().to_string();
        let context = rig.context(&execution);
        rig.run_provider(&context, false);
        let logical = format!("AgentDesk-codex-owner-{channel}");
        let marker = crate::services::tmux_common::session_temp_path(&logical, "host_kind");
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        let owner = HostedOwner {
            provider: "codex".into(),
            discord_token_hash: "owner-herdr".into(),
            channel_id: channel.to_string(),
            logical_key: logical.clone(),
            owner_node: NODE.into(),
            runtime_root: "/adk/runtime".into(),
        };
        let record = HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: execution.clone(),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: KEY.into(),
                socket_addr: rig.socket().display().to_string(),
                named_session: SESSION.into(),
                pane_id: PANE.into(),
            }),
            expected: Some(rig.expected(&execution)),
            source_ref: SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: owner.channel_id.clone(),
                provider: "codex".into(),
                logical_key: logical.clone(),
                execution_nonce: execution.clone(),
                initial_source: None,
                baseline_event_seq: None,
            },
            owner,
        };
        let target = {
            let _registry = rig.registry_on_this_thread();
            crate::services::claude::herdr_turn::gate(&record).unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex_busy_inject/model_turn.jsonl");
        let records: String = std::fs::read_to_string(fixture)
            .unwrap()
            .lines()
            .take(11)
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(&rollout, records).unwrap();
        Self {
            rig,
            channel,
            logical,
            execution,
            target,
            rollout,
            context,
            _dir: dir,
        }
    }

    fn physical(&self) -> String {
        format!("{}#{PANE}", self.rig.socket().display())
    }

    fn nonce(&self) -> String {
        format!("{:032x}", self.channel)
    }

    fn input(&self, target_turn: &str) -> CodexInput {
        CodexInput {
            session: self.logical.clone(),
            rollout: self.rollout.clone(),
            target_turn: target_turn.into(),
            channel: self.channel,
            message: None,
            source: "discord".into(),
            author: "7".into(),
            name: "tester".into(),
            nonce: self.nonce(),
            text: PROMPT.into(),
        }
    }

    fn frame(&self) -> String {
        busy_inject::frame("discord", "7", "tester", &self.nonce(), PROMPT)
    }

    /// One admitted original on the real mailbox, shared by one attempt or an automatic retry.
    fn with_owner<R>(
        &self,
        run: impl FnOnce(
            &tokio::runtime::Runtime,
            crate::services::turn_orchestrator::ChannelMailboxHandle,
            PreparedPayload,
        ) -> R,
    ) -> R {
        let _registry = self.rig.registry_on_this_thread();
        open();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .on_thread_start(open)
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let registry = ChannelMailboxRegistry::default();
        let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(self.channel));
        let id = 8_000_000_000_000_008_000 + self.channel % 1000;
        let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
            "message_id":id,"channel_id":self.channel,"source_message_ids":[id],
            "queued_generation":1,"text":PROMPT}))
        .unwrap();
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&self.rollout).unwrap();
        let prepared = PreparedPayload {
            frame: self.frame(),
            sources: vec![id.to_string()],
            attempt: json!({"nonce":self.nonce(),"provider":"codex",
                "target":{"host":"herdr","owner":self.logical,
                    "root":self.rig.socket().display().to_string(),
                    "execution_nonce":self.execution},
                "source":{"kind":"existing","session_id":"sid-codex-herdr",
                    "transcript":self.rollout.display().to_string(),
                    "file":[meta.dev(), meta.ino()],"offset":meta.len()}}),
        };
        let channel = self.channel;
        runtime.block_on(async {
            let context = crate::services::turn_orchestrator::QueuePersistenceContext::new(
                &crate::services::provider::ProviderKind::Codex,
                "owner-herdr",
                None,
            );
            let id = poise::serenity_prelude::ChannelId::new(channel);
            let (path, _) = crate::services::turn_orchestrator::v2_queue(&context, id).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            mailbox.v2_ensure_channel(&context, id).await.unwrap();
            use crate::services::discord::queue_io::input_owner::admission::{
                Admission, AdmissionTicket, admit,
            };
            assert!(matches!(
                admit(&mailbox, &original, AdmissionTicket::new()).await,
                Admission::Accepted(_)
            ));
        });
        run(&runtime, mailbox, prepared)
    }

    fn leaf(&self, target_turn: &str) -> Leaf {
        BusyCodexHerdr {
            target: self.target.clone(),
            writer: self.physical(),
            input: self.input(target_turn),
        }
        .leaf()
    }

    /// One owner attempt through the V2 pump with this pane's leaf aimed at `target_turn`.
    fn attempt(&self, target_turn: &str) -> (Completion, Value) {
        self.with_owner(|runtime, mailbox, prepared| {
            let completion = runtime.block_on(pump::submit(
                &mailbox,
                prepared,
                Arc::new(|| 10_000),
                self.leaf(target_turn),
            ));
            let snapshot = runtime
                .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
                .unwrap();
            (completion.unwrap(), snapshot)
        })
    }

    /// The legacy ledger holds no row for this nonce: registering it again is accepted.
    fn ledger_free(&self) -> bool {
        let (register, withdraw) = (
            crate::services::tui_prompt_dedupe::register_injected_steer,
            crate::services::tui_prompt_dedupe::withdraw_injected_steer,
        );
        let free = register("codex", &self.logical, &self.nonce(), T, None);
        withdraw("codex", &self.logical, &self.nonce());
        free
    }
}

/// A busy Codex Herdr arrival takes the adapter: one paste of the reserved frame and one Enter on
/// the bound pane, the head settled, and the legacy ledger row released.
#[test]
fn owner_herdr_busy_codex_pastes_and_enters_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pane = Pane::start(7_272_541);
    pane.rig.answer("pane.read", screen(""));
    let (completion, snapshot) = pane.attempt(T);
    assert_eq!(completion, Completion::Submitted, "{snapshot}");
    let sends = pane.rig.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    let frame = pane.frame();
    assert_eq!(sends[0]["text"], format!("\u{1b}[200~{frame}\u{1b}[201~"));
    assert_eq!(sends[1]["keys"], json!(["enter"]));
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    assert!(pane.ledger_free());
}

/// A moved model turn and a person's draft are refused before any key: the original goes back to
/// custody as known not sent, with no ledger row left.
#[test]
fn owner_herdr_busy_codex_refuses_a_moved_turn_or_a_draft_before_any_key() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, draft, target_turn, reason) in [
        (7_272_542, "", "01b-another-turn", "TurnChanged"),
        (7_272_543, "a person's draft", T, "ComposerUnread"),
    ] {
        let pane = Pane::start(channel);
        pane.rig.answer("pane.read", screen(draft));
        let (completion, snapshot) = pane.attempt(target_turn);
        assert_eq!(completion, Completion::NotSent(reason.into()), "{snapshot}");
        assert!(pane.rig.sends().is_empty(), "{:?}", pane.rig.sends());
        let retained = &snapshot["retained_inputs"];
        assert_eq!(retained[0]["disposition"], "known_not_sent", "{snapshot}");
        assert!(pane.ledger_free(), "{reason}");
    }
}

/// A draft typed while the adapter registers its nonce is read again before the owner's ready, so
/// it is refused with no key and the registration withdrawn.
#[test]
fn a_draft_typed_during_registration_is_refused_before_any_key() {
    use crate::services::codex_tui::busy_inject::{Ledger, Outcome, Request, Veto};
    struct Typing<'a>(&'a HerdrRig, Mutex<Vec<&'static str>>);
    impl Ledger for Typing<'_> {
        fn register(&self) -> bool {
            self.0.answer("pane.read", screen("typed meanwhile"));
            self.1.lock().unwrap().push("register");
            true
        }
        fn withdraw(&self) {
            self.1.lock().unwrap().push("withdraw");
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pane = Pane::start(7_272_544);
    let _registry = pane.rig.registry_on_this_thread();
    open();
    pane.rig.answer("pane.read", screen(""));
    let ledger = Typing(&pane.rig, Mutex::new(Vec::new()));
    let nonce = pane.nonce();
    let request = Request {
        session: &pane.logical,
        rollout: &pane.rollout,
        source: "discord",
        author: "7",
        name: "tester",
        nonce: &nonce,
        text: PROMPT,
        target_turn: T,
    };
    let outcome = crate::services::codex::herdr_turn::busy::inject(&pane.target, &request, &ledger);
    assert_eq!(outcome, Outcome::NotSent(Veto::ComposerUnread));
    assert!(pane.rig.sends().is_empty(), "{:?}", pane.rig.sends());
    assert_eq!(*ledger.1.lock().unwrap(), ["register", "withdraw"]);
}

/// The owner's ready pins the bound physical pane, not the logical session; a pin it cannot take
/// refuses before any key and leaves no ledger row.
#[test]
fn owner_ready_refusal_pins_the_physical_pane_and_sends_nothing() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pane = Pane::start(7_272_545);
    let _registry = pane.rig.registry_on_this_thread();
    open();
    pane.rig.answer("pane.read", screen(""));
    let pinned = Arc::new(Mutex::new(Vec::new()));
    let seen = pinned.clone();
    let target = json!({"host":"herdr","owner":pane.logical,
        "root":pane.rig.socket().display().to_string(),"execution_nonce":pane.execution});
    let leaf = BusyCodexHerdr {
        target: pane.target.clone(),
        writer: pane.physical(),
        input: pane.input(T),
    }
    .leaf();
    let completion = crate::services::claude_tui::owner_input::with_scope(
        "codex".into(),
        target,
        pane.channel,
        Arc::new(|_| Ok(())),
        Arc::new(move |_, key| {
            seen.lock().unwrap().push(key);
            Err("the physical lease is taken".into())
        }),
        Arc::new(|_| Ok(())),
        || leaf(pane.frame()),
    );
    assert_eq!(completion, Ok(Completion::NotSent("LockContended".into())));
    assert_eq!(
        *pinned.lock().unwrap(),
        [format!("herdr:{}", pane.physical())]
    );
    assert!(pane.rig.sends().is_empty(), "{:?}", pane.rig.sends());
    assert!(pane.ledger_free());
}

/// The adapter catches an injection panic and emits one alert carrying this input's context.
#[test]
fn codex_adapter_panic_emits_one_owned_alert() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pane = Pane::start(7_272_546);
    let (completion, events) =
        crate::services::observability::events::test_capture::capture_sync(|| {
            pane.input(T)
                .run(|_, _| panic!("injected busy adapter panic"))
        });
    assert_eq!(
        completion,
        Completion::WriteUncertain("executor_failed".into())
    );
    let alerts: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == "busy_inject_unconfirmed")
        .collect();
    assert_eq!(alerts.len(), 1, "exactly one panic alert: {events:?}");
    let alert = alerts[0];
    assert_eq!(alert.channel_id, Some(pane.channel));
    assert_eq!(alert.payload["nonce"], pane.nonce());
    assert_eq!(alert.payload["detail"], "executor_failed");
    assert_eq!(alert.payload["owner"], true);
    assert!(pane.rig.sends().is_empty());
}

/// A real Codex Modal veto holds the same admission; the drive's timer retries the actual adapter.
#[test]
fn a_codex_modal_is_retried_by_the_drive_without_another_wake() {
    use crate::services::discord::queue_io::input_owner::drive::{self, Drive, Handoff};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pane = Pane::start(7_272_547);
    let _registry = pane.rig.registry_on_this_thread();
    let mut modal = screen("");
    modal["read"]["text"] = json!("Approval required\n› 1. Yes\ngpt-5.4 xhigh · /tmp");
    pane.rig.answer("pane.read", modal);
    pane.with_owner(|runtime, mailbox, prepared| {
        let refused = Arc::new(AtomicBool::new(false));
        let observed = refused.clone();
        let (m, target, writer, logical, rollout, nonce, channel) = (
            mailbox.clone(),
            pane.target.clone(),
            pane.physical(),
            pane.logical.clone(),
            pane.rollout.clone(),
            pane.nonce(),
            pane.channel,
        );
        let handoff: Handoff = Arc::new(move |front| {
            let (m, observed) = (m.clone(), observed.clone());
            let payload = PreparedPayload {
                frame: prepared.frame.clone(),
                sources: prepared.sources.clone(),
                attempt: prepared.attempt.clone(),
            };
            let leaf = BusyCodexHerdr {
                target: target.clone(),
                writer: writer.clone(),
                input: CodexInput {
                    session: logical.clone(),
                    rollout: rollout.clone(),
                    target_turn: T.into(),
                    channel,
                    message: None,
                    source: "discord".into(),
                    author: "7".into(),
                    name: "tester".into(),
                    nonce: nonce.clone(),
                    text: PROMPT.into(),
                },
            }
            .leaf();
            Box::pin(async move {
                let claim = drive::claim_turn(&m, front.1.message_id, &front)
                    .await?
                    .ok_or("missing front")?;
                let completion =
                    pump::submit_front(&m, Some(claim), payload, Arc::new(|| 10_000), leaf).await?;
                observed.store(true, Ordering::SeqCst);
                match completion {
                    Completion::Submitted => Ok(()),
                    other => Err(format!("{other:?}")),
                }
            })
        });
        let drive = Arc::new(Drive {
            mailbox: mailbox.clone(),
            channel: pane.channel,
            handoff,
            recheck: Duration::from_millis(50),
            settled: None,
        });
        runtime.block_on(async {
            drive::wake(drive);
            let deadline = Instant::now() + Duration::from_secs(20);
            while !refused.load(Ordering::SeqCst) {
                assert!(
                    Instant::now() < deadline,
                    "the actual Codex adapter did not settle"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let held = mailbox
                .v2_request(|s| s.as_ref().unwrap().json())
                .await
                .unwrap();
            assert!(held["head_injection"].is_null(), "{held}");
            assert_eq!(held["retained_inputs"], json!([]));
            assert_eq!(held["items"].as_array().unwrap().len(), 1);
            let ticket = held["admission_receipts"][0]["ticket"].clone();
            assert_eq!(held["admission_receipts"].as_array().unwrap().len(), 1);
            assert!(pane.rig.sends().is_empty());
            assert!(pane.ledger_free());
            pane.rig.answer("pane.read", screen(""));
            loop {
                let done = mailbox
                    .v2_request(|s| s.as_ref().unwrap().json())
                    .await
                    .unwrap();
                if done["items"] == json!([]) && done["head_injection"].is_null() {
                    assert_eq!(done["admission_receipts"].as_array().unwrap().len(), 1);
                    assert_eq!(done["admission_receipts"][0]["ticket"], ticket);
                    assert_eq!(done["terminal_identities"].as_array().unwrap().len(), 1);
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the drive did not retry the real Codex adapter: {done}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let sends = pane.rig.sends();
        assert_eq!(sends.len(), 2, "{sends:?}");
        assert_eq!(
            sends[0]["text"],
            format!("\u{1b}[200~{}\u{1b}[201~", pane.frame())
        );
        assert_eq!(sends[1]["keys"], json!(["enter"]));
    });
}

/// A real gate refusal after keyed remains Unknown through both clear and checked purge.
#[test]
fn keyed_then_herdr_gate_refused_stays_owned_through_clear_and_purge() {
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pane = Arc::new(Pane::start(7_272_549));
    let _registry = pane.rig.registry_on_this_thread();
    pane.rig.answer("pane.read", screen(""));
    pane.with_owner(|runtime, mailbox, prepared| {
        let replacement = pane.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let clock: super::super::Clock = Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 1 {
                // Ready has pinned the original provider; start succeeds before the gate rechecks it.
                replacement.rig.run_provider(&replacement.context, true);
            }
            10_000
        });
        let completion = runtime
            .block_on(pump::submit(&mailbox, prepared, clock, pane.leaf(T)))
            .unwrap();
        let before = runtime
            .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
            .unwrap();
        assert_eq!(
            before["head_injection"]["phase"], "unknown",
            "keyed prevents a false not-sent proof: {before}"
        );
        assert_eq!(
            completion,
            Completion::WriteUncertain("unproven: PaneUnavailable".into()),
            "the actual gate refused the replaced provider"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "the refusal followed ready and start"
        );
        assert!(pane.rig.sends().is_empty());
        runtime.block_on(async {
            assert_eq!(before["retained_inputs"], json!([]));
            let context = QueuePersistenceContext::new(
                &crate::services::provider::ProviderKind::Codex,
                "owner-herdr",
                None,
            );
            let cleared = mailbox.clear(context.clone()).await;
            assert!(cleared.discarded_message_ids.is_empty());
            assert!(cleared.persistence.failed.is_empty());
            let captured = mailbox.try_capture_v2().await.unwrap().unwrap();
            let purged = mailbox
                .try_purge_queue_if_unchanged(context, captured)
                .await
                .unwrap();
            assert!(purged.failed.is_none(), "{:?}", purged.failed);
            assert!(
                purged.receipt.is_empty(),
                "the attempted original cannot be purged"
            );
            let after = mailbox
                .v2_request(|s| s.as_ref().unwrap().json())
                .await
                .unwrap();
            assert_eq!(
                after["head_injection"], before["head_injection"],
                "clear and purge retain the exact attempt"
            );
            assert_eq!(after["items"], before["items"]);
            assert_eq!(after["admission_receipts"], before["admission_receipts"]);
            assert_eq!(after["retained_inputs"], json!([]));
        });
    });
}
