//! The owner's launch hook at each Herdr and Codex create driver's hand-over point: a real Herdr
//! socket rig or tmux server, the owner's V2 actor, and what reached the pane.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::discord::queue_io::input_owner::admission::{
    AdmissionTicket, TurnBind, admit, exact_turn,
};
use crate::services::herdr_admission::{Admission, force_for_test};
use crate::services::herdr_launch::HerdrLaunchEndpoint;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use scope::Lineage;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const PROMPT: &str = "owner herdr first prompt";

/// Claude's idle composer, as Herdr reads the pane.
fn claude_ready() -> Value {
    let border = "─".repeat(52);
    let text = format!(
        "✻ Baked for 2s\n\n{border}\n❯ \n{border}\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    );
    json!({"type":"pane_read","read":{"pane_id":PANE,"source":"recent_unwrapped","text":text,
        "revision":1,"truncated":false}})
}

/// Codex's empty composer, as Herdr reads the pane.
fn codex_ready() -> Value {
    json!({"type":"pane_read","read":{"pane_id":PANE,"source":"recent_unwrapped",
        "text":"\n› \ngpt-5.4 xhigh · /tmp","revision":1,"truncated":false}})
}

/// An owner over a real Herdr rig whose provider runs `execution` under the logical session
/// `logical`, with one admitted original on `channel`.
struct HerdrOwner {
    rig: HerdrRig,
    runtime: tokio::runtime::Runtime,
    _registry: ChannelMailboxRegistry,
    mailbox: ChannelMailboxHandle,
    ticket: AdmissionTicket,
    original: PendingQueueItem,
    channel: u64,
    logical: String,
    execution: String,
    _dir: tempfile::TempDir,
}

impl HerdrOwner {
    fn start(provider: &str, channel: u64) -> Self {
        let rig = HerdrRig::start();
        let execution = uuid::Uuid::new_v4().simple().to_string();
        rig.run_provider(&rig.context(&execution), false);
        let logical = format!("AgentDesk-{provider}-owned-{channel}");
        let marker = crate::services::tmux_common::session_temp_path(&logical, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        let open = admit_herdr_here;
        open();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .on_thread_start(open)
            .build()
            .unwrap();
        let entered = runtime.enter();
        let registry = ChannelMailboxRegistry::default();
        let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
        let id = 8_000_000_000_000_009_000 + channel % 1000;
        let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
            "message_id":id,"channel_id":channel,"source_message_ids":[id],
            "queued_generation":1,"text":PROMPT}))
        .unwrap();
        let ticket = AdmissionTicket::new();
        runtime.block_on(async {
            let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
                .unwrap()
                .join(format!("{provider}/input-owner-herdr-cold"));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{channel}.json"));
            mailbox.v2_adopt(path, "input-owner".into()).await.unwrap();
            let admitted = admit(&mailbox, &original, ticket.clone()).await;
            assert!(matches!(
                admitted,
                crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
            ));
        });
        drop(entered);
        Self {
            rig,
            runtime,
            _registry: registry,
            mailbox,
            ticket,
            original,
            channel,
            logical,
            execution,
            _dir: tempfile::tempdir().unwrap(),
        }
    }

    /// The recorded execution of the rig's pane, `state` as the turn host read it.
    fn record(&self, provider: &str, state: HostedState) -> HostedExecution {
        let owner = HostedOwner {
            provider: provider.into(),
            discord_token_hash: "owned-herdr".into(),
            channel_id: self.channel.to_string(),
            logical_key: self.logical.clone(),
            owner_node: NODE.into(),
            runtime_root: "/adk/runtime".into(),
        };
        HostedExecution {
            schema: 1,
            state,
            execution_nonce: self.execution.clone(),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: KEY.into(),
                socket_addr: self.rig.socket().display().to_string(),
                named_session: SESSION.into(),
                pane_id: PANE.into(),
            }),
            expected: Some(self.rig.expected(&self.execution)),
            source_ref: SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: owner.channel_id.clone(),
                provider: provider.into(),
                logical_key: self.logical.clone(),
                execution_nonce: self.execution.clone(),
                initial_source: None,
                baseline_event_seq: None,
            },
            owner,
        }
    }

    /// The dispatch owner waiting for a `wanted` hand-over, prepared before any launch on a
    /// target that names no execution the driver will observe.
    fn wait_for(&self, provider: &str, wanted: Lineage) -> scope::LaunchGuard {
        scope::install_launch(self.hook(provider, "herdr", wanted))
    }

    fn hook(&self, provider: &str, host: &str, wanted: Lineage) -> scope::LaunchHook {
        let nonce = format!("{:032x}", self.channel);
        let registration = Registration {
            mailbox: self.mailbox.clone(),
            execution: Execution::LocalTui,
            origin: InputOrigin::Discord,
            phase: target::Phase::Absent,
            attempt: json!({"nonce":nonce,"provider":provider,
                "target":{"host":host,"owner":self.logical,"root":"/prepared",
                    "execution_nonce":"0".repeat(32)},
                "source":{"kind":"pre_source","expected_session_id":null,
                    "expected_transcript":null,"binding_watermark":null}}),
            sources: vec![self.original.message_id.to_string()],
            transcript: self._dir.path().join("unused.jsonl"),
            program: "tmux".into(),
            herdr: None,
            author: "7".into(),
            name: "tester".into(),
            clock: Arc::new(|| 10_000),
            timing: &crate::services::claude_tui::busy_inject::TIMING,
        };
        created(registration, PROMPT.into(), wanted)
    }

    /// The owner's head as the pane saw it at each send.
    fn heads_at_send(&self) -> Arc<Mutex<Vec<Value>>> {
        let heads = Arc::new(Mutex::new(Vec::new()));
        let (seen, mailbox, handle) = (
            heads.clone(),
            self.mailbox.clone(),
            self.runtime.handle().clone(),
        );
        self.rig.on_send(move |_| {
            let snapshot = handle
                .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
                .unwrap();
            seen.lock()
                .unwrap()
                .push(snapshot["head_injection"].clone());
        });
        heads
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

    /// Delivered once: the paste carries exactly the reserved prompt and one Enter follows.
    fn delivered_once(&self) {
        let sends = self.rig.sends();
        assert_eq!(sends.len(), 2, "{sends:?}");
        let pasted = sends[0]["text"].as_str().unwrap_or_default();
        assert!(pasted.contains(PROMPT), "{sends:?}");
        assert_eq!(sends[1]["keys"], json!(["enter"]));
        let snapshot = self.snapshot();
        assert!(snapshot["head_injection"].is_null(), "{snapshot}");
        assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
        assert!(matches!(self.bind(), TurnBind::Settled(_)));
    }

    /// Nothing reserved, nothing written: the original stays queued for its own driver.
    fn untouched(&self) {
        assert!(self.rig.sends().is_empty(), "{:?}", self.rig.sends());
        let snapshot = self.snapshot();
        assert!(snapshot["head_injection"].is_null(), "{snapshot}");
        assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
        assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
        assert_eq!(self.bind(), TurnBind::NotArmed);
    }

    fn claude_turn<'a>(
        &'a self,
        pool: &'a sqlx::PgPool,
    ) -> crate::services::claude::herdr_turn::HerdrTurn<'a> {
        crate::services::claude::herdr_turn::HerdrTurn {
            pool,
            owner: self.record("claude", HostedState::Bound).owner,
            channel_id: self.channel,
            endpoint: self.endpoint(),
            row: None,
            prompt: PROMPT,
            working_dir: "/tmp",
            system_prompt: None,
            model: None,
            hook_endpoint: None,
            cancel: None,
        }
    }

    fn codex_turn<'a>(
        &'a self,
        pool: &'a sqlx::PgPool,
    ) -> crate::services::codex::herdr_turn::CodexHerdrTurn<'a> {
        crate::services::codex::herdr_turn::CodexHerdrTurn {
            pool,
            owner: self.record("codex", HostedState::Pending).owner,
            channel_id: self.channel,
            endpoint: self.endpoint(),
            row: None,
            prompt: PROMPT,
            working_dir: "/tmp",
            system_prompt: None,
            allowed_tools: &[],
            model: None,
            fast_mode: None,
            goals: None,
            compact_token_limit: None,
            cancel: None,
        }
    }

    fn endpoint(&self) -> HerdrLaunchEndpoint {
        HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: KEY.into(),
            socket_addr: self.rig.socket().display().to_string(),
            herdr_session: SESSION.into(),
        }
    }
}

/// A pool no test path connects, made inside the owner's runtime as sqlx requires.
fn unused_pool(owner: &HerdrOwner) -> sqlx::PgPool {
    let _entered = owner.runtime.enter();
    sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused_owned_herdr_test")
        .unwrap()
}

/// A launched Claude Herdr execution's first prompt is reserved on the execution the driver
/// observed, not the prepared one; the owner binds that pane before its first ready, so the
/// owner's physical pin, the gate's fence and the paste all name the same pane.
#[test]
fn owned_herdr_claude_launch_reserves_the_observed_execution_and_its_pane() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_601);
    let _registry = owner.rig.registry_on_this_thread();
    owner.rig.answer("pane.read", claude_ready());
    let heads = owner.heads_at_send();
    let _waiting = owner.wait_for("claude", Lineage::Created);
    let pool = unused_pool(&owner);
    let turn = owner.claude_turn(&pool);
    let record = owner.record("claude", HostedState::Bound);
    let transcript = owner._dir.path().join("launched.jsonl");
    let _entered = owner.runtime.enter();
    let result = crate::services::claude::herdr_turn::owned_prompt_for_tests(
        &turn,
        &record,
        ("launched-sid", transcript.clone()),
        Lineage::Created,
    );
    assert_eq!(result, Ok(()));
    owner.delivered_once();
    let heads = heads.lock().unwrap();
    let at_paste = &heads[0]["attempt"];
    assert_eq!(
        at_paste["target"],
        json!({"host":"herdr","owner":owner.logical,"root":owner.rig.socket().display().to_string(),
            "execution_nonce":owner.execution})
    );
    assert_eq!(at_paste["source"]["expected_session_id"], "launched-sid");
    assert_eq!(
        at_paste["source"]["expected_transcript"],
        transcript.display().to_string()
    );
    assert_eq!(
        scope::launch_refuses_live(),
        Ok(()),
        "the hook was consumed"
    );
}

/// A Pending Herdr execution is not a create: whether or not it already took a first prompt, the
/// owner refuses it before any reservation, so no hold, proof, leaf or key exists.
#[test]
fn owned_herdr_pending_execution_is_refused_before_any_reservation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_602);
    let _registry = owner.rig.registry_on_this_thread();
    owner.rig.answer("pane.read", claude_ready());
    let _waiting = owner.wait_for("claude", Lineage::Created);
    let pool = unused_pool(&owner);
    let turn = owner.claude_turn(&pool);
    let record = owner.record("claude", HostedState::Pending);
    let _entered = owner.runtime.enter();
    let result = crate::services::claude::herdr_turn::owned_prompt_for_tests(
        &turn,
        &record,
        ("pending-sid", owner._dir.path().join("pending.jsonl")),
        Lineage::Pending,
    );
    assert_eq!(result, Err("NotSent(\"lineage_refused: Pending\")".into()));
    owner.untouched();
    assert_eq!(
        crate::services::claude::herdr_turn::not_held(&owner.execution),
        Ok(()),
        "no input hold was taken"
    );
}

/// A bound idle Claude Herdr pane takes its next prompt as the owner's effect on the existing
/// source it is bound to.
#[test]
fn owned_herdr_claude_idle_writes_the_bound_source_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_603);
    let _registry = owner.rig.registry_on_this_thread();
    owner.rig.answer("pane.read", claude_ready());
    let heads = owner.heads_at_send();
    let _waiting = owner.wait_for("claude", Lineage::Existing);
    let pool = unused_pool(&owner);
    let turn = owner.claude_turn(&pool);
    let record = owner.record("claude", HostedState::Bound);
    let transcript = owner._dir.path().join("bound.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();
    let _entered = owner.runtime.enter();
    let result = crate::services::claude::herdr_turn::owned_prompt_for_tests(
        &turn,
        &record,
        ("bound-sid", transcript.clone()),
        Lineage::Existing,
    );
    assert_eq!(result, Ok(()));
    owner.delivered_once();
    let source = &heads.lock().unwrap()[0]["attempt"]["source"];
    assert_eq!(source["kind"], "existing", "{source}");
    assert_eq!(source["session_id"], "bound-sid", "{source}");
}

/// A fresh Codex Herdr execution's first prompt is the owner's effect on the recorded execution
/// and pane; a Pending one is refused before any reservation.
#[test]
fn owned_herdr_codex_first_prompt_takes_only_a_fresh_execution() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 7_272_604);
    let _registry = owner.rig.registry_on_this_thread();
    owner.rig.answer("pane.read", codex_ready());
    let pool = unused_pool(&owner);
    let turn = owner.codex_turn(&pool);
    let record = owner.record("codex", HostedState::Pending);
    let _entered = owner.runtime.enter();
    {
        let _waiting = owner.wait_for("codex", Lineage::Created);
        let refused = crate::services::codex::herdr_turn::owned_first_prompt_for_tests(
            &turn,
            &record,
            Lineage::Pending,
        );
        assert_eq!(refused, Err("NotSent(\"lineage_refused: Pending\")".into()));
        owner.untouched();
    }
    let heads = owner.heads_at_send();
    let _waiting = owner.wait_for("codex", Lineage::Created);
    let result = crate::services::codex::herdr_turn::owned_first_prompt_for_tests(
        &turn,
        &record,
        Lineage::Created,
    );
    assert_eq!(result, Ok(()));
    owner.delivered_once();
    assert_eq!(
        heads.lock().unwrap()[0]["attempt"]["target"]["execution_nonce"],
        owner.execution
    );
}

/// A Codex tmux create carries the reserved frame as its argv prompt on the execution its launch
/// prepared; a launch script carrying another prompt creates nothing and is never retried.
#[test]
fn owned_codex_absence_creates_only_with_the_reserved_prompt() {
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let exists = |session: &str| {
        std::process::Command::new(&f.real)
            .args(["-L", &f.socket, "-f", "/dev/null", "has-session", "-t"])
            .arg(format!("={session}"))
            .status()
            .unwrap()
            .success()
    };
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    for (case, channel, script_prompt, created) in [
        (
            "other",
            7_272_606,
            "a prompt the owner never reserved",
            false,
        ),
        ("same", 7_272_607, PROMPT, true),
    ] {
        let owner = HerdrOwner::start("codex", channel);
        let session = format!("owned-absence-{case}");
        let nonce = format!("{:032x}", channel + 1);
        let context: crate::services::tui_prompt_dedupe::binding_context::BindingContext =
            serde_json::from_value(json!({"schema":1,"provider":"codex",
                "created_at":chrono::Utc::now(),"execution_nonce":nonce,"tmux_session":session,
                "channel_id":channel,"owner_runtime_root":root,"host":"owned-absence",
                "expected_native_session_id":null,"launch_mode":"fresh","provider_root":null}))
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let inner = owner.hook("codex", "tmux", Lineage::Created);
        let record = seen.clone();
        let _waiting =
            scope::install_launch(Box::new(move |lineage, target, source, writer, submit| {
                record.lock().unwrap().push(target.clone());
                inner(lineage, target, source, writer, submit)
            }));
        let _entered = owner.runtime.enter();
        let result = crate::services::codex::herdr_turn::owned_spawn_for_tests(
            &context,
            (&session, "/tmp"),
            script_prompt,
            "sleep 30".into(),
        );
        assert_eq!(result.is_ok(), created, "{case}: {result:?}");
        assert_eq!(exists(&session), created, "{case}");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [json!({"host":"tmux","owner":session,"root":root,"execution_nonce":nonce})],
            "{case}"
        );
        let snapshot = owner.snapshot();
        if created {
            assert!(snapshot["head_injection"].is_null(), "{snapshot}");
            assert!(matches!(owner.bind(), TurnBind::Settled(_)));
        } else {
            assert_eq!(snapshot["head_injection"]["phase"], "unknown", "{snapshot}");
            assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
        }
    }
}

/// One owner per physical Herdr pane at a time through the real gate: a second logical owner of
/// the same pane gets no byte while the first holds it, a pane on another socket writes meanwhile,
/// and the pane takes the second owner once the first scope ends.
#[test]
fn two_herdr_rigs_serialize_one_physical_pane_through_the_gate() {
    use crate::services::session_host::{HerdrMutation as Mutation, HerdrTarget, HostMutation};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (a, b) = (
        HerdrOwner::start("claude", 7_272_608),
        HerdrOwner::start("claude", 7_272_609),
    );
    let gate = |owner: &HerdrOwner| {
        let _registry = owner.rig.registry_on_this_thread();
        crate::services::claude::herdr_turn::gate(&owner.record("claude", HostedState::Bound))
            .unwrap()
    };
    let (pane_a, pane_b) = (gate(&a), gate(&b));
    // One logical owner's paste through `pane`, holding its scope across `between`.
    let write = |logical: &str, owner: &HerdrOwner, pane: &HerdrTarget, between: &dyn Fn()| {
        admit_herdr_here();
        let root = owner.rig.socket().display().to_string();
        let physical = format!("{root}#{PANE}");
        let target = json!({"host":"herdr","owner":logical,"root":root,"execution_nonce":"h"});
        scope::with_scope(
            "claude".into(),
            target,
            owner.channel,
            Arc::new(|_| Ok(())),
            Arc::new(|_, _| Ok(())),
            Arc::new(|_| Ok(())),
            || {
                scope::bind_writer(&physical)?;
                pane.pin(Mutation::Input)
                    .map_err(|refusal| format!("{refusal:?}"))?;
                let wrote = pane.send_paste(logical);
                between();
                match wrote {
                    Ok(HostMutation::Confirmed) => Ok(()),
                    other => Err(format!("{other:?}")),
                }
            },
        )
    };
    let (held, release) = (std::sync::Barrier::new(2), std::sync::Barrier::new(2));
    // Results are asserted after the scope, so a failure never leaves the first writer parked.
    let (first, refused, other) = std::thread::scope(|threads| {
        let first = threads.spawn(|| {
            write("logical-a", &a, &pane_a, &|| {
                held.wait();
                release.wait();
            })
        });
        held.wait();
        let refused = threads
            .spawn(|| write("logical-b", &a, &pane_a, &|| {}))
            .join();
        let other = threads
            .spawn(|| write("logical-c", &b, &pane_b, &|| {}))
            .join();
        release.wait();
        (first.join().unwrap(), refused.unwrap(), other.unwrap())
    });
    assert_eq!(first, Ok(()));
    assert!(
        refused
            .as_ref()
            .is_err_and(|why| why.contains("owner_content_fence")),
        "same pane, second owner: {refused:?}"
    );
    assert_eq!(other, Ok(()), "another socket's pane");
    assert_eq!(write("logical-b", &a, &pane_a, &|| {}), Ok(()), "released");
    let pasted = |owner: &HerdrOwner| {
        let sends = owner.rig.sends();
        let texts = sends.iter().filter_map(|send| send["text"].as_str());
        texts.map(str::to_owned).collect::<Vec<_>>()
    };
    let on_a = pasted(&a);
    assert_eq!(on_a.len(), 2, "{on_a:?}");
    assert!(
        on_a[0].contains("logical-a") && on_a[1].contains("logical-b"),
        "{on_a:?}"
    );
    assert_eq!(pasted(&b).len(), 1);
}

/// Herdr input admitted on this thread, as on an unstopped node.
fn admit_herdr_here() {
    let stop = std::env::temp_dir().join(format!("adk-oh-none-{}", uuid::Uuid::new_v4()));
    std::mem::forget(force_for_test(Admission::new(None, Some(stop))));
}
