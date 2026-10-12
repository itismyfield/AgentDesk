use super::*;
use crate::services::discord::input_runtime::clear::ClearHost;
use crate::services::discord::input_runtime::fence::{Gate, Mode};
use crate::services::discord::input_runtime::supervisor::mapping::Check;
use crate::services::discord::{DiscordSession, make_shared_data_for_tests_with_storage};
use crate::services::tui_input::rows::RowState;
use crate::services::tui_input::transition::{Move, Outcome};
use axum::{
    Json, Router,
    http::{Method, StatusCode, Uri},
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn shared(channel: u64, pool: Option<sqlx::PgPool>) -> Arc<SharedData> {
    let shared = make_shared_data_for_tests_with_storage(pool);
    {
        let mut settings = shared.settings.write().await;
        settings.provider = ProviderKind::Claude;
        settings.owner_user_id = Some(7);
        settings.allowed_channel_ids = vec![channel];
    }
    shared.core.lock().await.sessions.insert(
        ChannelId::new(channel),
        DiscordSession {
            session_id: Some("g2b-session".into()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel),
            channel_name: Some(format!("g2b-{channel}-cc")),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: shared.restart.current_generation,
        },
    );
    shared
}

async fn ports(shared: Arc<SharedData>, channel: u64, probe: Probe) -> RuntimePorts {
    RuntimePorts::new(
        shared,
        Arc::new(Http::new("test-token")),
        fence::population_root().unwrap(),
        channel,
        probe,
    )
    .await
    .unwrap()
}

struct PolicyDiscord {
    http: Arc<Http>,
    channels: Arc<Mutex<BTreeMap<u64, Option<Value>>>>,
    calls: Arc<Mutex<Vec<(Method, String)>>>,
    server: tokio::task::JoinHandle<()>,
    blocked: Arc<std::sync::atomic::AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl Drop for PolicyDiscord {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl PolicyDiscord {
    async fn new(channel: u64) -> Self {
        let channels = Arc::new(Mutex::new(BTreeMap::<u64, Option<Value>>::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (responses, recorded) = (channels.clone(), calls.clone());
        let blocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (entered, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let (hold, started, resume) = (blocked.clone(), entered.clone(), release.clone());
        let app = Router::new().fallback(move |method: Method, uri: Uri| {
            let (responses, recorded) = (responses.clone(), recorded.clone());
            let (hold, started, resume) = (hold.clone(), started.clone(), resume.clone());
            async move {
                if hold.load(std::sync::atomic::Ordering::Acquire) {
                    started.notify_one();
                    resume.notified().await;
                }
                recorded
                    .lock()
                    .unwrap()
                    .push((method.clone(), uri.path().into()));
                let response = uri.path().rsplit_once("/channels/").and_then(|(_, id)| {
                    let id = id.parse::<u64>().ok()?;
                    responses.lock().unwrap().get(&id).cloned()
                });
                if method == Method::GET
                    && let Some(Some(channel)) = response
                {
                    return Json(channel).into_response();
                }
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"code":50013,"message":"fixture channel unavailable"})),
                )
                    .into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            poise::serenity_prelude::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let fixture = Self {
            http,
            channels,
            calls,
            server,
            blocked,
            entered,
            release,
        };
        fixture.channel(channel, 0, None, &format!("g2b-{channel}-cc"));
        fixture
    }

    fn channel(&self, channel: u64, kind: u8, parent: Option<u64>, name: &str) {
        self.channels.lock().unwrap().insert(
            channel,
            Some(json!({
                "id":channel.to_string(), "type":kind, "name":name, "guild_id":"42000",
                "position":0, "permission_overwrites":[], "nsfw":false,
                "parent_id":parent.map(|id| id.to_string()),
                "thread_metadata":{"archived":false, "auto_archive_duration":60,
                    "archive_timestamp":"2026-10-12T00:00:00Z", "locked":false}
            })),
        );
    }

    fn fail(&self, channel: u64) {
        self.channels.lock().unwrap().insert(channel, None);
    }

    async fn port(&self, shared: Arc<SharedData>, channel: u64) -> RuntimePorts {
        RuntimePorts::new(
            shared,
            self.http.clone(),
            fence::population_root().unwrap(),
            channel,
            Probe::default(),
        )
        .await
        .unwrap()
    }

    fn assert_reads_only(&self) {
        let calls = self.calls.lock().unwrap();
        assert!(
            !calls.is_empty(),
            "the actual Discord HTTP accessor must be exercised"
        );
        assert!(
            calls
                .iter()
                .all(|(method, path)| *method == Method::GET && path.contains("/channels/")),
            "policy lookup cannot post or mutate Discord: {calls:?}"
        );
    }
}

fn policy_source(key: u64, original: u64, execution: u64, input: Value) -> PreparedSource {
    PreparedSource::from_material(
        key,
        crate::services::tui_input::rows::receipt_identity::ReceiptIdentity::new(
            key,
            vec![key],
            7,
            original,
            execution,
        )
        .unwrap(),
        input,
    )
    .unwrap()
}

#[test]
fn c2_runtime_source_policy_reads_current_author_provider_routing_and_holds_unknown_voice() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let config = root.path().join("policy-config.yml");
    std::fs::write(&config, "server: {}\nagents: []\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    runtime().block_on(async {
        let channel = 6_326_980;
        let shared = shared(channel, None).await;
        let discord = PolicyDiscord::new(channel).await;
        let mut port = discord.port(shared.clone(), channel).await;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let prepared = policy_source(
            41,
            channel,
            channel,
            json!({
                "text":"transformed", "original_text":"original", "author_is_bot":false,
                "source_kind":"live_message", "message_kind":"regular",
            }),
        );
        let original = prepared.input().clone();
        let source = Source::from_prepared(prepared.clone()).unwrap();
        let before = source_policy_facts(&shared, &discord.http, &prepared).await;
        assert_eq!(before.authorized, Some(true));
        assert_eq!(before.supported, Some(true));
        assert_eq!(before.disposition, Some(InputDisposition::Prompt));
        assert!(matches!(before.voice_target, VoiceTarget::NotApplicable));
        assert_eq!(intake::classify(&before, &prepared), Classification::Target);
        assert!(port.source_policy(&source).await);

        std::fs::write(&config, "agents: [invalid\n").unwrap();
        let invalid_config = port.policy_facts(&source).await;
        assert!(matches!(invalid_config.voice_target, VoiceTarget::Unknown));
        assert_eq!(
            intake::classify(&invalid_config, &prepared),
            Classification::Retry(intake::Retry::Unknown)
        );
        assert!(!port.source_policy(&source).await);
        std::fs::remove_file(&config).unwrap();
        let missing_config = port.policy_facts(&source).await;
        assert!(matches!(missing_config.voice_target, VoiceTarget::Unknown));
        assert_eq!(
            intake::classify(&missing_config, &prepared),
            Classification::Retry(intake::Retry::Unknown)
        );
        assert!(!port.source_policy(&source).await);
        std::fs::write(&config, "server: {}\nagents: []\n").unwrap();
        assert!(
            port.source_policy(&source).await,
            "fresh readable config restores confirmed absence"
        );

        discord.fail(channel);
        let missing_route = port.policy_facts(&source).await;
        assert_eq!(missing_route.supported, None);
        assert!(matches!(
            missing_route.execution.protection,
            Protection::Unknown
        ));
        assert!(matches!(
            missing_route.voice_target,
            VoiceTarget::NotApplicable
        ));
        assert_eq!(
            intake::classify(&missing_route, &prepared),
            Classification::Retry(intake::Retry::Unknown)
        );
        assert!(!port.source_policy(&source).await);
        discord.channel(channel, 0, None, &format!("g2b-{channel}-cc"));
        assert!(port.source_policy(&source).await);

        {
            let mut current = shared.settings.write().await;
            current.owner_user_id = Some(8);
            current.allow_all_users = false;
            current.allowed_user_ids.clear();
            current.allowed_bot_ids.clear();
        }
        let revoked = port.policy_facts(&source).await;
        assert_eq!(
            revoked.authorized,
            Some(false),
            "resource assembly cannot cache this author's authorization"
        );
        assert_eq!(
            intake::classify(&revoked, &prepared),
            Classification::Rejected(intake::Refusal::Unauthorized)
        );
        assert!(!port.source_policy(&source).await);
        shared.settings.write().await.owner_user_id = Some(7);
        shared.settings.write().await.provider = ProviderKind::Codex;
        let rebound = port.policy_facts(&source).await;
        assert_eq!(rebound.provider, Some(ProviderKind::Codex));
        assert_eq!(rebound.supported, Some(false));
        assert!(!port.source_policy(&source).await);
        shared.settings.write().await.provider = ProviderKind::Claude;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&ChannelId::new(channel))
            .unwrap()
            .channel_name = Some("changed-cdx".into());
        assert_eq!(port.policy_facts(&source).await.supported, Some(false));
        assert!(!port.source_policy(&source).await);
        shared
            .core
            .lock()
            .await
            .sessions
            .remove(&ChannelId::new(channel));
        let missing_session = port.policy_facts(&source).await;
        assert_eq!(missing_session.supported, None);
        assert_eq!(
            intake::classify(&missing_session, &prepared),
            Classification::Retry(intake::Retry::Unknown)
        );
        assert!(!port.source_policy(&source).await);
        assert_eq!(source.input(), &original);
        assert_eq!(prepared.input(), &original);
        assert!(!port.root.join("input_ledger").exists());
        assert!(!port.root.join("discord_pending_queue").exists());
        discord.assert_reads_only();
        drop(gate);
    });
}

#[test]
fn c2b_fresh_thread_parent_route_blocks_changed_foreign_target_and_http_errors() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let config = root.path().join("policy-config.yml");
    std::fs::write(&config, "server: {}\nagents: []\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    runtime().block_on(async {
        let parent = 6_326_981;
        let thread = 6_326_982;
        let foreign = 6_326_983;
        let shared = shared(parent, None).await;
        // These stale hints must not substitute for the current Discord parent response.
        shared.dispatch.thread_parents.insert(ChannelId::new(parent), ChannelId::new(thread));
        let discord = PolicyDiscord::new(parent).await;
        discord.channel(thread, 11, Some(parent), "thread-cdx");
        discord.channel(foreign, 0, None, "foreign-cdx");
        let mut port = discord.port(shared.clone(), parent).await;
        let own_gate = Gate::protect(ProviderKind::Claude, parent).unwrap();
        let foreign_gate = Gate::protect(ProviderKind::Codex, foreign).unwrap();
        let prepared = policy_source(42, thread, parent, json!({
            "text":"rendered thread request", "original_text":"thread request", "author_is_bot":false,
            "source_kind":"live_message", "message_kind":"inline_reply",
        }));
        let original = prepared.input().clone();
        let source = Source::from_prepared(prepared.clone()).unwrap();
        let before = source_policy_facts(&shared, &discord.http, &prepared).await;
        assert_eq!(before.execution.channel_id, parent);
        assert_eq!(before.supported, Some(true), "thread suffix cannot override the effective parent");
        assert!(matches!(before.voice_target, VoiceTarget::NotApplicable));
        assert_eq!(intake::classify(&before, &prepared), Classification::Target);
        assert!(port.source_policy(&source).await);

        discord.channel(thread, 11, Some(foreign), "thread-cdx");
        let changed = source_policy_facts(&shared, &discord.http, &prepared).await;
        assert_eq!(changed.execution.channel_id, foreign);
        assert_eq!(changed.execution.protection, Protection::Protected(ProviderKind::Codex));
        assert_eq!(changed.supported, Some(false));
        assert_eq!(intake::classify(&changed, &prepared), Classification::Rejected(intake::Refusal::Provenance));
        assert!(!port.source_policy(&source).await);
        assert_eq!(shared.dispatch.thread_parents.get(&ChannelId::new(parent)).map(|entry| *entry.value()), Some(ChannelId::new(thread)));

        discord.channel(thread, 11, Some(parent), "thread-cdx");
        discord.fail(parent);
        let failed_parent = source_policy_facts(&shared, &discord.http, &prepared).await;
        assert_eq!(failed_parent.execution.channel_id, parent);
        assert_eq!(failed_parent.execution.protection, Protection::Unknown);
        assert_eq!(failed_parent.supported, None);
        assert_eq!(intake::classify(&failed_parent, &prepared), Classification::Retry(intake::Retry::Unknown));
        assert!(!port.source_policy(&source).await);
        discord.channel(parent, 0, None, &format!("g2b-{parent}-cc"));
        discord.fail(thread);
        let failed_thread = source_policy_facts(&shared, &discord.http, &prepared).await;
        assert_eq!(failed_thread.execution.protection, Protection::Unknown);
        assert_eq!(intake::classify(&failed_thread, &prepared), Classification::Retry(intake::Retry::Unknown));
        assert!(!port.source_policy(&source).await);
        discord.channel(thread, 11, Some(parent), "thread-cdx");
        assert!(port.source_policy(&source).await);
        assert_eq!(source.input(), &original);
        assert_eq!(prepared.input(), &original);
        assert!(!port.root.join("input_ledger").exists());
        assert!(!port.root.join("discord_pending_queue").exists());
        discord.assert_reads_only();
        drop((own_gate, foreign_gate));
    });
}

#[test]
fn c2b_source_disposition_uses_live_commands_skill_origins_and_missing_kind() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let config = root.path().join("policy-config.yml");
    std::fs::write(&config, "server: {}\nagents: []\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    runtime().block_on(async {
        let channel = 6_326_984;
        let shared = shared(channel, None).await;
        shared.known_slash_commands.set(["health".to_owned(), "skill".to_owned()].into_iter().collect()).unwrap();
        let discord = PolicyDiscord::new(channel).await;
        let mut port = discord.port(shared.clone(), channel).await;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        for (index, (kind, raw, disposition)) in [
            ("text_skill", "!skill", InputDisposition::Unsupported),
            ("slash_skill", "/cc", InputDisposition::Unsupported),
            ("live_message", "!stop", InputDisposition::Terminal),
            ("live_message", "!cc stop", InputDisposition::Terminal),
            ("live_message", "!skill explain", InputDisposition::Terminal),
            ("live_message", "/health", InputDisposition::Terminal),
            ("text_skill", "!cc stop", InputDisposition::Terminal),
            ("text_skill", "!skill explain", InputDisposition::Prompt),
            ("text_skill", "<@1> !cc explain", InputDisposition::Prompt),
            ("slash_skill", "/skill explain full original arguments", InputDisposition::Prompt),
            ("slash_skill", "/cc stop", InputDisposition::Terminal),
        ].into_iter().enumerate() {
            let prepared = policy_source(43 + index as u64, channel, channel, json!({
                "text":"rendered skill prompt", "original_text":raw, "author_is_bot":false,
                "source_kind":kind, "message_kind":"regular",
            }));
            let original = prepared.input().clone();
            let source = Source::from_prepared(prepared.clone()).unwrap();
            let facts = source_policy_facts(&shared, &discord.http, &prepared).await;
            assert_eq!(facts.disposition, Some(disposition), "{kind}: {raw}");
            assert!(matches!(facts.voice_target, VoiceTarget::NotApplicable), "{kind}: {raw}");
            assert_eq!(intake::classify(&facts, &prepared), match disposition {
                InputDisposition::Prompt => Classification::Target,
                InputDisposition::Terminal => Classification::NonTarget,
                InputDisposition::Unsupported => Classification::Rejected(intake::Refusal::Unsupported),
            }, "{kind}: {raw}");
            assert_eq!(port.source_policy(&source).await, disposition == InputDisposition::Prompt, "{kind}: {raw}");
            assert_eq!(source.input(), &original);
            assert_eq!(prepared.input()["original_text"], raw);
        }
        for missing in ["source_kind", "message_kind"] {
            let mut input = json!({"text":"rendered", "original_text":"actual request", "author_is_bot":false,
                "source_kind":"live_message", "message_kind":"regular"});
            input.as_object_mut().unwrap().remove(missing);
            let prepared = policy_source(60, channel, channel, input);
            let source = Source::from_prepared(prepared.clone()).unwrap();
            let facts = source_policy_facts(&shared, &discord.http, &prepared).await;
            assert_eq!(facts.disposition, None, "missing {missing} is unreadable, not a prompt");
            assert_eq!(intake::classify(&facts, &prepared), Classification::Retry(intake::Retry::Unknown));
            assert!(!port.source_policy(&source).await);
        }
        assert!(!port.root.join("input_ledger").exists());
        assert!(!port.root.join("discord_pending_queue").exists());
        discord.assert_reads_only();
        drop(gate);
    });
}

#[test]
fn c2b_policy_revocation_during_route_lookup_never_approves_stale_authority() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let config = root.path().join("policy-config.yml");
    std::fs::write(&config, "server: {}\nagents: []\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    runtime().block_on(async {
        let channel = 6_326_985;
        let shared = shared(channel, None).await;
        let discord = PolicyDiscord::new(channel).await;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let source = policy_source(
            46,
            channel,
            channel,
            json!({
                "text":"transformed", "original_text":"original", "author_is_bot":false,
                "source_kind":"live_message", "message_kind":"regular",
            }),
        );
        let original = source.input().clone();
        assert_eq!(
            intake::classify(
                &source_policy_facts(&shared, &discord.http, &source).await,
                &source
            ),
            Classification::Target
        );
        discord
            .blocked
            .store(true, std::sync::atomic::Ordering::Release);
        let mut query = Box::pin(source_policy_facts(&shared, &discord.http, &source));
        let entered = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                () = discord.entered.notified() => {},
                _ = &mut query => panic!("held HTTP lookup returned before release"),
            }
        })
        .await;
        assert!(
            entered.is_ok(),
            "the actual protected policy reaches the held REST lookup"
        );
        shared.settings.write().await.owner_user_id = Some(8);
        discord
            .blocked
            .store(false, std::sync::atomic::Ordering::Release);
        discord.release.notify_one();
        let facts = query.await;
        assert_eq!(
            facts.authorized, None,
            "revocation during IO invalidates the earlier approval"
        );
        assert_eq!(
            intake::classify(&facts, &source),
            Classification::Retry(intake::Retry::Unknown)
        );
        assert_eq!(source.input(), &original);
        assert!(!root.path().join("input_ledger").exists());
        discord.assert_reads_only();
        drop(gate);
    });
}

#[test]
fn b_runtime_ports_reject_unresolved_resources_without_effects() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let channel = 6_326_910;
        let shared = shared(channel, None).await;
        let population = fence::population_root().unwrap();
        let http = Arc::new(Http::new("test-token"));
        assert!(
            RuntimePorts::new(
                shared.clone(),
                http.clone(),
                population.join("foreign"),
                channel,
                Probe::default()
            )
            .await
            .is_err()
        );
        shared.settings.write().await.provider = ProviderKind::Codex;
        assert!(
            RuntimePorts::new(
                shared.clone(),
                http.clone(),
                population.clone(),
                channel,
                Probe::default()
            )
            .await
            .is_err()
        );
        shared.settings.write().await.provider = ProviderKind::Claude;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&ChannelId::new(channel))
            .unwrap()
            .channel_id = Some(channel + 1);
        assert!(
            RuntimePorts::new(shared, http, population.clone(), channel, Probe::default())
                .await
                .is_err()
        );
        assert!(!population.join("input_ledger").exists());
        assert!(!population.join("discord_pending_queue").exists());
    });
}

#[test]
fn b_adopt_requires_all_provider_mapping_views_ports() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let channel = 6_326_911;
        let selected = shared(channel, None).await;
        let mut foreign = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut foreign).unwrap().provider = ProviderKind::Codex;
        let probe = Probe::default();
        let port = ports(selected.clone(), channel, probe.clone()).await;
        assert_eq!(port.mapping().check(channel), Check::Pending);
        assert!(probe.adopt(
            &[ProviderKind::Claude, ProviderKind::Codex],
            vec![selected, foreign.clone()]
        ));
        for reverse in [false, true] {
            let (parent, thread) = if reverse {
                (channel + 1, channel)
            } else {
                (channel, channel + 1)
            };
            foreign
                .dispatch
                .thread_parents
                .insert(ChannelId::new(parent), ChannelId::new(thread));
            let check = port.mapping().check(channel);
            assert!(
                matches!(check, Check::Mapped(_)),
                "actual foreign runtime map must be read"
            );
            let Check::Mapped(edge) = check else {
                unreachable!()
            };
            assert_eq!(
                (edge.writer.as_str(), edge.parent, edge.thread),
                ("codex", parent, thread)
            );
            foreign.dispatch.thread_parents.clear();
        }
        let partial = Probe::default();
        assert!(partial.adopt(
            &[ProviderKind::Claude, ProviderKind::Codex],
            vec![port.shared.clone()]
        ));
        let partial_port = ports(port.shared.clone(), channel, partial).await;
        assert_eq!(partial_port.mapping().check(channel), Check::Unavailable);
        assert_eq!(port.mapping().check(channel), Check::Empty);
    });
}

#[test]
fn b_runtime_ports_sources_are_read_only_and_unreadable_is_not_empty() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let channel = 6_326_914;
        let selected = shared(channel, None).await;
        let population = fence::population_root().unwrap();
        let path = crate::services::discord::inflight::inflight_state_path(
            &crate::services::discord::inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            channel,
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"unreadable inflight").unwrap();
        assert!(
            RuntimePorts::new(
                selected.clone(),
                Arc::new(Http::new("test-token")),
                population.clone(),
                channel,
                Probe::default()
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"unreadable inflight");
        let mut row = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel,
            Some(format!("g2b-{channel}-cc")),
            7,
            63_258_141,
            0,
            "active sources".into(),
            Some("g2b-session".into()),
            None,
            None,
            None,
            0,
        );
        row.source_message_ids = vec![63_258_142, 63_258_141, 0];
        let bytes = serde_json::to_vec(&row).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let port = ports(selected.clone(), channel, Probe::default()).await;
        assert_eq!(port.resources.active_sources, [63_258_141, 63_258_142]);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "read-only source probe cannot backfill old rows"
        );
        row.provider = "codex".into();
        std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
        assert!(
            RuntimePorts::new(
                selected,
                Arc::new(Http::new("test-token")),
                population.clone(),
                channel,
                Probe::default()
            )
            .await
            .is_err()
        );
        assert!(!population.join("input_ledger").exists());
    });
}

#[test]
fn b_runtime_ports_freeze_failure_preserves_legacy_and_closing() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let channel = 6_326_912;
        let shared = shared(channel, None).await;
        let mut port = ports(shared.clone(), channel, Probe::default()).await;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let closing = Arc::new(gate.close().unwrap());
        let queue = port
            .root
            .join("discord_pending_queue/claude/test-token-hash")
            .join(format!("{channel}.json"));
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        let original = serde_json::to_vec(
            &json!([{"author_id":7,"message_id":63258121,"text":"preserve","channel_id":channel}]),
        )
        .unwrap();
        std::fs::write(&queue, &original).unwrap();
        let held = shared
            .acquire_session_transition(ChannelId::new(channel))
            .await
            .unwrap();
        assert_eq!(port.freeze(closing.clone()).await, Err(Failure::Busy));
        assert_eq!(gate.mode(), Mode::Closing);
        assert_eq!(std::fs::read(&queue).unwrap(), original);
        assert!(port.transition(closing.clone()).is_err());
        assert!(!port.root.join("input_ledger").exists());
        drop(held);
        assert!(
            port.clear().await.is_none(),
            "missing PG cannot be clear success"
        );
        port.freeze(closing.clone()).await.unwrap();
        assert_eq!(gate.mode(), Mode::Frozen);
        assert!(
            port.transition(closing).is_err(),
            "missing PG cannot become a Move host"
        );
        assert!(queue.exists());
    });
}

#[test]
fn b_runtime_ports_freeze_clear_move_fail_closed_pg() {
    assert_eq!(
        std::env::var("AGENTDESK_REQUIRE_PG").as_deref(),
        Ok("1"),
        "required PG lane must be explicit"
    );
    // Ledger directory creation refuses symlink ancestors, including macOS /var and /tmp.
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent.canonicalize().unwrap()).unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create("adk_g2b_ports", "G2-B production ports").await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let channel = 6_326_913;
        let waiting_channel = 6_326_915;
        let waiting_shared = shared(waiting_channel, Some(pool.clone())).await;
        let registry = Box::leak(Box::new(crate::services::discord::input_runtime::supervisor::Registry::new()));
        let reserved = registry.reserve_for_test(&ProviderKind::Claude, waiting_channel, &fence::population_root().unwrap()).unwrap();
        let runtime = crate::services::discord::input_runtime::runtime::Runtime::new(
            reserved,
            crate::services::discord::input_runtime::supervisor::Config {
                provider: ProviderKind::Claude, channel: waiting_channel,
                root: fence::population_root().unwrap(),
                request: crate::services::discord::input_runtime::supervisor::Request::Ledger,
                refusal: None,
                binding: Arc::new(crate::services::tui_o::writer::binding::ChannelBindingLog::new(waiting_channel, crate::services::tui_o::shadow::ShadowProvider::Claude)),
            },
            waiting_shared.clone(), Arc::new(Http::new("test-token")),
            vec![ProviderKind::Claude, ProviderKind::Codex],
        ).await.unwrap();
        assert!(registry.health_reasons().iter().any(|reason| reason.contains("runtime_view_pending")));
        let (views, handoff) = tokio::sync::oneshot::channel();
        let (commands, mailbox) = tokio::sync::mpsc::channel(4);
        let waiting = tokio::spawn(runtime.run(handoff, mailbox));
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "actual runtime retains reservation while final views are pending");
        assert!(!fence::population_root().unwrap().join("input_ledger").exists(), "actual runtime cannot open a WAL before final views");
        assert_eq!(fence::lookup(&ProviderKind::Claude, waiting_channel).unwrap().mode(), Mode::Closing);
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        drop(views);
        drop(commands);
        drop(waiting_shared);
        let shared = shared(channel, Some(pool.clone())).await;
        let mut port = ports(shared.clone(), channel, Probe::default()).await;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let closing = Arc::new(gate.close().unwrap());
        let queue = port.root.join("discord_pending_queue/claude/test-token-hash").join(format!("{channel}.json"));
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        std::fs::write(&queue, serde_json::to_vec(&json!([{"author_id":7,"message_id":63258131,"text":"move actual port","channel_id":channel}])).unwrap()).unwrap();
        port.freeze(closing.clone()).await.unwrap();
        assert_eq!(gate.mode(), Mode::Frozen);
        let lock = shared.acquire_session_transition(ChannelId::new(channel)).await.unwrap();
        drop(lock);
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(&shared.token_hash, &shared.provider, &port.resources.tmux);
        crate::services::discord::inflight::seed_session_row_keyed(&pool, &session_key, channel, None).await;
        let (clear, guard) = port.clear().await.expect("real LedgerClear must locate the seeded PG session");
        assert_eq!(clear.identity().tmux, port.resources.tmux);
        drop(guard);
        drop(clear);
        let mut host = port.transition(closing.clone()).unwrap();
        let population = port.root.clone();
        let (mut host, row) = tokio::task::spawn_blocking(move || fence::blocking(|| {
            let _worker = fence::effect::worker_scope(None);
            let mut lease = crate::services::tui_input::ledger::LedgerLease::new(&population, channel);
            let captured = crate::services::tui_input::transition::Host::collect(&mut host, lease.get().unwrap());
            assert!(captured.is_ok(), "actual production Files::frozen collects with worker and Closing capability: {:?}", captured.as_ref().err());
            assert_eq!(captured.unwrap().len(), 1, "actual production port must capture the queued source");
            let prepared = Move::prepare(&mut lease, &mut host);
            assert!(prepared.is_ok(), "actual production Files::frozen retains Closing capability: {:?}", prepared.as_ref().err());
            let mut movement = prepared.unwrap();
            assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Ledger);
            let row = lease.get().unwrap().rows().unwrap().row(63_258_131).unwrap().clone();
            assert_eq!(row.state, RowState::Received);
            assert!(!queue.exists(), "actual Files retirement executed");
            (host, row)
        })).await.unwrap();
        host.notice(Some(63_258_131), "row_held").unwrap();
        assert_eq!(RuntimePorts::take_notices(&mut host), [(Some(63_258_131), "row_held")]);
        assert!(RuntimePorts::take_notices(&mut host).is_empty());
        shared.settings.write().await.owner_user_id = Some(8);
        assert_eq!(host.enqueue(63_258_131, &row).unwrap(), EnqueueOutcome::Rejected, "handback author is rechecked, never authorized by channel alone");
        shared.settings.write().await.owner_user_id = Some(7);
        let current = shared.settings.write().await;
        assert!(host.enqueue(63_258_131, &row).is_err(), "unreadable auth snapshot holds responsibility");
        drop(current);
        shared.settings.write().await.provider = ProviderKind::Codex;
        assert!(host.enqueue(63_258_131, &row).is_err(), "changed provider settings cannot authorize old handback");
        assert_eq!(gate.mode(), Mode::Frozen);
        drop(host);
        drop(port);
        drop(shared);
        pool.close().await;
        db.drop().await;
    });
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn g1b_off_entrypoint_preserves_legacy_trace_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::router::message_handler::IntakeDeps;
    use crate::services::discord::router::{IntakeOrigin, dispatch_skill_intake};
    use crate::services::tui_input::ledger::OPENS;
    use g1b_off::{CHANNEL, MESSAGE, OWNER, OutboxRow, TEXT, TMUX_LOG, child, resources, seed};
    use poise::serenity_prelude::{MessageId, UserId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    if !child() {
        return;
    }
    let config = serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": {"enabled": true, "instance_id": "g1b-off-gateway"},
    }))
    .unwrap();
    crate::config_live_reload::install(config);
    let setup = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let database = setup.block_on(async {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        seed(&pool).await;
        pool.close().await;
        database
    });
    drop(setup);
    let workers = Arc::new(AtomicUsize::new(0));
    let started = workers.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(move || {
            started.fetch_add(1, Ordering::SeqCst);
        })
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = crate::config::runtime_root().unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&database.database_url)
            .await
            .unwrap();
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let http = Arc::new(Http::new("Bot off-intake-test"));
        let deps = IntakeDeps { http: &http, cache: None, ctx_for_chained_dispatch: None, shared: &shared, token: "Bot off-intake-test" };
        resources(&root, &workers);
        dispatch_skill_intake(
            &deps,
            ProviderKind::Claude,
            ChannelId::new(CHANNEL),
            MessageId::new(MESSAGE),
            UserId::new(6325),
            "off input owner".to_owned(),
            TEXT.to_owned(),
            IntakeOrigin::LiveMessage,
            Vec::new(),
            None,
        ).await.unwrap();
        let rows: Vec<OutboxRow> = sqlx::query_as(
            "SELECT target_instance_id, provider, channel_id, user_msg_id, user_text, status,
             preserve_on_cancel FROM intake_outbox WHERE channel_id = $1 ORDER BY id",
        ).bind(CHANNEL.to_string()).fetch_all(&pool).await.unwrap();
        assert_eq!(rows, vec![(OWNER.to_owned(), "claude".to_owned(), CHANNEL.to_string(), MESSAGE.to_string(), TEXT.to_owned(), "pending".to_owned(), Some(false))]);
        assert!(shared.core.lock().await.sessions.is_empty());
        assert!(shared.mailbox(ChannelId::new(CHANNEL)).snapshot().await.intervention_queue.is_empty());
        resources(&root, &workers);
        assert!(!std::path::PathBuf::from(std::env::var_os(TMUX_LOG).unwrap()).exists());
        println!("G1B_OFF_TRACE={}", serde_json::json!({"processed": 1, "legacy_effects": rows.len(), "outbox": rows, "input_workers": workers.load(Ordering::SeqCst), "ledger_opens": OPENS.with(|opens| opens.get())}));
        pool.close().await;
    });
    drop(runtime);
    let cleanup = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    cleanup.block_on(database.drop());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod g1b_off {
    pub(super) type OutboxRow = (String, String, String, String, String, String, Option<bool>);
    use crate::services::discord::input_runtime::{fence, supervisor};
    use crate::services::provider::ProviderKind;
    use crate::services::tui_input::ledger::{Ledger, OPENS, Presence};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    pub(super) const TEST: &str = "g1b_off_entrypoint_preserves_legacy_trace_pg";
    const CHILD: &str = "ADK_G1B_OFF_CHILD";
    pub(super) const TMUX_LOG: &str = "ADK_G1B_OFF_TMUX_LOG";
    pub(super) const CHANNEL: u64 = 6_325_710;
    pub(super) const MESSAGE: u64 = 6_325_711;
    pub(super) const OWNER: &str = "g1b-off-foreign-owner";
    pub(super) const TEXT: &str = "off intake keeps the existing foreign route";

    pub(super) fn child() -> bool {
        if std::env::var(CHILD).as_deref() == Ok(TEST) {
            return true;
        }
        let sandbox = tempfile::tempdir().unwrap();
        let runtime = sandbox.path().join("runtime");
        let temporary = sandbox.path().join("tmp");
        let bin = sandbox.path().join("bin");
        for path in [&runtime, &temporary, &bin] {
            std::fs::create_dir(path).unwrap();
        }
        let stub = bin.join("tmux");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$ADK_G1B_OFF_TMUX_LOG\"\nexit 97\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        let qualified = format!(
            "{}::{TEST}",
            module_path!()
                .strip_suffix("::g1b_off")
                .unwrap()
                .split_once("::")
                .unwrap()
                .1
        );
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &qualified, "--nocapture"])
            .env(CHILD, TEST)
            .env("AGENTDESK_ROOT_DIR", &runtime)
            .env("AGENTDESK_CONFIG", sandbox.path().join("unused-config"))
            .env("TMPDIR", &temporary)
            .env(TMUX_LOG, sandbox.path().join("tmux-calls"))
            .env("ADK_INTAKE_ROUTING_MODE", "enforce")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while process.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                process.kill().unwrap();
                let _ = process.wait();
                panic!("off intake child exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = process.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        assert!(
            stdout
                .lines()
                .any(|line| line.starts_with("G1B_OFF_TRACE="))
        );
        print!("{stdout}");
        false
    }

    pub(super) async fn seed(pool: &sqlx::PgPool) {
        let channel = CHANNEL.to_string();
        let agent = format!("g1b-off-{CHANNEL}");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id,
             preferred_intake_node_labels) VALUES ($1, 'Off input', 'claude', $2, '[]'::jsonb)",
        )
        .bind(&agent)
        .bind(&channel)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO worker_nodes (instance_id, status, role, effective_role,
             labels, capabilities, last_heartbeat_at, started_at, updated_at)
             VALUES ($1, 'online', 'worker', 'worker', '[]'::jsonb,
             '{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"],
             \"features\":[\"preserve_on_cancel_v1\"]}}'::jsonb, NOW(), NOW(), NOW())",
        )
        .bind(OWNER)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (session_key, agent_id, provider, channel_id,
             instance_id, status, last_heartbeat)
             VALUES ($1, $2, 'claude', $3, $4, 'idle', NOW())",
        )
        .bind(format!("claude-{CHANNEL}"))
        .bind(&agent)
        .bind(&channel)
        .bind(OWNER)
        .execute(pool)
        .await
        .unwrap();
    }

    pub(super) fn resources(root: &std::path::Path, workers: &AtomicUsize) {
        assert!(!supervisor::REGISTRY.used());
        assert_eq!(supervisor::REGISTRY.population_for_test(), (0, 0));
        assert_eq!(supervisor::ordering::handback_count_for_test(), 0);
        assert_eq!(supervisor::ordering::handback_callback_count_for_test(), 0);
        assert!(fence::test_health::population_unused());
        assert!(fence::lookup(&ProviderKind::Claude, CHANNEL).is_none());
        assert!(!supervisor::ordering::order_barrier(
            &ProviderKind::Claude,
            CHANNEL
        ));
        assert_eq!(OPENS.with(|opens| opens.get()), 0);
        assert_eq!(workers.load(Ordering::SeqCst), 0);
        assert_eq!(Ledger::probe(root, CHANNEL), Presence::Absent);
        assert!(std::fs::read_dir(root).unwrap().next().is_none());
        assert!(fence::effect::current().is_none());
        assert!(fence::require_worker().is_err());
    }
}
