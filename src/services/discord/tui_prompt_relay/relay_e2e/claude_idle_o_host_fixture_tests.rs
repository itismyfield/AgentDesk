//! Isolated idle discovery, production HTTP transport and durable writer fixture.
use crate::services::discord::SharedData;
use crate::services::discord::outbound::o_writer_io::GatewayPort;
use crate::services::discord::tui_prompt_relay::relay_e2e::{AbortOnDrop, discord_mock};
use crate::services::provider::ProviderKind;
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tmux_common as tc;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, binding_reader::source_id_for};
use crate::services::tui_o::store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::WriterAlarm;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::deliver::{ChannelWriter, Step};
use crate::services::tui_o::writer::host::{self, HostIo, test_io::*};
use crate::services::tui_o::writer::pieces::{Derived, UnitDeriver};
use crate::services::tui_o::writer::rotation::Sources;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::*, binding_events};
use poise::serenity_prelude::{self as serenity, ChannelId};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub(crate) type Writer = ChannelWriter<GatewayPort, AnyLease, Alarms>;
struct BindingRoot;
impl BindingRoot {
    fn at(root: &Path) -> Self {
        binding_events::set_test_root(Some(root));
        Self
    }
}
impl Drop for BindingRoot {
    fn drop(&mut self) {
        binding_events::set_test_root(None);
    }
}

pub(crate) struct Pane {
    pub(crate) tmux: String,
    pub(crate) channel: ChannelId,
    pub(crate) a: String,
    pub(crate) b: String,
    pub(crate) path: PathBuf,
    pub(crate) shared: Arc<SharedData>,
    _home: crate::config::TestEnvVarGuard,
}

pub(crate) fn modified(path: &Path, seconds_ago: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(seconds_ago))
        .unwrap();
}

pub(crate) fn record(session: &str, key: &str, at: &str, body: &str, sdk: bool) -> String {
    let entrypoint = if sdk { "sdk-cli" } else { "cli" };
    format!(
        "{}\n",
        serde_json::json!({"type":"assistant", "sessionId":session, "uuid":key,
        "timestamp":at, "entrypoint":entrypoint, "apiBlockIndex":0, "message":{"id":key, "role":"assistant",
        "content":[{"type":"text", "text":body}], "stop_reason":"end_turn"}})
    )
}

impl Pane {
    pub(crate) fn new(root: &Path, channel: u64) -> Self {
        Self::with_tmux(root, channel, format!("scope-source-{channel}"))
    }
    pub(crate) fn with_tmux(root: &Path, channel: u64, tmux: String) -> Self {
        Self::build(root, channel, tmux, false)
    }
    fn build(root: &Path, channel: u64, tmux: String, pending: bool) -> Self {
        let (a, b) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let home = root.join("claude-home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let home_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CLAUDE_CONFIG_DIR",
            &home,
        );
        let path =
            crate::services::claude_tui::transcript_tail::claude_transcript_path(&cwd, &a, None)
                .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Mirror the host evidence published by the production tmux launch.
        tc::host_marker::record_tmux_host_marker(&tmux);
        assert_eq!(
            tc::host_marker::read_host_kind_marker(&tmux),
            tc::host_marker::HostKindMarker::Known(crate::services::session_host::HostKind::Tmux)
        );
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel),
            owner_runtime_root: root.display().to_string(),
            host: None,
            expected_native_session_id: Some(a.clone()),
            launch_mode: "fresh".into(),
            provider_root: Some(home),
            first_prompt_digest: None,
            source_policy: None,
        };
        let prepared = PreparedIncarnation::create(context).unwrap();
        let script = tc::session_temp_path(&tmux, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
        std::fs::write(
            &script,
            format!(
                "{}cd '{}'\nexec 'claude' '--session-id' '{a}'\n",
                prepared.env_lines(),
                cwd.display()
            ),
        )
        .unwrap();
        std::fs::write(
            tc::session_temp_path(&tmux, "spawn_nonce"),
            &prepared.context.execution_nonce,
        )
        .unwrap();
        std::fs::write(tc::session_temp_path(&tmux, tc::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT),
            serde_json::json!({"hooks":{"SessionStart":[{"hooks":[{"command":format!("adk hook --session-id {a}")}]}]}}).to_string()).unwrap();
        modified(Path::new(&script), 1800);
        if pending {
            let mut binding = crate::services::discord::tui_prompt_relay::launch_script::claude_tui_rehydrated_binding(&a, &path);
            binding.output_path = root.join("not-written.jsonl").display().to_string();
            dedupe::register_tmux_runtime_binding(&tmux, binding);
            dedupe::register_tmux_channel(&tmux, channel);
        } else {
            dedupe::register_tmux_channel(&tmux, channel);
            std::fs::write(
                &path,
                record(&a, "a1", "2026-10-01T01:00:00Z", "A_BODY", false),
            )
            .unwrap();
            modified(&path, 1200);
            let binding =
            crate::services::discord::tui_prompt_relay::launch_script::claude_tui_rehydrated_binding(&a, &path);
            assert!(dedupe::register_rehydrated_tmux_runtime_binding(
                "claude", &tmux, channel, binding
            ));
        }
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(&tmux, ChannelId::new(channel));
        Self {
            tmux,
            channel: ChannelId::new(channel),
            a,
            b,
            path,
            shared,
            _home: home_env,
        }
    }
    pub(crate) fn b_path(&self) -> PathBuf {
        self.path.with_file_name(format!("{}.jsonl", self.b))
    }
    pub(crate) fn row(&self) -> crate::services::discord::inflight::InflightTurnState {
        crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            self.channel.get(),
            None,
            1,
            991,
            992,
            "running A".into(),
            None,
            Some(self.tmux.clone()),
            Some(self.path.display().to_string()),
            None,
            17,
        )
    }
    pub(crate) async fn idle(&self) {
        crate::services::discord::tui_prompt_relay::claude_idle_runtime::relay_idle_claude_bindings(
            &self.shared,
        )
        .await;
    }
}

pub(crate) struct Fixture {
    pub root: PathBuf,
    pub pane: Pane,
    pub store: OStore,
    pub io: Arc<HttpIo>,
    pub gate: Arc<OwnershipGate>,
    pub mock: discord_mock::DiscordMockState,
    _tmux: FakeTmux,
    _server: AbortOnDrop<()>,
    _bindings: BindingRoot,
}
pub(crate) struct HttpIo {
    pub base: Arc<TestHost>,
    port: Arc<GatewayPort>,
}
impl HostIo for HttpIo {
    type Port = GatewayPort;
    type Lease = AnyLease;
    type Alarms = Alarms;
    type Bindings = ChannelBindingLog;
    async fn port(&self) -> Arc<GatewayPort> {
        self.port.clone()
    }
    fn lease(&self) -> AnyLease {
        AnyLease
    }
    fn alarms(&self) -> Alarms {
        self.base.alarms.clone()
    }
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        Arc::new(ChannelBindingLog::new(channel, provider))
    }
    async fn activation_facts(
        &self,
        c: u64,
        p: ShadowProvider,
    ) -> Result<crate::services::tui_o::writer::activation::ActivationFacts, String> {
        self.base.activation_facts(c, p).await
    }
    fn local_custody(&self, c: u64, p: ShadowProvider) -> Result<host::Custody, String> {
        self.base.local_custody(c, p)
    }
    fn legacy(&self) -> Arc<dyn crate::services::tui_o::writer::adoption::LegacyView> {
        self.base.legacy()
    }
    async fn legacy_busy(&self, c: u64) -> bool {
        self.base.legacy_busy(c).await
    }
    fn relaying(&self, c: u64) -> bool {
        self.base.relaying(c)
    }
}
impl Fixture {
    pub async fn new(root: &Path) -> Self {
        Self::with_pending(root, false).await
    }
    async fn with_pending(root: &Path, pending: bool) -> Self {
        let bindings = BindingRoot::at(root);
        let pane = Pane::build(
            root,
            discord_mock::CHANNEL_ID,
            format!("scope-source-{}", discord_mock::CHANNEL_ID),
            pending,
        );
        let tmux = FakeTmux::install(&pane.tmux);
        let mock = discord_mock::DiscordMockState::new();
        mock.park_first_placeholder
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (proxy, _, server) = discord_mock::start(mock.clone()).await;
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(proxy)
                .ratelimiter_disabled(true)
                .build(),
        );
        let port = Arc::new(GatewayPort::new(http, discord_mock::BOT_ID));
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root)
            .unwrap()
            .unwrap();
        Self {
            root: root.into(),
            pane,
            store,
            io: Arc::new(HttpIo {
                base: TestHost::new([]),
                port,
            }),
            gate: Arc::new(OwnershipGate::default()),
            mock,
            _tmux: tmux,
            _server: AbortOnDrop(Some(server)),
            _bindings: bindings,
        }
    }
    pub fn a(&self) -> SourceId {
        source_id_for(&self.pane.a, &self.pane.path).unwrap()
    }
    pub fn b(&self) -> SourceId {
        source_id_for(&self.pane.b, &self.pane.b_path()).unwrap()
    }
    pub fn initialize(&self, start: u64) {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(&self.pane.path).unwrap();
        self.store
            .begin_era(&[self.pane.channel.get()], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![InitSource {
                        source_id: self.a(),
                        delivery_start: start,
                        prefix_hash: hex::encode(Sha256::digest(&bytes[..start as usize])),
                    }],
                    initial_anchor: 100,
                    build_digest: "test".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
    }
    pub fn channel(&self) -> ChannelStore {
        self.store
            .open_channel(
                &self.store.read_era().unwrap().unwrap(),
                self.pane.channel.get(),
            )
            .unwrap()
            .unwrap()
    }
    pub fn posts(&self) -> Vec<String> {
        let all = self.mock.channel_posts.lock().unwrap();
        assert!(all.iter().all(|(c, _)| *c == self.pane.channel.get()));
        all.iter().map(|(_, s)| s.clone()).collect()
    }
    pub fn engine(&self) -> Engine {
        let writer = ChannelWriter::new(
            self.channel(),
            self.gate.clone(),
            self.io.port.clone(),
            AnyLease,
            self.io.alarms(),
        );
        let sources = Sources::new(
            self.pane.channel.get(),
            ShadowProvider::Claude,
            self.io
                .bindings(self.pane.channel.get(), ShadowProvider::Claude),
        );
        let mut engine = Engine {
            writer,
            sources,
            deriver: UnitDeriver::new(self.pane.channel.get(), ShadowProvider::Claude),
            owed: VecDeque::new(),
        };
        engine
            .sources
            .resume(&mut engine.writer, &mut engine.deriver, &mut engine.owed)
            .unwrap();
        engine
    }
    pub async fn discover(&self, bytes: &str) {
        crate::services::discord::inflight::save_inflight_state(&self.pane.row()).unwrap();
        std::fs::write(self.pane.b_path(), bytes).unwrap();
        modified(&self.pane.path, 1200);
        self.pane.idle().await;
        append(&self.pane.b_path(), "{\"type\":\"mode\"}\n");
        tokio::join!(self.pane.idle(), self.pane.idle());
    }
    pub fn assert_posted(&self, session: &str, bytes: &str) {
        use crate::services::tui_o::shadow::CapturedRecord;
        use crate::services::tui_o::store::ledger::PieceOutcome;
        let channel = self.channel();
        let mut scratch = UnitDeriver::new(self.pane.channel.get(), ShadowProvider::Claude);
        let mut at = 0;
        for line in bytes.split_inclusive('\n') {
            let end = at + line.len() as u64;
            for d in scratch.derive(&CapturedRecord {
                start: at,
                end,
                line: line.as_bytes().to_vec(),
            }) {
                if let Derived::Piece(p) = d {
                    assert!(
                        channel
                            .ledger()
                            .latest_piece(&p.unit_key, p.index)
                            .is_some_and(|(_, r)| matches!(
                                r.outcome,
                                Some(PieceOutcome::Posted(_))
                            )),
                        "{session} {}:{} has durable Posted",
                        p.unit_key.native_key,
                        p.index
                    );
                }
            }
            at = end;
        }
    }
}
pub(crate) struct Engine {
    pub writer: Writer,
    pub sources: Sources<ChannelBindingLog>,
    pub deriver: UnitDeriver,
    pub owed: VecDeque<Derived>,
}
impl Engine {
    pub fn capture(&mut self) -> Result<(), WriterAlarm> {
        self.sources.follow(&mut self.writer)?;
        self.sources
            .capture(&mut self.writer, &mut self.deriver, &mut self.owed)?;
        self.sources.tend(&mut self.writer)
    }
    pub async fn deliver(&mut self, max: usize) {
        for _ in 0..max {
            let Some(item) = self.owed.front() else {
                break;
            };
            if self.writer.deliver(item).await != Step::Done {
                break;
            }
            self.owed.pop_front();
        }
    }
}
pub(crate) fn append(path: &Path, bytes: &str) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes.as_bytes())
        .unwrap();
}
pub(crate) fn run(scenario: impl AsyncFnOnce(Fixture)) {
    run_with_pending(scenario, false);
}
pub(crate) fn run_with_pending(scenario: impl AsyncFnOnce(Fixture), pending: bool) {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dedupe::reset_state_for_tests();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let fixture = if pending {
                Fixture::with_pending(root.path(), true).await
            } else {
                Fixture::new(root.path()).await
            };
            scenario(fixture).await;
        });
    dedupe::reset_state_for_tests();
}
