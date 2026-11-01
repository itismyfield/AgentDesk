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
    context: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl HerdrOwner {
    fn start(provider: &str, channel: u64) -> Self {
        Self::start_with(provider, channel, PROMPT)
    }

    fn start_with(provider: &str, channel: u64, prompt: &str) -> Self {
        let rig = HerdrRig::start();
        let execution = uuid::Uuid::new_v4().simple().to_string();
        let context = rig.context(&execution);
        rig.run_provider(&context, false);
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
            "queued_generation":1,"text":prompt}))
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
            context,
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
        self.hook_with(provider, host, wanted, PROMPT)
    }

    /// As `hook`, with `prompt` as the turn's text.
    fn hook_with(
        &self,
        provider: &str,
        host: &str,
        wanted: Lineage,
        prompt: &str,
    ) -> scope::LaunchHook {
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
            until_done: None,
        };
        created(
            registration,
            prompt.into(),
            wanted,
            (None, reply_once(None)),
        )
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

    /// Delivered once: the write types exactly `typed`, outside any bracketed-paste wrapper, and
    /// one Enter follows.
    fn delivered_once(&self, typed: &str) {
        let sends = self.rig.sends();
        assert_eq!(sends.len(), 2, "{sends:?}");
        let sent = sends[0]["text"].as_str().unwrap_or_default();
        let unwrapped = sent
            .strip_prefix("\u{1b}[200~")
            .and_then(|s| s.strip_suffix("\u{1b}[201~"));
        assert_eq!(unwrapped.unwrap_or(sent), typed, "{sends:?}");
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
            session_id: None,
            force_fresh: false,
        }
    }

    fn codex_turn<'a>(
        &'a self,
        pool: &'a sqlx::PgPool,
    ) -> crate::services::codex::herdr_turn::CodexHerdrTurn<'a> {
        self.codex_turn_with(pool, PROMPT)
    }

    fn codex_turn_with<'a>(
        &'a self,
        pool: &'a sqlx::PgPool,
        prompt: &'a str,
    ) -> crate::services::codex::herdr_turn::CodexHerdrTurn<'a> {
        crate::services::codex::herdr_turn::CodexHerdrTurn {
            pool,
            owner: self.record("codex", HostedState::Pending).owner,
            channel_id: self.channel,
            endpoint: self.endpoint(),
            row: None,
            prompt,
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

/// A launched Claude Herdr execution's first prompt is reserved on the observed execution, and its
/// pane is bound before the owner's first ready, so pin, gate fence and paste name one pane.
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
    owner.delivered_once(PROMPT);
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

/// A Pending Herdr execution, which may already hold an unheld first prompt, is refused before any
/// hold, reservation, proof, leaf or key, and its original resumes under its ticket as stored.
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
    let same = |a: &PendingQueueItem, b: &PendingQueueItem| a.message_id == b.message_id;
    let mut offered = owner.original.clone();
    offered.queued_generation += 100;
    let resumed = owner.runtime.block_on(owner.mailbox.v2_resume_ticket(
        offered,
        owner.ticket.as_str().into(),
        same,
    ));
    let (_, stored) = resumed.expect("the refused original resumes under its ticket");
    let json = |item: &PendingQueueItem| serde_json::to_value(item).unwrap();
    assert_eq!(
        stored.queued_generation, owner.original.queued_generation,
        "a resumed ticket keeps its stored generation"
    );
    assert_eq!(json(&stored), json(&owner.original));
    let snapshot = owner.snapshot();
    assert_eq!(
        snapshot["items"].as_array().map(Vec::len),
        Some(1),
        "{snapshot}"
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
    owner.delivered_once(PROMPT);
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
    owner.delivered_once(PROMPT);
    assert_eq!(
        heads.lock().unwrap()[0]["attempt"]["target"]["execution_nonce"],
        owner.execution
    );
}

/// A Codex Herdr first prompt is reserved as the text its write types: with LF, CRLF or a bare CR
/// line ending, the head's frame and digest, the history's frame and the sent text are one string.
#[test]
fn owned_herdr_codex_reserves_the_typed_text_for_every_line_ending() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let typed = "first line\nsecond line";
    let sha = |text: &str| {
        format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes())
        )
    };
    for (channel, prompt) in [
        (7_272_611, "first line\nsecond line"),
        (7_272_612, "first line\r\nsecond line"),
        (7_272_613, "first line\rsecond line"),
    ] {
        let owner = HerdrOwner::start("codex", channel);
        let _registry = owner.rig.registry_on_this_thread();
        owner.rig.answer("pane.read", codex_ready());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (at_send, mailbox, handle) = (
            seen.clone(),
            owner.mailbox.clone(),
            owner.runtime.handle().clone(),
        );
        owner.rig.on_send(move |_| {
            let snapshot = handle
                .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
                .unwrap();
            let registry =
                crate::services::tui_input::human_frame::history::v2::Registry::runtime();
            let history = registry.and_then(|r| r.json()).unwrap();
            at_send
                .lock()
                .unwrap()
                .push((snapshot["head_injection"].clone(), history));
        });
        let hook = owner.hook_with("codex", "herdr", Lineage::Created, prompt);
        let _waiting = scope::install_launch(hook);
        let pool = unused_pool(&owner);
        let turn = owner.codex_turn_with(&pool, prompt);
        let record = owner.record("codex", HostedState::Pending);
        let _entered = owner.runtime.enter();
        let result = crate::services::codex::herdr_turn::owned_first_prompt_for_tests(
            &turn,
            &record,
            Lineage::Created,
        );
        assert_eq!(result, Ok(()), "{prompt:?}");
        owner.delivered_once(typed);
        let (head, history) = seen.lock().unwrap()[0].clone();
        assert_eq!(head["payload"]["frame"], typed, "{head}");
        assert_eq!(head["payload"]["digest"], sha(typed), "{head}");
        let frames: Vec<&Value> = ["planned", "live", "passive"]
            .iter()
            .filter_map(|state| history[state].as_array())
            .flatten()
            .filter(|record| record["key"]["attempt"]["nonce"] == format!("{channel:032x}"))
            .map(|record| &record["frame"]["hash"])
            .collect();
        assert_eq!(frames, [&json!(sha(typed))], "{history}");
    }
}

/// The owner's idle Codex Herdr hand-over reserves and hands its submit the typed text of a CRLF
/// prompt, so the reserved bytes are the ones the driver's plan writes.
#[test]
fn owner_idle_codex_herdr_hook_reserves_the_typed_text() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 7_272_614);
    let hook = owner.hook_with("codex", "herdr", Lineage::Existing, "one\r\ntwo\rthree");
    let submitted = Arc::new(Mutex::new(Vec::new()));
    let (seen, mailbox, handle) = (
        submitted.clone(),
        owner.mailbox.clone(),
        owner.runtime.handle().clone(),
    );
    let submit: scope::CreatedSubmit = Box::new(move |frame| {
        let snapshot = handle
            .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
            .unwrap();
        seen.lock().unwrap().push((
            frame,
            snapshot["head_injection"]["payload"]["frame"].clone(),
        ));
        Ok(())
    });
    let target = json!({"host":"herdr","owner":owner.logical,
        "root":owner.rig.socket().display().to_string(),"execution_nonce":owner.execution});
    let transcript = owner._dir.path().join("bound.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    let meta = std::fs::metadata(&transcript).unwrap();
    use std::os::unix::fs::MetadataExt;
    let source = json!({"kind":"existing","session_id":"bound-sid",
        "transcript":transcript.display().to_string(),"file":[meta.dev(), meta.ino()],
        "offset":meta.len()});
    let _entered = owner.runtime.enter();
    let result = hook(Lineage::Existing, target, source, None, submit);
    assert_eq!(result, Ok(()));
    let submitted = submitted.lock().unwrap();
    assert_eq!(
        *submitted,
        [("one\ntwo\nthree".to_string(), json!("one\ntwo\nthree"))]
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
    for (case, channel, script_prompt, foreign_field, created) in [
        (
            "other",
            7_272_606,
            "a prompt the owner never reserved",
            None,
            false,
        ),
        ("same", 7_272_607, PROMPT, None, true),
        (
            "foreign-nonce",
            7_272_616,
            PROMPT,
            Some("execution_nonce"),
            false,
        ),
        ("foreign-root", 7_272_617, PROMPT, Some("root"), false),
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
        let _waiting = scope::install_launch(Box::new(
            move |lineage, mut target, source, writer, submit| {
                record.lock().unwrap().push(target.clone());
                if let Some(field) = foreign_field {
                    target[field] = json!("another-execution");
                }
                inner(lineage, target, source, writer, submit)
            },
        ));
        let _entered = owner.runtime.enter();
        let result = crate::services::codex::herdr_turn::owned_spawn_for_tests(
            &context,
            (&session, "/tmp"),
            script_prompt,
            "sleep 30".into(),
        );
        if foreign_field.is_some() {
            assert!(
                result
                    .as_ref()
                    .is_err_and(|why| why.contains("foreign argv execution")),
                "matching prompt with a foreign context is refused: {case}: {result:?}"
            );
        }
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

/// Keeps the scope alive through the rendezvous on either a write or a refusal.
fn write_herdr_between(
    logical: &str,
    owner: &HerdrOwner,
    pane: &crate::services::session_host::HerdrTarget,
    between: &dyn Fn(),
) -> Result<(), String> {
    use crate::services::session_host::{HerdrMutation as Mutation, HostMutation};
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
            let wrote = (|| {
                scope::bind_writer(&physical)?;
                pane.pin(Mutation::Input)
                    .map_err(|refusal| format!("{refusal:?}"))?;
                match pane.send_paste(logical) {
                    Ok(HostMutation::Confirmed) => Ok(()),
                    other => Err(format!("{other:?}")),
                }
            })();
            between();
            wrote
        },
    )
}

/// A pin refusal still reaches the rendezvous; the parent can release and inspect the error.
#[test]
fn a_refused_first_herdr_pin_reaches_the_rendezvous() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_618);
    let _registry = owner.rig.registry_on_this_thread();
    let pane =
        crate::services::claude::herdr_turn::gate(&owner.record("claude", HostedState::Bound))
            .unwrap();
    owner.rig.run_provider(&owner.context, true);
    let reached = std::sync::atomic::AtomicUsize::new(0);
    let result = write_herdr_between("logical-refused", &owner, &pane, &|| {
        reached.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    assert!(
        result
            .as_ref()
            .is_err_and(|why| why.contains("ProviderReplaced")),
        "pin must reject the replaced provider: {result:?}"
    );
    assert_eq!(
        reached.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a refused first pin still completes the rendezvous"
    );
    assert!(owner.rig.sends().is_empty());
}

/// Through the real gate a second logical owner of a held pane gets no byte, another socket's pane
/// writes meanwhile, and the pane takes the second owner once the first scope ends.
#[test]
fn two_herdr_rigs_serialize_one_physical_pane_through_the_gate() {
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
    let (held, release) = (std::sync::Barrier::new(2), std::sync::Barrier::new(2));
    // Results are asserted after the scope, so a failure never leaves the first writer parked.
    let (first, refused, other) = std::thread::scope(|threads| {
        let first = threads.spawn(|| {
            write_herdr_between("logical-a", &a, &pane_a, &|| {
                held.wait();
                release.wait();
            })
        });
        held.wait();
        let refused = threads
            .spawn(|| write_herdr_between("logical-b", &a, &pane_a, &|| {}))
            .join();
        let other = threads
            .spawn(|| write_herdr_between("logical-c", &b, &pane_b, &|| {}))
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
    assert_eq!(
        write_herdr_between("logical-b", &a, &pane_a, &|| {}),
        Ok(()),
        "released"
    );
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

/// With the pane bound before the owner's first ready, a submit that readies before its paste pins
/// that pane through the real V2 pin, so the paste after it is not a writer transfer.
#[test]
fn owned_herdr_ready_before_content_pins_the_bound_pane() {
    use crate::services::claude_tui::guarded_outcome::SubmissionError;
    use crate::services::session_host::{HerdrMutation as Mutation, HostMutation};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_610);
    let _registry = owner.rig.registry_on_this_thread();
    owner.rig.answer("pane.read", claude_ready());
    let pane =
        crate::services::claude::herdr_turn::gate(&owner.record("claude", HostedState::Bound))
            .unwrap();
    let root = owner.rig.socket().display().to_string();
    let target = json!({"host":"herdr","owner":owner.logical,"root":root,
        "execution_nonce":owner.execution});
    let source = json!({"kind":"pre_source","expected_session_id":null,
        "expected_transcript":null,"binding_watermark":null});
    let submit: scope::CreatedSubmit = Box::new(move |frame| {
        scope::ready_payload().map_err(|refusal| SubmissionError::from(format!("{refusal:?}")))?;
        pane.pin(Mutation::Input)
            .map_err(|refusal| SubmissionError::from(format!("{refusal:?}")))?;
        match pane.send_paste(&frame) {
            Ok(HostMutation::Confirmed) => Ok(()),
            other => Err(format!("{other:?}").into()),
        }
    });
    let hook = owner.hook("claude", "herdr", Lineage::Created);
    let _entered = owner.runtime.enter();
    let writer = Some(format!("{root}#{PANE}"));
    assert_eq!(
        hook(Lineage::Created, target, source, writer, submit),
        Ok(())
    );
    let sends = owner.rig.sends();
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert!(
        sends[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains(PROMPT)
    );
    assert!(matches!(owner.bind(), TurnBind::Settled(_)));
}

/// Herdr input admitted on this thread, as on an unstopped node.
fn admit_herdr_here() {
    let stop = std::env::temp_dir().join(format!("adk-oh-none-{}", uuid::Uuid::new_v4()));
    std::mem::forget(force_for_test(Admission::new(true, Some(stop))));
}

/// The public Bound turn entry reaches followup_write; each newline form reserves its typed bytes.
#[test]
fn bound_codex_followup_reserves_and_writes_each_normalized_prompt() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::codex::herdr_turn::{self, CodexHerdrPorts};
    use crate::services::session_host::HerdrTarget;
    use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, register_tmux_runtime_binding};
    use sha2::{Digest, Sha256};
    struct Bound;
    impl CodexHerdrPorts for Bound {
        fn launch_host(&self) -> Option<Arc<dyn crate::services::herdr_launch::HerdrLaunchHost>> {
            None
        }
        fn attach(
            &self,
            _: &HostedOwner,
            _: &HostedExecution,
            _: &crate::services::tui_prompt_dedupe::binding_events::SourceId,
            _: &HerdrTarget,
        ) -> Result<bool, String> {
            panic!("Bound followup never attaches a fresh source")
        }
        fn confirm_bound(
            &self,
            _: &HostedOwner,
            record: &HostedExecution,
            _: &HerdrTarget,
        ) -> Result<(), String> {
            assert_eq!(record.state, HostedState::Bound);
            Ok(())
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _effort = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_REASONING_EFFORT",
        std::ffi::OsStr::new("high"),
    );
    for (index, prompt) in ["one\ntwo\nthree", "one\r\ntwo\r\nthree", "one\rtwo\rthree"]
        .into_iter()
        .enumerate()
    {
        let owner = HerdrOwner::start_with("codex", 7_272_620 + index as u64, prompt);
        let _registry = owner.rig.registry_on_this_thread();
        owner.rig.answer("pane.read", codex_ready());
        let transcript = owner._dir.path().join("bound.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        register_tmux_runtime_binding(
            &owner.logical,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: transcript.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("bound-followup-sid".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let instructions =
            crate::services::codex::compose_codex_developer_instructions(None, Some(&[]));
        let options = crate::services::codex::CodexLaunchOptions::new("")
            .with_developer_instructions(instructions.as_deref())
            .with_reasoning_effort(Some("high"))
            .with_readonly_mode(crate::services::provider::is_readonly_tool_policy(
                Some(&[]),
            ))
            .with_cwd(Some("/tmp"));
        let fingerprint =
            crate::services::codex_tui::warm_followup::codex_tui_launch_options_fingerprint(
                &options,
            );
        crate::services::codex_tui::session::write_codex_tui_launch_options_fingerprint(
            &owner.logical,
            &format!("{} {fingerprint}", owner.execution),
        )
        .unwrap();
        let cancel = Arc::new(crate::services::provider::CancelToken::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (at_send, mailbox, handle) = (
            seen.clone(),
            owner.mailbox.clone(),
            owner.runtime.handle().clone(),
        );
        owner.rig.on_send(move |_| {
            let snapshot = handle
                .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
                .unwrap();
            let history = crate::services::tui_input::human_frame::history::v2::Registry::runtime()
                .and_then(|r| r.json())
                .unwrap();
            at_send
                .lock()
                .unwrap()
                .push((snapshot["head_injection"].clone(), history));
        });
        let hook = owner.hook_with("codex", "herdr", Lineage::Existing, prompt);
        let stop = cancel.clone();
        let _waiting =
            scope::install_launch(Box::new(move |lineage, target, source, writer, submit| {
                let result = hook(lineage, target, source, writer, submit);
                stop.cancelled
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                result
            }));
        let pool = unused_pool(&owner);
        let record = crate::db::dispatched_sessions::hosted_execution::HostedRecord::Known(
            owner.record("codex", HostedState::Bound),
        );
        let mut turn = owner.codex_turn_with(&pool, prompt);
        turn.row = Some(&record);
        turn.cancel = Some(cancel);
        let (sender, _messages) = std::sync::mpsc::channel();
        let _entered = owner.runtime.enter();
        let result = herdr_turn::execute(turn, &Bound, sender);
        assert_eq!(result, Ok(()), "{prompt:?}");
        owner.delivered_once("one\ntwo\nthree");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "followup_write must send text and Enter");
        let (head, history) = &seen[0];
        let hash = format!("{:x}", Sha256::digest(b"one\ntwo\nthree"));
        assert_eq!(head["payload"]["frame"], "one\ntwo\nthree");
        assert_eq!(head["payload"]["digest"], hash);
        assert_eq!(
            head["attempt"]["source"]["session_id"],
            "bound-followup-sid"
        );
        let hashes: Vec<_> = ["planned", "live", "passive"]
            .iter()
            .filter_map(|state| history[state].as_array())
            .flatten()
            .map(|row| row.get("registration").unwrap_or(row))
            .filter(|row| row["key"]["attempt"]["nonce"] == format!("{:032x}", owner.channel))
            .map(|row| row["frame"]["hash"].clone())
            .collect();
        assert_eq!(hashes, [json!(hash)], "{history}");
    }
}
