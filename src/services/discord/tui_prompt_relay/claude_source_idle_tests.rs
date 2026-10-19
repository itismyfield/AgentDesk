//! Discovery through the idle entry while the real O host keeps reading its original source.
use super::*;
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tmux_common as tc;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::store::{OStore, StoreConfig, rotation::Boundary};
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::{self, HostIo, HostParts, Readiness, test_io::*};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::*, binding_events};

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

// Reuse the gateway fixture, but let the running host read the production binding log.
struct LoggedHost(Arc<TestHost>);
impl HostIo for LoggedHost {
    type Port = Posts;
    type Lease = AnyLease;
    type Alarms = Alarms;
    type Bindings = ChannelBindingLog;
    async fn port(&self) -> Arc<Posts> {
        self.0.port().await
    }
    fn lease(&self) -> AnyLease {
        self.0.lease()
    }
    fn alarms(&self) -> Alarms {
        self.0.alarms()
    }
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        Arc::new(ChannelBindingLog::new(channel, provider))
    }
    async fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Result<crate::services::tui_o::writer::activation::ActivationFacts, String> {
        self.0.activation_facts(channel, provider).await
    }
    fn local_custody(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Result<host::Custody, String> {
        self.0.local_custody(channel, provider)
    }
    fn legacy(&self) -> Arc<dyn crate::services::tui_o::writer::adoption::LegacyView> {
        self.0.legacy()
    }
    async fn legacy_busy(&self, channel: u64) -> bool {
        self.0.legacy_busy(channel).await
    }
    fn relaying(&self, channel: u64) -> bool {
        self.0.relaying(channel)
    }
}

struct Pane {
    tmux: String,
    channel: ChannelId,
    a: String,
    b: String,
    path: PathBuf,
    shared: Arc<SharedData>,
    _home: crate::config::TestEnvVarGuard,
}

fn modified(path: &Path, seconds_ago: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(seconds_ago))
        .unwrap();
}

fn record(session: &str, key: &str, at: &str, body: &str, sdk: bool) -> String {
    let entrypoint = if sdk { "sdk-cli" } else { "cli" };
    format!(
        "{}\n",
        serde_json::json!({"type":"assistant", "sessionId":session, "uuid":key,
        "timestamp":at, "entrypoint":entrypoint, "apiBlockIndex":0, "message":{"id":key, "role":"assistant",
        "content":[{"type":"text", "text":body}], "stop_reason":"end_turn"}})
    )
}

impl Pane {
    fn new(root: &Path, channel: u64) -> Self {
        Self::with_tmux(root, channel, format!("scope-source-{channel}"))
    }
    fn with_tmux(root: &Path, channel: u64, tmux: String) -> Self {
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
        std::fs::write(
            &path,
            record(&a, "a1", "2026-10-01T01:00:00Z", "A_BODY", false),
        )
        .unwrap();
        modified(&path, 1200);
        let binding = super::super::launch_script::claude_tui_rehydrated_binding(&a, &path);
        dedupe::register_tmux_channel(&tmux, channel);
        assert!(dedupe::register_rehydrated_tmux_runtime_binding(
            "claude", &tmux, channel, binding
        ));
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
    fn b_path(&self) -> PathBuf {
        self.path.with_file_name(format!("{}.jsonl", self.b))
    }
    fn row(&self) -> super::super::super::inflight::InflightTurnState {
        super::super::super::inflight::InflightTurnState::new(
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
    fn replace(&self) {
        std::fs::write(
            self.b_path(),
            record(&self.b, "b1", "2026-10-01T02:00:00Z", "B_BODY", false),
        )
        .unwrap();
    }
    fn grow(&self) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.b_path())
            .unwrap()
            .write_all(
                record(&self.b, "b2", "2026-10-01T02:01:00Z", "B_NEW_BODY", false).as_bytes(),
            )
            .unwrap();
    }
    async fn idle(&self) {
        relay_idle_claude_bindings(&self.shared).await;
    }
}

async fn polls() {
    tokio::time::sleep(Duration::from_secs(4)).await;
}

#[test]
fn legacy_inflight_discovery_preserves_the_first_prompt_until_finalize() {
    use crate::services::discord::{inflight, shared_state::test_rest, turn_finalizer::*};
    use axum::{
        body::Bytes,
        http::{Method, StatusCode, Uri},
        response::IntoResponse,
    };
    use std::io::Write;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dedupe::reset_state_for_tests();
    let _bindings = BindingRoot::at(root.path());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .finish();
    let _trace = tracing::subscriber::set_default(subscriber);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
        let mut pane = Pane::with_tmux(root.path(), 7_574,
            ProviderKind::Claude.build_tmux_session_name("scope-cut2-1a-7574"));
        let _tmux = FakeTmux::install(&pane.tmux);
        std::fs::write(tc::session_temp_path(&pane.tmux, "generation"), b"1").unwrap();
        let _selected = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        assert!(!crate::services::tui_o::turn_mode::transcript_turns(pane.channel.get()));
        let requests = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
        let recorded = requests.clone();
        let channel = pane.channel.get();
        let next = Arc::new(std::sync::atomic::AtomicU64::new(7_574_100));
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
            let (requests, next) = (recorded.clone(), next.clone());
            async move {
                let path = uri.path().to_string();
                requests.lock().unwrap().push((method.to_string(), path.clone(), String::from_utf8_lossy(&body).into_owned()));
                if method == Method::DELETE || method == Method::PUT || path.ends_with("/typing") {
                    return StatusCode::NO_CONTENT.into_response();
                }
                let id = path.rsplit('/').next().and_then(|s| s.parse::<u64>().ok())
                    .filter(|_| method != Method::POST).unwrap_or_else(|| next.fetch_add(1, Ordering::SeqCst));
                axum::Json(serde_json::json!({"id":id.to_string(), "channel_id":channel.to_string(), "content":"",
                    "author":{"id":"1", "username":"bot", "discriminator":"0001", "avatar":null},
                    "timestamp":"2026-10-15T00:00:00+00:00", "edited_timestamp":null, "tts":false,
                    "mention_everyone":false, "mentions":[], "mention_roles":[], "attachments":[],
                    "embeds":[], "pinned":false, "type":0})).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap())).ratelimiter_disabled(true).build();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Arc::new(http);
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        Arc::get_mut(&mut pane.shared).unwrap().health_registry = Arc::downgrade(&registry);
        registry.set_utility_bot_http_for_tests(crate::services::discord::bot_role::UtilityBotRole::Notify, http.clone()).await;
        let _http = test_rest::install(http);
        let relay_shared = pane.shared.clone();
        super::super::hook_observer::spawn_tui_prompt_relay_observer(
            "claude".into(), subscribe_hook_events(), move |prompt| {
                let shared = relay_shared.clone();
                Box::pin(async move { super::super::relay_observed_prompt(&shared, prompt).await })
            });
        let posts = || requests.lock().unwrap().iter().filter(|(method, path, body)|
            method == "POST" && path.ends_with("/messages") && (body.contains("message_reference") || body.contains("B_NEW_BODY"))).count();
        let delivered = || requests.lock().unwrap().iter().any(|(method, _, body)|
            matches!(method.as_str(), "POST" | "PATCH") && body.contains("B_NEW_BODY"));
        let actor = Arc::new(CancelToken::new());
        pane.shared.mailbox(pane.channel).restore_active_turn(actor, serenity::UserId::new(1), MessageId::new(991)).await;
        let row = pane.row();
        inflight::save_inflight_state(&row).unwrap();
        let old_prompt = serde_json::json!({"type":"user", "sessionId":pane.b, "uuid":"b-old-user",
            "entrypoint":"cli", "message":{"role":"user", "content":"previous B prompt"}});
        let new_prompt = serde_json::json!({"type":"user", "sessionId":pane.b, "uuid":"b-new-user",
            "entrypoint":"cli", "message":{"role":"user", "content":"new B prompt"}});
        let history = format!("{old_prompt}\n{}", record(&pane.b, "b-old-answer", "2026-10-01T02:00:00Z", "B_OLD_BODY", false));
        std::fs::write(pane.b_path(), format!("{history}{new_prompt}\n")).unwrap();
        pane.idle().await;
        pane.grow();
        writeln!(std::fs::OpenOptions::new().append(true).open(pane.b_path()).unwrap(),
            "{}", serde_json::json!({"type":"system", "subtype":"stop_hook_summary", "session_id":pane.b})).unwrap();
        pane.idle().await;
        pane.idle().await;
        let replacement = source_id_for(&pane.b, &pane.b_path()).unwrap();
        assert_eq!(binding_events::binding_events_since(channel, 0).unwrap().iter()
            .filter(|event| event.new == binding_events::BindingTarget::Source(replacement.clone())).count(), 1);
        let retained = inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel).unwrap();
        assert_eq!((retained.output_path, retained.last_offset), (row.output_path.clone(), row.last_offset));
        assert_eq!(posts(), 0, "B is registered but A still owns the body");
        let finalized = Box::pin(pane.shared.turn_finalizer.submit_terminal(
            TurnKey::new(pane.channel, row.effective_finalizer_turn_id(), pane.shared.restart.current_generation),
            ProviderKind::Claude, TerminalEvent::Complete,
            FinalizeContext { clear_inflight: true, ..FinalizeContext::watcher() }, pane.shared.clone())).await;
        assert!(matches!(finalized, FinalizeOutcome::Finalized { .. }), "A must finalize through the production actor");
        assert!(inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel).is_none());
        pane.idle().await;
        let completed = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                if posts() == 1 && delivered() && inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel).is_none() { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await;
        assert!(completed.is_ok(), "after A finalize B must POST exactly once: {:?}", requests.lock().unwrap());
        assert_eq!(posts(), 1);
        assert!(delivered(), "the response card must carry B's actual body");
        for _ in 0..3 { pane.idle().await; }
        assert_eq!(posts(), 1, "later idle ticks cannot replay B");
        assert!(!requests.lock().unwrap().iter().any(|(_, _, body)| body.contains("B_OLD_BODY")));
        _tmux.mark_missing();
        tokio::time::timeout(Duration::from_secs(10), async {
            while CLAUDE_IDLE_RESPONSE_TAILS.lock().unwrap().contains(&pane.tmux) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("the fixture's remaining reader must stop when its fake pane disappears");
        server.abort();
        let _ = server.await;
        dedupe::reset_state_for_tests();
    });
}

#[test]
fn inflight_idle_discovers_a_growing_replacement_and_keeps_its_boundary_pending() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dedupe::reset_state_for_tests();
    let _bindings = BindingRoot::at(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::pause();
            let pane = Pane::new(root.path(), 7_571);
            assert_eq!(
                binding_events::binding_events_since(pane.channel.get(), 0)
                    .unwrap()
                    .len(),
                1,
                "the production registration must publish A before the host starts"
            );
            let _tmux = FakeTmux::install(&pane.tmux);
            // The host initializes the empty A itself, then reads A before B exists.
            std::fs::write(&pane.path, b"").unwrap();
            let _selected = crate::services::tui_o::cutover::test_override::force_candidates(&[(
                pane.channel.get(),
                RuntimeHandoffKind::ClaudeTui,
            )]);
            let source = source_id_for(&pane.a, &pane.path).unwrap();
            let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
                .unwrap()
                .unwrap();
            let io = TestHost::new([]);
            let gate = Arc::new(OwnershipGate::default());
            gate.acquired();
            let ready = Arc::new(Readiness::default());
            let hosts = host::start(ShadowProvider::Claude, true, || HostParts {
                io: Arc::new(LoggedHost(io.clone())),
                runtime_root: Some(root.path().into()),
                gate,
                readiness: ready.clone(),
            });
            assert_eq!(hosts.len(), 1, "the real host must start A's channel");
            polls().await;
            assert!(
                ready.is_ready(pane.channel.get()),
                "O host is already reading A: {:?}",
                io.alarms.0.lock().unwrap()
            );
            std::fs::write(
                &pane.path,
                record(&pane.a, "a1", "2026-10-01T01:00:00Z", "A_BODY", false),
            )
            .unwrap();
            modified(&pane.path, 1200);
            polls().await;
            assert_eq!(io.posts.to(pane.channel.get()), ["A_BODY"]);
            let era = store.read_era().unwrap().unwrap();
            let before = store
                .open_channel(&era, pane.channel.get())
                .unwrap()
                .unwrap()
                .cursor(&source)
                .cloned();
            let binding_before = dedupe::runtime_binding_for_tmux_session(&pane.tmux).unwrap();
            let row = pane.row();
            super::super::super::inflight::save_inflight_state(&row).unwrap();
            pane.replace();
            pane.idle().await;
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(&pane.tmux)
                    .unwrap()
                    .output_path,
                binding_before.output_path,
                "one sample does not prove growth"
            );
            pane.grow();
            // Overlapping callers share the existing source authority and append only one event.
            tokio::join!(pane.idle(), pane.idle());
            let replacement = source_id_for(&pane.b, &pane.b_path()).unwrap();
            let events = binding_events::binding_events_since(pane.channel.get(), 0).unwrap();
            let new: Vec<_> = events
                .iter()
                .filter(|event| {
                    event.new == binding_events::BindingTarget::Source(replacement.clone())
                })
                .collect();
            assert_eq!(new.len(), 1, "inflight idle must publish B without a hook");
            let bound = dedupe::runtime_binding_for_tmux_session(&pane.tmux).unwrap();
            assert_eq!(bound.output_path, pane.b_path().display().to_string());
            assert_eq!(
                bound.last_offset,
                std::fs::metadata(pane.b_path()).unwrap().len(),
                "registration preserves the existing EOF cursor policy for B"
            );
            polls().await;
            let channel = store
                .open_channel(&era, pane.channel.get())
                .unwrap()
                .unwrap();
            assert_eq!(
                channel.cursor(&source).cloned(),
                before,
                "discovery cannot move A's body cursor"
            );
            let link = channel
                .rotation()
                .unwrap()
                .links
                .into_values()
                .find(|link| link.source == replacement)
                .unwrap();
            assert!(
                matches!(link.boundary, Boundary::Pending { .. }),
                "B waits for its start proof: {:?}",
                link.boundary
            );
            assert_eq!(new[0].cause, binding_events::BindingCause::Unknown);
            assert_eq!(
                io.posts.to(pane.channel.get()),
                ["A_BODY"],
                "B has zero POSTs"
            );
            let retained = super::super::super::inflight::load_inflight_state_read_only(
                &ProviderKind::Claude,
                pane.channel.get(),
            )
            .unwrap();
            assert_eq!(
                (retained.output_path, retained.last_offset),
                (row.output_path, row.last_offset)
            );
            for task in hosts {
                task.abort();
                let _ = task.await;
            }
            dedupe::reset_state_for_tests();
        });
}

#[test]
fn inflight_idle_does_not_register_another_panes_or_sdk_source() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dedupe::reset_state_for_tests();
    let _bindings = BindingRoot::at(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let pane = Pane::new(root.path(), 7_572);
            let _tmux = FakeTmux::install(&pane.tmux);
            super::super::super::inflight::save_inflight_state(&pane.row()).unwrap();
            pane.replace();
            let peer = "source-peer";
            dedupe::register_tmux_runtime_binding(
                peer,
                super::super::launch_script::claude_tui_rehydrated_binding(&pane.b, &pane.b_path()),
            );
            let sdk = uuid::Uuid::new_v4().to_string();
            let sdk_path = pane.path.with_file_name(format!("{sdk}.jsonl"));
            std::fs::write(
                &sdk_path,
                record(&sdk, "sdk", "2026-10-01T03:00:00Z", "SDK_BODY", true),
            )
            .unwrap();
            let before = binding_events::binding_events_since(pane.channel.get(), 0).unwrap();
            pane.idle().await;
            pane.grow();
            pane.idle().await;
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(&pane.tmux)
                    .unwrap()
                    .output_path,
                pane.path.display().to_string()
            );
            assert_eq!(
                binding_events::binding_events_since(pane.channel.get(), 0).unwrap(),
                before
            );
            dedupe::reset_state_for_tests();
        });
}

#[test]
fn inflight_idle_resumes_a_retained_row_from_its_original_source_after_discovery() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dedupe::reset_state_for_tests();
    let _bindings = BindingRoot::at(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let pane = Pane::new(root.path(), 7_573);
            let _tmux = FakeTmux::install(&pane.tmux);
            let _selected = crate::services::tui_o::cutover::test_override::force_channels(&[]);
            let mut row = pane.row();
            super::super::super::inflight::save_inflight_state(&row).unwrap();
            pane.replace();
            pane.idle().await;
            pane.grow();
            pane.idle().await;
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(&pane.tmux)
                    .unwrap()
                    .output_path,
                pane.b_path().display().to_string()
            );
            row.turn_source = TurnSource::ExternalInput;
            row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
            row.current_msg_id = 0;
            row.request_owner_user_id = TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
            row.injected_prompt_message_id = Some(row.user_msg_id);
            row.relay_owner_kind = RelayOwnerKind::None;
            row.external_turn_id = Some("retained-source-A".into());
            row.turn_nonce = Some("retained-source-nonce".into());
            row.turn_start_offset = Some(17);
            // Round-trip a row lacking optional newer receipt fields, as a restart restores it.
            let mut legacy = serde_json::to_value(&row).unwrap();
            legacy
                .as_object_mut()
                .unwrap()
                .remove("last_watcher_relayed_offset");
            let row = serde_json::from_value(legacy).unwrap();
            super::super::super::inflight::save_inflight_state(&row).unwrap();
            assert!(
                super::super::synthetic_start::bridge_handoff::resume_unpublished(
                    &pane.shared,
                    &row,
                    &pane.b_path()
                )
                .await
                .is_none(),
                "exact validation refuses B with A's offset"
            );
            pane.idle().await;
            let snapshot =
                crate::services::discord::mailbox_snapshot(&pane.shared, pane.channel).await;
            assert!(
                snapshot.cancel_token.is_some(),
                "actual idle recovery must re-admit A despite binding B"
            );
            let saved = super::super::super::inflight::load_inflight_state_read_only(
                &ProviderKind::Claude,
                pane.channel.get(),
            )
            .unwrap();
            assert_eq!(
                (
                    saved.output_path,
                    saved.turn_start_offset,
                    saved.last_offset
                ),
                (row.output_path, Some(17), 17)
            );
            snapshot
                .cancel_token
                .unwrap()
                .cancelled
                .store(true, Ordering::Release);
            CLAUDE_IDLE_RESPONSE_TAILS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&pane.tmux);
            dedupe::reset_state_for_tests();
        });
}
