//! Busy source decisions through the owner's real intake, with only a Herdr socket behind it.
use super::*;
use crate::services::discord::queue_io::input_owner::admission::{
    Admission as OwnerAdmission, AdmissionTicket, admit,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{self as discord, SharedData, inflight};
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::{self, test_override};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::writer::adoption::{LegacyCursor, LegacyView};
use crate::services::tui_o::writer::host::{
    self, HostParts, ManagedWriterHandle, test_io::TestHost,
};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, OwnerFront, PendingQueueItem};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::os::unix::fs::PermissionsExt;

const MESSAGE: u64 = 1_570_000_811_403_001_001;
const INPUT: &str = "join the running Herdr turn now";

struct DeliveredLegacy {
    logical: String,
    path: PathBuf,
    end: u64,
}

impl LegacyView for DeliveredLegacy {
    fn started(&self) -> bool {
        true
    }
    fn cursor(&self, logical: &str) -> LegacyCursor {
        match logical == self.logical {
            true => LegacyCursor::Bound {
                path: self.path.clone(),
                offset: self.end,
            },
            false => LegacyCursor::NoPane,
        }
    }
    fn late_cursor(&self, logical: &str) -> Result<LegacyCursor, String> {
        Ok(self.cursor(logical))
    }
    fn frontier(&self, channel: u64, logical: &str, eof: u64) -> Option<u64> {
        (channel == CHANNEL && logical == self.logical && eof >= self.end).then_some(self.end)
    }
    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

thread_local! {
    static WORKER_GUARDS: std::cell::RefCell<Vec<Box<dyn std::any::Any>>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct Intake {
    shared: Arc<SharedData>,
    mailbox: ChannelMailboxHandle,
    runtime: tokio::runtime::Runtime,
    hosts: Vec<ManagedWriterHandle>,
    server: tokio::task::JoinHandle<()>,
    _api: discord::admin_host_guard::tests::Recorder,
    _config: Guard,
    _routing: Guard,
    _tmux: Guard,
    _boot: test_override::ChannelsGuard,
}

fn assert_idle_source(
    fx: &Fixture,
    started: &Started,
    shared: &Arc<SharedData>,
    host: &crate::services::turn_host::TurnHost,
) {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::claude_tui::host_input::busy::{BusyOperation, BusyState};
    use crate::services::claude_tui::source_verify::{self, Observation, PinJudgment};
    use crate::services::tui_prompt_dedupe::{self, binding_events};
    use std::os::unix::fs::MetadataExt;

    let crate::services::turn_host::TurnHost::Herdr(plan) = host else {
        panic!("the initial source belongs to a resolved Herdr plan");
    };
    let HostedRecord::Known(record) = &plan.row.as_ref().unwrap().record else {
        panic!("the initial execution row is known");
    };
    assert_eq!(record.state, HostedState::Bound);
    assert_eq!(
        record.owner, fx.owner,
        "the launched execution keeps its full owner"
    );
    assert_eq!(record.source_ref.execution_nonce, record.execution_nonce);
    assert_eq!(record.source_ref.logical_key, fx.logical());
    assert_eq!(record.owner.discord_token_hash, shared.token_hash);
    assert_eq!(record.owner.owner_node, plan.endpoint.execution_node);
    assert_eq!(
        Path::new(&record.owner.runtime_root),
        crate::config::runtime_root().unwrap()
    );
    let target = herdr_endpoints()
        .target(record)
        .expect("the exact Bound location resolves");
    assert!(
        target.execution_alive(),
        "the captured physical execution is still alive"
    );
    let capture = target
        .capture_ansi(-80)
        .expect("the initial physical pane remains readable");
    assert!(
        !crate::services::claude_tui::busy_inject::modal(&capture),
        "the initial pane is not modal"
    );
    assert_eq!(
        cutover::peek_o_owns_tui_output_for_channel(CHANNEL, Some(RuntimeHandoffKind::ClaudeTui)),
        Ok(true)
    );
    assert!(
        host::channel_accepts(CHANNEL),
        "the actual O writer still accepts this source"
    );
    let (pin, history) =
        binding_events::claude_history_at(CHANNEL, fx.logical(), Some(&record.execution_nonce))
            .expect("the initial execution's binding log is readable");
    let (pin, pinned_at) = pin.expect("the initial pane has a verified global pin");
    assert!(
        history.complete,
        "the initial nonce-scoped history is complete: {history:?}"
    );
    assert!(
        history.awaiting.is_none(),
        "the initial source is not awaiting a clear: {history:?}"
    );
    let current = history
        .current
        .as_ref()
        .expect("the initial nonce owns a current source");
    assert_eq!(
        current.pin.as_ref(),
        Some(&pin),
        "the nonce's current pin equals the global pin"
    );
    assert_eq!(current.session, pin.session_id);
    assert_eq!(pin.session_id, started.session);
    assert_eq!(pin.path, started.path);
    let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical())
        .expect("the initial source has its actual current runtime binding");
    assert_eq!(binding.runtime_kind, RuntimeHandoffKind::ClaudeTui);
    assert_eq!(
        binding.session_id.as_deref(),
        Some(started.session.as_str())
    );
    assert_eq!(Path::new(&binding.output_path), started.path);
    let opened = source_verify::observe_transcript(&pin.path);
    assert_eq!(
        source_verify::judge_pin(
            Some(&pin),
            pinned_at,
            &pin.session_id,
            &pin.path,
            Observation::Opened(opened.as_ref())
        ),
        PinJudgment::Verified(pin.clone())
    );
    let file = std::fs::metadata(&pin.path).unwrap();
    assert_eq!((file.dev(), file.ino()), (pin.dev, pin.ino));
    assert_eq!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&started.path),
        crate::services::tui_turn_state::TuiTurnState::Idle,
        "the launch completed before a later fixture changes the source"
    );
    assert_eq!(
        BusyOperation::observe_hosted(CHANNEL, fx.logical(), host, &shared.token_hash).state(),
        BusyState::Idle,
        "the complete initial source reaches the same actual busy observer"
    );
}

impl Intake {
    fn new(fx: &Fixture, started: &Started) -> Self {
        let label = fx.logical().strip_prefix("AgentDesk-claude-").unwrap();
        let config: crate::config::Config = serde_json::from_value(json!({
            "server": {}, "providers": {"claude": {"runtime": "tui"}},
            "agents": [{"id": "busy-entry", "name": "Busy entry", "provider": "claude",
                "channels": {"claude": {"id": CHANNEL.to_string(), "name": label, "runtime": "tui"}}}],
            "cluster": {"instance_id": NODE}, "runtime": {"herdr_turn_enabled": true},
            "session_hosts": {"herdr": {
                "endpoints": {(KEY): {"execution_node": NODE, "socket_path": fx.rig.socket(),
                    "herdr_home": "/adk/herdr", "herdr_session": SESSION}},
                "channels": {(CHANNEL.to_string()): KEY}}},
            "tui_o": {"writer": {"channels": [CHANNEL]}, "turn": {"all_owned": false}}
        }))
        .unwrap();
        let path = fx.cwd.path().join("i8114-entry-fixture.yml");
        std::fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
        let configured = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &path);
        let routing = Guard::set_value_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            std::ffi::OsStr::new("disabled"),
        );
        let tmux = fx.cwd.path().join("tmux");
        std::fs::write(
            &tmux,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/i8114-tmux.calls\"\necho 'fixture has no tmux host' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
        let no_tmux = Guard::prepend_path_after_shared_test_env_lock(fx.cwd.path());
        crate::config::session_hosts::install(&config).unwrap();
        crate::config_live_reload::install(config.clone());
        crate::services::provider_hosting::install_provider_hosting_config(&config);
        crate::services::tui_o::channel_policy::install(&config).unwrap();
        let boot = BootChannels::validate(&config)
            .unwrap()
            .adopted(Adoption::Pending);
        let worker_boot = boot.clone();
        let on_thread = fx.rig.thread_installer(NODE, KEY, fx.rig.socket(), SESSION);
        let log_root = crate::services::tui_prompt_dedupe::binding_events::test_root();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .on_thread_start(move || {
                on_thread();
                crate::services::tui_prompt_dedupe::binding_events::set_test_root(
                    log_root.as_deref(),
                );
                WORKER_GUARDS.with(|guards| {
                    guards.borrow_mut().extend([
                        Box::new(test_override::force_boot(worker_boot.clone()))
                            as Box<dyn std::any::Any>,
                        Box::new(open_admission()),
                    ]);
                });
            })
            .on_thread_stop(|| WORKER_GUARDS.with(|guards| guards.borrow_mut().clear()))
            .build()
            .unwrap();
        let boot_guard = test_override::force_boot(boot);
        let (shared, mailbox, hosts, server, api) = runtime.block_on(async {
            let api = discord::admin_host_guard::tests::Recorder::start().await;
            discord::internal_api::init(api.port, Some(fx.pool.clone()));
            let mock = discord_mock::DiscordMockState::new();
            mock.allow_channel(CHANNEL);
            mock.name_channel(CHANNEL, label);
            mock.park_first_placeholder.store(false, Ordering::SeqCst);
            let (proxy, gateway, server) = discord_mock::start(mock).await;
            let ctx = discord_mock::serenity_context(proxy, gateway).await;
            let mut shared =
                discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
            let own = Arc::get_mut(&mut shared).unwrap();
            own.provider = ProviderKind::Claude;
            own.token_hash = TOKEN.into();
            own.api_port = api.port;
            shared.settings.write().await.provider = ProviderKind::Claude;
            shared.restart.reconcile_done.store(true, Ordering::Release);
            shared.http.cached_serenity_ctx.set(ctx).unwrap();
            shared
                .http
                .cached_bot_token
                .set("busy-entry-token".into())
                .unwrap();
            let channel = ChannelId::new(CHANNEL);
            discord::host_defer_gate::tests::map_channel(&shared, channel, label).await;
            {
                let mut core = shared.core.lock().await;
                let session = core.sessions.get_mut(&channel).unwrap();
                session.current_path = Some(fx.cwd.path().display().to_string());
                session.session_id = Some(started.session.clone());
            }
            let canonical_key = discord::adk_session::build_adk_session_key(
                &shared, channel, &ProviderKind::Claude, None,
            ).await.expect("the mapped channel has its actual session key");
            let changed = sqlx::query(
                "UPDATE sessions SET session_key=$1 WHERE provider='claude' AND channel_id=$2 AND discord_token_hash=$3",
            ).bind(&canonical_key).bind(CHANNEL.to_string()).bind(TOKEN)
                .execute(&fx.pool).await.unwrap();
            assert_eq!(changed.rows_affected(), 1, "the canonical key names the launched fixture row");
            let source = source_id_for(&started.session, &started.path).unwrap();
            let io = TestHost::new([(CHANNEL, source)]);
            *io.legacy.lock().unwrap() = Some(Arc::new(DeliveredLegacy {
                logical: fx.logical().into(),
                path: started.path.clone(),
                end: std::fs::metadata(&started.path).unwrap().len(),
            }));
            io.sessions
                .lock()
                .unwrap()
                .insert(CHANNEL, fx.logical().into());
            let gate = Arc::new(OwnershipGate::default());
            let alarms = io.alarms.clone();
            let parts = HostParts {
                io,
                runtime_root: Some(crate::config::runtime_root().unwrap()),
                gate: gate.clone(),
                readiness: host::process_readiness(),
            };
            let hosts = host::start_managed(
                ShadowProvider::Claude,
                true,
                cutover::boot_ownership(),
                || parts,
            );
            assert_eq!(hosts.len(), 1, "the fixture starts a real O writer");
            gate.acquired();
            tokio::time::timeout(LIMIT, async {
                while !host::channel_accepts(CHANNEL) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the O writer resumes its source: {:?}",
                    alarms.0.lock().unwrap()
                )
            });
            assert_eq!(discord::adk_session::turn_session_key(&shared, channel, &ProviderKind::Claude).await,
                Some(canonical_key.clone()));
            let host = crate::services::turn_host::for_turn(Some(&fx.pool), &ProviderKind::Claude,
                CHANNEL, Some(&canonical_key)).await;
            assert!(matches!(host, crate::services::turn_host::TurnHost::Herdr(ref plan)
                if plan.row.as_ref().is_some_and(|row| matches!(&row.record,
                    HostedRecord::Known(record) if record.state == HostedState::Bound
                        && record.owner.logical_key == fx.logical()))),
                "the real turn judge resolves the canonical row as the same Bound Herdr pane");
            assert_idle_source(fx, started, &shared, &host);
            let mailbox = shared.mailbox(channel);
            let pending =
                discord::queue_persistence_context(&shared, &ProviderKind::Claude, channel);
            mailbox.v2_ensure_channel(&pending, channel).await.unwrap();
            assert_eq!(mailbox.has_active_turn().await, Ok(false));
            (shared, mailbox, hosts, server, api)
        });
        Self {
            shared,
            mailbox,
            runtime,
            hosts,
            server,
            _api: api,
            _config: configured,
            _routing: routing,
            _tmux: no_tmux,
            _boot: boot_guard,
        }
    }

    async fn front(&self) -> OwnerFront {
        let original: PendingQueueItem = serde_json::from_value(json!({
            "author_id":7, "message_id":MESSAGE, "channel_id":CHANNEL,
            "source_message_ids":[MESSAGE], "queued_generation":1, "text":INPUT
        }))
        .unwrap();
        assert!(matches!(
            admit(&self.mailbox, &original, AdmissionTicket::new()).await,
            OwnerAdmission::Accepted(_)
        ));
        self.mailbox
            .v2_request(|slot| {
                let actor = slot.as_mut().ok_or("no owner")?;
                let front = actor.front().ok_or("no front")?;
                actor.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap()
    }

    async fn install_holder(&self, fx: &Fixture, started: &Started) {
        assert!(host::channel_accepts(CHANNEL));
        assert_eq!(self.mailbox.has_active_turn().await, Ok(false));
        let token = Arc::new(CancelToken::new());
        self.mailbox
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(MESSAGE - 1))
            .await;
        let mut row = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            CHANNEL,
            Some(
                discord::adk_session::turn_session_key(
                    &self.shared,
                    ChannelId::new(CHANNEL),
                    &ProviderKind::Claude,
                )
                .await
                .expect("the holder uses the actual canonical key"),
            ),
            7,
            MESSAGE - 1,
            0,
            "the already running turn".into(),
            Some(started.session.clone()),
            Some(fx.logical().into()),
            Some(started.path.display().to_string()),
            None,
            0,
        );
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        inflight::save_inflight_state(&row).unwrap();
        assert_eq!(self.mailbox.has_active_turn().await, Ok(true));
    }
}

impl Drop for Intake {
    fn drop(&mut self) {
        let hosts = std::mem::take(&mut self.hosts);
        self.runtime.block_on(async {
            for handle in hosts {
                tokio::time::timeout(LIMIT, handle.stop_and_join())
                    .await
                    .unwrap()
                    .unwrap();
            }
        });
        self.server.abort();
    }
}

#[test]
fn a_managed_herdr_busy_input_observes_early_once_without_a_late_read_pg() {
    let fx = Fixture::new("busy-early-entry", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    assert_eq!(before, 2);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let intake = Intake::new(&fx, &started);
    append(
        &started.path,
        &[
            json!({"type":"user","sessionId":started.session,"message":{"role":"user","content":"keep working"}}),
            json!({"type":"assistant","sessionId":started.session,"message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}),
        ],
    );
    let screen = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
    ));
    fx.rig.answer(
        "pane.read",
        json!({"type":"pane_read", "read": {
            "pane_id":PANE, "source":"recent_unwrapped", "text":screen.replace("Baked", "Thinking"),
            "revision":2, "truncated":false
        }}),
    );
    intake.runtime.block_on(async {
        let front = intake.front().await;
        let (entered, resume) =
            crate::services::discord::router::intake_dispatch::after_upload_take(ChannelId::new(
                CHANNEL,
            ))
            .await
            .pause();
        let start = super::super::start_idle_for_test(
            intake.shared.clone(),
            ChannelId::new(CHANNEL),
            front,
        );
        let holder = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            intake.install_holder(&fx, &started).await;
            resume.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(tokio::time::timeout(LIMIT, start), holder);
        assert_eq!(
            crate::services::discord::router::message_handler::provider_dispatch::OwnerTurn::probes(
                CHANNEL
            ),
            (1, 0),
            "the actual early intake decides before tmux diagnostics; no late rescue"
        );
        assert_eq!(result.unwrap(), Ok(()));
        let sends = fx.rig.sends();
        assert_eq!(sends.len() - before, 2, "{sends:?}");
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
        assert!(
            crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&started.path)
                .is_busy()
        );
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(true));
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert_eq!(state["items"], json!([]), "{state}");
        assert!(state["head_injection"].is_null(), "{state}");
        assert_eq!(state["retained_inputs"], json!([]), "{state}");
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        let tmux = crate::config::runtime_root()
            .unwrap()
            .join("i8114-tmux.calls");
        assert!(
            std::fs::read_to_string(tmux).unwrap_or_default().is_empty(),
            "early intake queried tmux"
        );
    });
}

#[path = "provider_dispatch_herdr_busy_late_tests.rs"]
mod late_tests;

#[test]
fn an_early_new_herdr_execution_cannot_borrow_its_old_global_pin_pg() {
    let fx = Fixture::new("busy-entry-new-nonce-old-pin", None);
    let launcher = Arc::new(Launcher::default());
    let (old_nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let intake = Intake::new(&fx, &started);
    append(
        &started.path,
        &[
            json!({"type":"user","sessionId":started.session,"message":{"role":"user","content":"keep working"}}),
            json!({"type":"assistant","sessionId":started.session,"message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}),
        ],
    );
    let screen = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
    ));
    fx.rig.answer(
        "pane.read",
        json!({"type":"pane_read", "read": {
            "pane_id":PANE, "source":"recent_unwrapped", "text":screen.replace("Baked", "Thinking"),
            "revision":2, "truncated":false
        }}),
    );
    let HostedRecord::Known(old_record) = fx.record() else {
        panic!("a known Bound source")
    };
    let old_pin =
        crate::services::tui_prompt_dedupe::binding_events::pinned_source(CHANNEL, fx.logical())
            .unwrap()
            .expect("the launched source has a verified global pin");
    intake.runtime.block_on(async {
        let counts = super::super::OwnerTurn::probes(CHANNEL);
        let front = intake.front().await;
        let original = front.1.clone();
        let (entered, resume) =
            crate::services::discord::router::intake_dispatch::after_upload_take(ChannelId::new(
                CHANNEL,
            ))
            .await
            .pause();
        let start = super::super::start_idle_for_test(
            intake.shared.clone(),
            ChannelId::new(CHANNEL),
            front,
        );
        let refuse = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            intake.install_holder(&fx, &started).await;
            {
                use crate::services::tui_prompt_dedupe::binding_context::{
                    PreparedIncarnation, execution_context,
                };
                let nonce = uuid::Uuid::new_v4().simple().to_string();
                let mut context = execution_context("claude", &old_nonce).unwrap();
                context.execution_nonce = nonce.clone();
                context.created_at = chrono::Utc::now();
                let context = PreparedIncarnation::create(context).unwrap();
                std::fs::write(
                    crate::services::tmux_common::session_temp_path(fx.logical(), "spawn_nonce"),
                    &nonce,
                )
                .unwrap();
                fx.rig.run_provider(&context.path, false);
                let mut record = old_record.clone();
                record.execution_nonce = nonce.clone();
                record.source_ref.execution_nonce = nonce.clone();
                record.source_ref.initial_source = None;
                record.expected = Some(fx.rig.expected(&nonce));
                sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE channel_id=$2")
                    .bind(serde_json::to_value(&record).unwrap())
                    .bind(CHANNEL.to_string())
                    .execute(&fx.pool)
                    .await
                    .unwrap();
                assert!(
                    crate::services::session_host::herdr_endpoints()
                        .target(&record)
                        .unwrap()
                        .execution_alive(),
                    "the new Bound pane itself is valid; only its old source pin is inadmissible"
                );
                let (pin, history) =
                    crate::services::tui_prompt_dedupe::binding_events::claude_history(
                        CHANNEL,
                        fx.logical(),
                        Some(&nonce),
                    )
                    .unwrap();
                assert_eq!(
                    pin,
                    Some(old_pin.clone()),
                    "the tempting global pin is still present"
                );
                assert!(
                    history.current.is_none(),
                    "the new execution has no current source"
                );
                assert!(
                    host::channel_accepts(CHANNEL),
                    "observer ownership itself still holds"
                );
            }
            resume.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(tokio::time::timeout(LIMIT, start), refuse);
        let after = super::super::OwnerTurn::probes(CHANNEL);
        assert_eq!(
            (after.0 - counts.0, after.1 - counts.1),
            (1, 0),
            "actual early refusal, no late rescue"
        );
        assert!(
            result
                .unwrap()
                .is_err_and(|error| error.starts_with("NotSent("))
        );
        assert_eq!(
            fx.rig.sends().len(),
            before,
            "neither refusal may key anything"
        );
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert_eq!(state["items"], json!([original]), "{state}");
        assert!(state["head_injection"].is_null(), "{state}");
        assert_eq!(state["retained_inputs"], json!([]), "{state}");
        let ops = intake.shared.turn_view_reconciler.ops();
        assert!(
            ops.iter().any(|op| op.add
                && op.emoji == '📬'
                && op.target.channel_id.get() == CHANNEL
                && op.target.message_id.get() == MESSAGE),
            "the actual canonical original owns the queue marker: {ops:?}"
        );
        let path = discord::runtime_store::discord_turn_view_reconciler_root()
            .unwrap()
            .join("intake_user_message")
            .join(format!("{CHANNEL}-{MESSAGE}.json"));
        let view: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(view["applied"], "queued", "{view}");
        assert_eq!(
            view["owner_generation"], original.queued_generation,
            "{view}"
        );
        assert_eq!(
            view["owner_turn_id"],
            format!("discord:{CHANNEL}:{MESSAGE}"),
            "{view}"
        );
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    });
}
