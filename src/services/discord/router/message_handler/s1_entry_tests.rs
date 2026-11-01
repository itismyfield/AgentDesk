use super::super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{
    self,
    health::{self as turn_deliver, HealthRegistry, HumanInputDelivery, HumanInputRequest},
    turn_presence::activity::tests as activity,
};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::io::Write;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

fn row(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![],
        source_message_queued_generations: vec![],
        source_text_segments: vec![],
        text: "selected driver input".into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: vec![],
    }
}
fn transcript(path: &std::path::Path, large: bool, closed: bool) {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    if large {
        let row = format!(
            "{}\n",
            serde_json::json!({"type":"summary","summary":"x".repeat(8192)})
        );
        let mut len = 0;
        while len <= 256 * 1024 * 1024 {
            file.write_all(row.as_bytes()).unwrap();
            len += row.len();
        }
    }
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"user","uuid":"A","message":{"content":"hello"}})
    )
    .unwrap();
    if closed {
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"system","subtype":"turn_duration"})
        )
        .unwrap();
    }
    file.flush().unwrap();
}
async fn runtime(
    pool: sqlx::PgPool,
    api_port: u16,
    ctx: &serenity::Context,
    channels: &[u64],
    workspace: &std::path::Path,
    provider: &ProviderKind,
) -> (Arc<discord::SharedData>, HealthRegistry) {
    let mut shared = discord::make_shared_data_for_tests_with_storage(Some(pool));
    Arc::get_mut(&mut shared).unwrap().api_port = api_port;
    Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
    shared.http.cached_serenity_ctx.set(ctx.clone()).unwrap();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    {
        let mut settings = shared.settings.write().await;
        settings.provider = provider.clone();
        settings.owner_user_id = Some(100);
        settings.allowed_user_ids = vec![200];
        settings.allowed_channel_ids = channels.to_vec();
    }
    for id in channels {
        let channel = ChannelId::new(*id);
        discord::host_defer_gate::tests::map_channel(&shared, channel, &format!("s1-entry-{id}"))
            .await;
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.current_path = Some(workspace.display().to_string());
        session.session_id = Some("s1-provider-session".into());
    }
    let registry = HealthRegistry::new();
    registry
        .register(provider.as_str().into(), shared.clone())
        .await;
    (shared, registry)
}
fn request(channel: ChannelId) -> HumanInputRequest {
    request_for(channel, ProviderKind::Claude)
}
fn request_for(channel: ChannelId, provider: ProviderKind) -> HumanInputRequest {
    HumanInputRequest {
        channel_id: channel,
        provider,
        text: "human input".into(),
        author_id: 200,
        source: "test".into(),
        metadata: Some(serde_json::json!({"silent":true})),
        channel_name_hint: None,
    }
}
type DriverProbe = (
    tokio::sync::oneshot::Receiver<std::thread::ThreadId>,
    std::sync::mpsc::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
    Arc<AtomicUsize>,
);
fn arm(channel: u64) -> DriverProbe {
    let (entered, receiver) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    assert!(
        provider_dispatch::INPUT_EFFECT_PROBE
            .lock()
            .unwrap()
            .replace(provider_dispatch::InputEffectProbe {
                channel,
                entered,
                release: wait,
                terminal_before_release: false
            })
            .is_none()
    );
    let (completed, completion) = tokio::sync::oneshot::channel();
    assert!(
        discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap()
            .replace((ChannelId::new(channel), completed))
            .is_none()
    );
    let extra = Arc::new(AtomicUsize::new(0));
    let count = extra.clone();
    assert!(
        provider_dispatch::CLAUDE_LEGACY_PROBE
            .lock()
            .unwrap()
            .replace((
                channel,
                Box::new(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err("duplicate selected driver".into())
                })
            ))
            .is_none()
    );
    (receiver, release, completion, extra)
}
async fn finish_driver(
    channel: u64,
    entered: tokio::sync::oneshot::Receiver<std::thread::ThreadId>,
    release: std::sync::mpsc::Sender<()>,
    complete: tokio::sync::oneshot::Receiver<()>,
    extra: Arc<AtomicUsize>,
) {
    let reached = tokio::time::timeout(Duration::from_secs(20), entered).await;
    let selected_io = usize::from(matches!(reached, Ok(Ok(_))));
    eprintln!("S1_SELECTED_DRIVER channel={channel} selected_io={selected_io}");
    assert_eq!(
        selected_io, 1,
        "Started must reach the selected execution boundary"
    );
    // The current-thread runtime runs the caller here, so a distinct id is the blocking worker.
    let worker = reached.unwrap().unwrap() != std::thread::current().id();
    assert!(
        worker,
        "the actual spawn_blocking provider worker did not execute"
    );
    eprintln!(
        "S1_SELECTED_DRIVER channel={channel} io=1 extra=0 worker={} skip=0",
        worker
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), complete)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        extra.load(Ordering::SeqCst),
        0,
        "more than one selected driver call"
    );
    provider_dispatch::CLAUDE_LEGACY_PROBE
        .lock()
        .unwrap()
        .take();
}

#[tokio::test(flavor = "current_thread")]
async fn s1_large_human_start_requires_selected_execution_pg() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::session_strategy_lifecycle_tests::s1_entry_tests::",
        "s1_large_human_start_requires_selected_execution_pg"
    )) {
        return;
    }
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _injection = injection_off();
    let root = tempfile::tempdir().unwrap();
    let _namespace_root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        root.path(),
    );
    let _binding = activity::BindingRoot::enter(root.path());
    let config = serde_json::from_value(serde_json::json!({"server":{},"agents":[]})).unwrap();
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let api = discord::admin_host_guard::tests::Recorder::start().await;
    discord::internal_api::init(api.port, None);
    let mock = discord_mock::DiscordMockState::new();
    for id in [69_184_001, 69_184_002] {
        mock.allow_channel(id);
    }
    let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let ids = [69_184_001, 69_184_002];
    let (shared, registry) = runtime(
        pool.clone(),
        api.port,
        &ctx,
        &ids,
        root.path(),
        &ProviderKind::Claude,
    )
    .await;
    let provider = ProviderKind::Claude;
    for (index, closed) in [true, false].into_iter().enumerate() {
        let channel = ChannelId::new(ids[index]);
        let path = root.path().join(format!("{}.jsonl", channel));
        transcript(&path, true, closed);
        let name = provider.build_tmux_session_name(&format!("s1-entry-{channel}"));
        let _turn =
            activity::bind_turn_mode_transcript(&shared, &provider, channel.get(), &name, &path);
        let _ = activity::settled_reason(&shared, &provider, channel.get()).await;
        let (mut entered, release, completion, extra) = arm(channel.get());
        let result = turn_deliver::deliver_human_input(&registry, request(channel))
            .await
            .unwrap();
        let snapshot = discord::mailbox_snapshot(&shared, channel).await;
        if closed {
            assert!(
                matches!(result, HumanInputDelivery::Started { .. }),
                "{result:?}"
            );
            assert!(
                snapshot.intervention_queue.is_empty(),
                "Idle was queued as activity unknown"
            );
            finish_driver(channel.get(), entered, release, completion, extra).await;
        } else {
            assert!(
                matches!(result,HumanInputDelivery::Queued{ref reason,..} if reason==turn_deliver::ExternalHold::Busy.reason() || reason==turn_deliver::ExternalHold::Unknown.reason()),
                "{result:?}"
            );
            assert!(
                entered.try_recv().is_err(),
                "held source executed a provider"
            );
            assert_eq!(extra.load(Ordering::SeqCst), 0);
            assert_eq!(snapshot.intervention_queue.len(), 1);
            provider_dispatch::INPUT_EFFECT_PROBE.lock().unwrap().take();
            discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
                .lock()
                .unwrap()
                .take();
            provider_dispatch::CLAUDE_LEGACY_PROBE
                .lock()
                .unwrap()
                .take();
            drop(release);
            drop(completion);
        }
        shared.mailboxes.remove_fixture_for_test(channel);
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s1_kickoff_and_backstop_reach_selected_execution_pg() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::session_strategy_lifecycle_tests::s1_entry_tests::",
        "s1_kickoff_and_backstop_reach_selected_execution_pg"
    )) {
        return;
    }
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _injection = injection_off();
    let root = tempfile::tempdir().unwrap();
    let _namespace_root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        root.path(),
    );
    let _binding = activity::BindingRoot::enter(root.path());
    let config = serde_json::from_value(serde_json::json!({"server":{},"agents":[]})).unwrap();
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let api = discord::admin_host_guard::tests::Recorder::start().await;
    discord::internal_api::init(api.port, None);
    let mock = discord_mock::DiscordMockState::new();
    for id in [69_184_101, 69_184_102, 69_184_103, 69_184_104] {
        mock.allow_channel(id);
    }
    let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let _primed = prime_placeholder(&mock, &ctx, 69_184_101).await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let ids = [69_184_101, 69_184_102, 69_184_103, 69_184_104];
    let (shared, _registry) = runtime(
        pool.clone(),
        api.port,
        &ctx,
        &ids,
        root.path(),
        &ProviderKind::Claude,
    )
    .await;
    let provider = ProviderKind::Claude;
    let mut turns = Vec::new();
    for (index, (closed, backstop)) in [(true, false), (false, false), (true, true), (false, true)]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(ids[index]);
        let _limits = activity::small_tail_limits(channel.get());
        let path = root.path().join(format!("{channel}.jsonl"));
        transcript(&path, false, closed);
        let name = provider.build_tmux_session_name(&format!("s1-entry-{channel}"));
        turns.push(activity::bind_turn_mode_transcript(
            &shared,
            &provider,
            channel.get(),
            &name,
            &path,
        ));
        let _ = activity::settled_reason(&shared, &provider, channel.get()).await;
        let context = discord::queue_persistence_context(&shared, &provider, channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![row(channel.get() + 1_000_000)], context)
            .await;
        let (mut entered, release, completion, extra) = arm(channel.get());
        if backstop {
            let armed = discord::queue_io::arm_slow_idle_queue_backstop_if_queue_nonempty(
                &shared, &provider, channel, "s1-test",
            )
            .await;
            assert!(armed, "actual slow backstop was not armed");
            tokio::time::pause();
            tokio::task::yield_now().await;
            tokio::time::advance(
                discord::queue_io::idle_queue_backstop_delay_for_tests() + Duration::from_millis(1),
            )
            .await;
            tokio::time::resume();
        } else {
            let deps = IntakeDeps {
                http: &ctx.http,
                cache: Some(&ctx.cache),
                ctx_for_chained_dispatch: Some(&ctx),
                shared: &shared,
                token: "test-token",
            };
            let result = discord::queue_dispatch::kickoff::kickoff_idle_queue_channel(
                &deps, &provider, channel,
            )
            .await;
            assert_eq!(result.started, closed);
        }
        if closed {
            finish_driver(channel.get(), entered, release, completion, extra).await;
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(entered.try_recv().is_err());
            assert_eq!(extra.load(Ordering::SeqCst), 0);
            assert_eq!(
                discord::mailbox_snapshot(&shared, channel)
                    .await
                    .intervention_queue[0]
                    .message_id
                    .get(),
                channel.get() + 1_000_000
            );
            provider_dispatch::INPUT_EFFECT_PROBE.lock().unwrap().take();
            discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
                .lock()
                .unwrap()
                .take();
            provider_dispatch::CLAUDE_LEGACY_PROBE
                .lock()
                .unwrap()
                .take();
            drop(release);
            drop(completion);
        }
        shared.mailboxes.remove_fixture_for_test(channel);
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s1_real_missing_namespace_reaches_selected_tmux_start_pg() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::session_strategy_lifecycle_tests::s1_entry_tests::",
        "s1_real_missing_namespace_reaches_selected_tmux_start_pg"
    )) {
        return;
    }
    use crate::services::platform::tmux::session_inventory::{
        self as inventory, EmptyReason, Inventory,
        tests::{self as inventory_tests, ChannelNamespace, ChannelProbeTrace},
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _injection = injection_off();
    let root = tempfile::tempdir().unwrap();
    let _binding = activity::BindingRoot::enter(root.path());
    // tmux reports the realpath of TMUX_TMPDIR (macOS /var -> /private/var); match it.
    let _namespace = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        &root.path().canonicalize().unwrap(),
    );
    let api = discord::admin_host_guard::tests::Recorder::start().await;
    discord::internal_api::init(api.port, None);
    let mock = discord_mock::DiscordMockState::new();
    for id in [69_184_201, 69_184_202, 69_184_203, 69_184_204] {
        mock.allow_channel(id);
    }
    let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let ids = [69_184_201, 69_184_202, 69_184_203, 69_184_204];
    let (shared, registry) = runtime(
        pool.clone(),
        api.port,
        &ctx,
        &ids,
        root.path(),
        &ProviderKind::Claude,
    )
    .await;
    let provider = ProviderKind::Claude;
    let agents: Vec<_> = ids.iter().enumerate().map(|(i,id)| serde_json::json!({
        "id":format!("fixture{i}"), "name":format!("fixture{i}"), "provider":"claude",
        "channels":{"claude":{"id":id.to_string(),"name":format!("s1-entry-{id}"),"runtime":"tui"}}
    })).collect();
    let config: crate::config::Config =
        serde_json::from_value(serde_json::json!({"server":{},"agents":agents})).unwrap();
    crate::config_live_reload::install(config.clone());
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let path = crate::runtime_layout::config_file_path(&crate::config::runtime_root().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_yaml::to_string(&config).unwrap()).unwrap();
    for (index, explicit) in [true, false, true, true].into_iter().enumerate() {
        let channel = ChannelId::new(ids[index]);
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .channel_name = None;
        let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(channel.get());
        let socket = root.path().join(format!("explicit-{channel}"));
        let namespace = explicit.then(|| ChannelNamespace::new(channel.get(), socket.clone()));
        let probe = ChannelProbeTrace::new(channel.get());
        let mut unrelated = None;
        if index == 2 {
            drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        } else if index == 3 {
            let other_name = format!("unrelated-{channel}");
            shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(&other_name, ChannelId::new(99_000_001));
            let lifetime = SessionLifetime::new(root.path(), channel.get() + 10_000);
            let output = namespace
                .as_ref()
                .unwrap()
                .start_command()
                .args(["new-session", "-d", "-s", &other_name, &lifetime.command])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            unrelated = Some(lifetime);
        }
        let first = inventory::observe_inventory(channel.get());
        let reported = match &first.inventory {
            Inventory::Empty(EmptyReason::MissingSocket(path)) if index < 2 => {
                assert!(!path.exists());
                path.clone()
            }
            Inventory::Empty(EmptyReason::NoServer(path)) if index == 2 => {
                assert!(path.exists());
                path.clone()
            }
            Inventory::Sessions(names) if index == 3 => {
                assert_eq!(names, &[format!("unrelated-{channel}")]);
                socket.clone()
            }
            _ => panic!("unexpected namespace control: {first:?}"),
        };
        let read =
            discord::turn_presence::activity::activity_now(&shared, &provider, channel).await;
        assert_eq!(
            read.activity,
            discord::turn_presence::activity::Activity::Idle,
            "{read:?}"
        );
        assert!(probe.calls() >= 2);
        assert_eq!(reported.exists(), index >= 2, "probe changed the namespace");
        let mut command = namespace.as_ref().map_or_else(
            inventory_tests::production_start_command,
            ChannelNamespace::start_command,
        );
        inventory_tests::assert_same_start_context(
            &first,
            &command,
            explicit.then_some(socket.clone()),
        );
        let name = provider.build_tmux_session_name(&format!("s1-entry-{channel}"));
        let mut lifetime = SessionLifetime::new(root.path(), channel.get());
        let command_text = lifetime.command.clone();
        let (release_driver, wait_driver) = std::sync::mpsc::channel();
        let expected = name.clone();
        let observed = first.clone();
        let selected_socket = explicit.then(|| reported.clone());
        let selected_path = reported.clone();
        let (sent, ran) = tokio::sync::oneshot::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        *provider_dispatch::CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((
            channel.get(),
            Box::new(move || {
                let actual =
                    discord::inflight::load_inflight_state(&ProviderKind::Claude, channel.get())
                        .expect("selected driver has its actual inflight row");
                assert_eq!(
                    actual.tmux_session_name.as_deref(),
                    Some(expected.as_str()),
                    "observation and actual start chose different targets"
                );
                crate::services::claude_tui::submission_fence::before_first_payload()?;
                inventory_tests::assert_same_start_context(&observed, &command, selected_socket);
                assert_eq!(
                    selected_path.exists(),
                    index >= 2,
                    "probe changed socket before execution"
                );
                command.args(["new-session", "-d", "-s", &expected, &command_text]);
                let output = command.output().map_err(|e| e.to_string())?;
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                calls.fetch_add(1, Ordering::SeqCst);
                let _ = sent.send(());
                let _ = wait_driver.recv_timeout(Duration::from_secs(30));
                Err("selected fixture driver completed".into())
            }),
        ));
        let (complete, completion) = tokio::sync::oneshot::channel();
        *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap() = Some((channel, complete));
        let delivery = turn_deliver::deliver_human_input(&registry, request(channel))
            .await
            .unwrap();
        assert!(
            matches!(delivery, HumanInputDelivery::Started { .. }),
            "{delivery:?}"
        );
        tokio::time::timeout(Duration::from_secs(20), ran)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(reported.exists());
        assert!(
            discord::mailbox_snapshot(&shared, channel)
                .await
                .intervention_queue
                .is_empty()
        );
        lifetime.release();
        release_driver.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(20), completion)
            .await
            .unwrap()
            .unwrap();
        if let Some(other) = &mut unrelated {
            other.release();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(
                inventory::observe_inventory(channel.get()).inventory,
                Inventory::Empty(_)
            ) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("fixture tmux must exit after its short-lived command");
        eprintln!(
            "S1_REAL_START explicit={explicit} name={name} socket={} uid={} io={} skip=0",
            reported.display(),
            unsafe { libc::geteuid() },
            count.load(Ordering::SeqCst)
        );
        tokio::time::timeout(Duration::from_secs(20), async {
            while discord::inflight::load_inflight_state(&provider, channel.get()).is_some()
                || discord::mailbox_snapshot(&shared, channel)
                    .await
                    .cancel_token
                    .is_some()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if reported.exists() {
            use std::os::unix::fs::FileTypeExt;
            assert!(
                std::fs::symlink_metadata(&reported)
                    .unwrap()
                    .file_type()
                    .is_socket()
            );
            std::fs::remove_file(&reported).unwrap();
        }
        std::fs::write(&reported, "ordinary file, not a tmux socket").unwrap();
        let before = probe.calls();
        let failure = inventory::observe_inventory(channel.get());
        let (mut entered, release, completion, extra) = arm(channel.get());
        let held = turn_deliver::deliver_human_input(&registry, request(channel))
            .await
            .unwrap();
        assert!(
            matches!(held, HumanInputDelivery::Queued {ref reason,..} if reason == turn_deliver::ExternalHold::Unknown.reason()),
            "{held:?}"
        );
        assert!(probe.calls() > before);
        assert!(matches!(failure.inventory, Inventory::ProbeFailed(_)));
        assert!(matches!(
            entered.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(extra.load(Ordering::SeqCst), 0);
        assert!(discord::inflight::load_inflight_state(&provider, channel.get()).is_none());
        let snapshot = discord::mailbox_snapshot(&shared, channel).await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert!(reported.is_file(), "probe changed the failing path");
        provider_dispatch::INPUT_EFFECT_PROBE.lock().unwrap().take();
        provider_dispatch::CLAUDE_LEGACY_PROBE
            .lock()
            .unwrap()
            .take();
        discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap()
            .take();
        drop(release);
        drop(completion);
        let persistence = discord::queue_persistence_context(&shared, &provider, channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![], persistence)
            .await;
        std::fs::remove_file(&reported).unwrap();
        eprintln!(
            "S1_REAL_PROBE_FAILURE case={index} selected_io=0 queue=1 probe_calls={} skip=0",
            probe.calls() - before
        );
        shared.mailboxes.remove_fixture_for_test(channel);
    }
    server.abort();
    let _ = server.await;
    pool.close().await;
    db.drop().await;
}

async fn prime_placeholder(
    mock: &discord_mock::DiscordMockState,
    ctx: &serenity::Context,
    id: u64,
) -> tokio::task::JoinHandle<()> {
    let http = ctx.http.clone();
    let task = tokio::spawn(async move {
        let _ = ChannelId::new(id).say(&http, "...").await;
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !mock.written_texts().iter().any(|s| s == "...") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::task::yield_now().await;
    task
}

#[tokio::test(flavor = "current_thread")]
async fn s1_fallback_only_and_restored_owner_only_live_hold_real_effects_pg() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::session_strategy_lifecycle_tests::s1_entry_tests::",
        "s1_fallback_only_and_restored_owner_only_live_hold_real_effects_pg"
    )) {
        return;
    }
    use crate::services::platform::tmux::session_inventory::tests::{
        ChannelNamespace, ChannelProbeTrace,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _injection = injection_off();
    let root = tempfile::tempdir().unwrap();
    let _namespace_root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        root.path(),
    );
    let _binding = activity::BindingRoot::enter(root.path());
    let api = discord::admin_host_guard::tests::Recorder::start().await;
    discord::internal_api::init(api.port, None);
    let mock = discord_mock::DiscordMockState::new();
    let ids = [69_184_301, 69_184_302];
    for id in ids {
        mock.allow_channel(id);
    }
    let (proxy, gateway, server) = discord_mock::start(mock).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (shared, registry) = runtime(
        pool.clone(),
        api.port,
        &ctx,
        &ids,
        root.path(),
        &ProviderKind::Claude,
    )
    .await;
    let mut config:crate::config::Config=serde_yaml::from_str(&format!("server: {{}}\nagents:\n  - id: fixture\n    name: fixture\n    provider: claude\n    channels:\n      claude: {{id: '{}', name: s1-entry-{}, runtime: tui}}\n  - id: fixture2\n    name: fixture2\n    provider: claude\n    channels:\n      claude: {{id: '{}', name: s1-entry-{}, runtime: tui}}\n",ids[0],ids[0],ids[1],ids[1])).unwrap();
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let config_path =
        crate::runtime_layout::config_file_path(&crate::config::runtime_root().unwrap());
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let provider = ProviderKind::Claude;
    for (index, restored) in [false, true].into_iter().enumerate() {
        let channel = ChannelId::new(ids[index]);
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        crate::config_live_reload::install(config.clone());
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .channel_name = None;
        let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(channel.get());
        let socket = root.path().join(format!("live-{channel}"));
        let namespace = ChannelNamespace::new(channel.get(), socket.clone());
        let trace = ChannelProbeTrace::new(channel.get());
        let positive =
            discord::turn_presence::activity::activity_now(&shared, &provider, channel).await;
        assert_eq!(
            positive.activity,
            discord::turn_presence::activity::Activity::Idle,
            "healthy empty namespace control: {positive:?}"
        );
        assert!(trace.calls() > 0);
        let (entered, release, complete, extra) = arm(channel.get());
        let delivered = turn_deliver::deliver_human_input(&registry, request(channel))
            .await
            .unwrap();
        assert!(
            matches!(delivered, HumanInputDelivery::Started { .. }),
            "same-fixture selected execution control: {delivered:?}"
        );
        finish_driver(channel.get(), entered, release, complete, extra).await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if discord::inflight::load_inflight_state(&provider, channel.get()).is_none()
                    && discord::mailbox_snapshot(&shared, channel)
                        .await
                        .cancel_token
                        .is_none()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .channel_name = None;
        if restored {
            let mut binding = config.agents[index].channels.get("claude").unwrap().clone();
            if let crate::config::AgentChannel::Detailed(c) = &mut binding {
                c.name = None;
            }
            config.agents[index].channels.insert("claude", binding);
        }
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        crate::config_live_reload::install(config.clone());
        let absent_again =
            discord::turn_presence::activity::activity_now(&shared, &provider, channel).await;
        assert_eq!(
            absent_again.activity,
            discord::turn_presence::activity::Activity::Idle,
            "positive execution left an unrelated hold in the live-only fixture: {absent_again:?}"
        );
        let live_name = if restored {
            format!("s1-restored-{channel}")
        } else {
            provider.build_tmux_session_name(&format!("s1-entry-{channel}"))
        };
        if restored {
            shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(&live_name, channel);
        }
        let mut lifetime = SessionLifetime::new(root.path(), channel.get());
        let output = namespace
            .start_command()
            .args(["new-session", "-d", "-s", &live_name, &lifetime.command])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(shared.tmux_watchers.channel_binding(&channel).is_none());
        assert!(
            shared
                .core
                .lock()
                .await
                .sessions
                .get(&channel)
                .unwrap()
                .channel_name
                .is_none()
        );
        let before = trace.calls();
        let observed =
            discord::turn_presence::activity::activity_now(&shared, &provider, channel).await;

        assert!(trace.calls() > before);
        let first = channel.get() + 1_000_000;
        let persistence = discord::queue_persistence_context(&shared, &provider, channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![row(first), row(first + 1)], persistence)
            .await;
        let taken =
            discord::mailbox_take_next_automatic_intervention(&shared, &provider, channel).await;
        assert!(taken.intervention.is_none());
        assert_eq!(
            observed.activity,
            discord::turn_presence::activity::Activity::Unknown,
            "restored={restored}: {observed:?}"
        );
        assert_eq!(
            observed.reason, "watcher_unbound",
            "known live ownership was not observed"
        );
        let (mut entered, release, completion, extra) = arm(channel.get());
        let delivery = turn_deliver::deliver_human_input(&registry, request(channel))
            .await
            .unwrap();
        assert!(
            matches!(delivery,HumanInputDelivery::Queued{ref reason,..} if reason==turn_deliver::ExternalHold::Unknown.reason()),
            "{delivery:?}"
        );
        assert!(
            matches!(
                entered.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "a held observation reached selected execution"
        );
        assert_eq!(extra.load(Ordering::SeqCst), 0);
        let snapshot = discord::mailbox_snapshot(&shared, channel).await;
        assert_eq!(
            snapshot
                .intervention_queue
                .iter()
                .take(2)
                .map(|i| i.message_id.get())
                .collect::<Vec<_>>(),
            vec![first, first + 1]
        );
        provider_dispatch::INPUT_EFFECT_PROBE.lock().unwrap().take();
        discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap()
            .take();
        provider_dispatch::CLAUDE_LEGACY_PROBE
            .lock()
            .unwrap()
            .take();
        drop(release);
        drop(completion);
        let persistence = discord::queue_persistence_context(&shared, &provider, channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![], persistence)
            .await;
        lifetime.release();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(
                crate::services::platform::tmux::session_inventory::observe_inventory(
                    channel.get()
                )
                .inventory,
                crate::services::platform::tmux::session_inventory::Inventory::Empty(_)
            ) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let persistence = discord::queue_persistence_context(&shared, &provider, channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![], persistence)
            .await;
        shared.mailboxes.remove_fixture_for_test(channel);
    }
    server.abort();
    let _ = server.await;
    pool.close().await;
    db.drop().await;
}

struct SessionLifetime {
    fifo: std::path::PathBuf,
    command: String,
    released: bool,
}
impl SessionLifetime {
    fn new(root: &std::path::Path, id: u64) -> Self {
        let fifo = root.join(format!("lifetime-{id}.fifo"));
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let read = format!(
            "read adk_fixture_line < {}",
            crate::services::process::shell_escape(fifo.to_str().unwrap())
        );
        let command = format!(
            "/bin/sh -c {}",
            crate::services::process::shell_escape(&read)
        );
        Self {
            fifo,
            command,
            released: false,
        }
    }
    fn release(&mut self) {
        if self.released {
            return;
        }
        use std::os::unix::fs::OpenOptionsExt;
        for _ in 0..300 {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.fifo)
            {
                let _ = file.write_all(b"done\n");
                self.released = true;
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for SessionLifetime {
    fn drop(&mut self) {
        self.release();
    }
}

fn injection_off() -> [crate::config::TestEnvVarGuard; 2] {
    let set = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock;
    [
        set("ADK_BUSY_INJECT", std::ffi::OsStr::new("0")),
        set("ADK_QUEUED_HEAD_INJECT", std::ffi::OsStr::new("0")),
    ]
}

thread_local! {
    static NATIVE_WORKER_GUARDS: std::cell::RefCell<Vec<Box<dyn std::any::Any>>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[test]
fn s1_herdr_first_create_and_retired_use_herdr_pg() {
    use crate::services::session_host::herdr_socket_rig_tests::{self as rig, NativeHerdrRig};
    use std::os::unix::fs::PermissionsExt;
    const TEST: &str = concat!(
        "services::discord::router::message_handler::session_strategy_lifecycle_tests::s1_entry_tests::",
        "s1_herdr_first_create_and_retired_use_herdr_pg"
    );
    if rig::native_server_child() || !discord::admin_host_guard::tests::api_child(TEST) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _injection = injection_off();
    let root = tempfile::tempdir().unwrap();
    // Resolve preparation against a local executable; the Herdr rig owns execution.
    let cli = root.path().join("claude-fixture");
    std::fs::write(&cli, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.0 (Claude Code)'; exit 0; fi\nexit 1\n").unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _cli = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CLAUDE_PATH",
        &cli,
    );
    let _namespace = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        root.path(),
    );
    let _binding = activity::BindingRoot::enter(root.path());
    let _ = tracing_subscriber::fmt().with_env_filter("agentdesk::services::discord=debug,agentdesk::services::turn_host=debug,agentdesk::services::herdr_launch=debug,agentdesk::services::claude::herdr_turn=debug,agentdesk::services::session_host=debug").with_test_writer().try_init();
    let rig = NativeHerdrRig::start(TEST);
    rig.prove_native_off();
    let ids = [
        69_184_501, 69_184_502, 69_184_503, 69_184_504, 69_184_505, 69_184_506, 69_184_507,
        69_184_508, 69_184_509, 69_184_510, 69_184_511, 69_184_512,
    ];
    let agents: Vec<_> = ids.iter().map(|id| serde_json::json!({"id":format!("fixture{id}"),"name":format!("fixture{id}"),"provider":"claude","channels":{"claude":{"id":id.to_string(),"name":format!("s1-entry-{id}"),"runtime":"tui"}}})).collect();
    let channels: serde_json::Map<String, serde_json::Value> = ids
        .iter()
        .map(|id| (id.to_string(), serde_json::json!("native")))
        .collect();
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server":{},"agents":agents,"cluster":{"instance_id":"test-node"},"runtime":{"herdr_turn_enabled":true},
        "session_hosts":{"herdr":{"endpoints":{"native":{"execution_node":"test-node","socket_path":rig.socket(),"herdr_home":rig.home(),"herdr_session":"agentdesk"}},"channels":channels}}
    })).unwrap();
    crate::config::session_hosts::install(&config).unwrap();
    crate::config_live_reload::install(config.clone());
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let config_path =
        crate::runtime_layout::config_file_path(&crate::config::runtime_root().unwrap());
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
    let boot = crate::config::session_hosts::BootSessionHosts::from_config(&config).unwrap();
    let owned: Vec<_> = ids
        .iter()
        .map(|id| {
            (
                *id,
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            )
        })
        .collect();
    let _registry = rig::native_registry_on_this_thread(&boot);
    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&owned);
    let _accepts = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let _intake_ready =
        crate::services::tui_o::cutover::intake_route::test_probe::answer_with(|_| true);
    let (store, era) = crate::services::herdr_launch::o_store_for_test(
        &crate::config::runtime_root().unwrap(),
        &ids,
    );
    for id in ids {
        store
            .open_channel(&era, id)
            .unwrap()
            .unwrap()
            .set_binding_checkpoint(1)
            .unwrap();
    }
    let worker_owned = owned.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(move || {
            NATIVE_WORKER_GUARDS.with(|guards| {
                guards.borrow_mut().extend([
                    Box::new(rig::native_registry_on_this_thread(&boot)) as Box<dyn std::any::Any>,
                    Box::new(
                        crate::services::tui_o::cutover::test_override::force_channels(
                            &worker_owned,
                        ),
                    ),
                    Box::new(crate::services::herdr_launch::force_writer_accepts(Some(
                        true,
                    ))),
                    Box::new(
                        crate::services::tui_o::cutover::intake_route::test_probe::answer_with(
                            |_| true,
                        ),
                    ),
                ])
            });
        })
        .on_thread_stop(|| NATIVE_WORKER_GUARDS.with(|guards| guards.borrow_mut().clear()))
        .build()
        .unwrap();
    rt.block_on(native_herdr_cases(&rig, &root, ids));
}

async fn native_session_api(pool: &sqlx::PgPool) -> (u16, tokio::task::JoinHandle<()>) {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).unwrap();
    let broadcast_tx = crate::eventbus::new_broadcast();
    let state = crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine,
        config: Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: None,
    };
    let app = axum::Router::new()
        .route(
            "/api/dispatched-sessions/webhook",
            axum::routing::post(crate::server::routes::dispatched_sessions::hook_session),
        )
        .with_state(state)
        .fallback(|| async { axum::Json(serde_json::json!({})) });
    let listener = tokio::net::TcpListener::bind((crate::config::loopback().as_str(), 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (port, server)
}

async fn native_herdr_cases(
    rig: &crate::services::session_host::herdr_socket_rig_tests::NativeHerdrRig,
    root: &tempfile::TempDir,
    ids: [u64; 12],
) {
    use crate::db::dispatched_sessions::hosted_execution::{
        self, HostedLocation, HostedOwner, HostedState,
    };
    use crate::services::platform::tmux::session_inventory::tests::ChannelNamespace;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (api_port, api_server) = native_session_api(&pool).await;
    discord::internal_api::init(api_port, Some(pool.clone()));
    let hooks = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _hook_endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://{}",
        hooks.local_addr().unwrap()
    ));
    let hook_server = tokio::spawn(async move {
        axum::serve(
            hooks,
            crate::services::claude_tui::hook_server::hook_receiver_router(),
        )
        .await
        .unwrap();
    });
    let mock = discord_mock::DiscordMockState::new();
    for id in ids {
        mock.allow_channel(id);
    }
    let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let _primed = prime_placeholder(&mock, &ctx, ids[0]).await;
    let provider = ProviderKind::Claude;
    let (mut shared, registry) =
        runtime(pool.clone(), api_port, &ctx, &ids, root.path(), &provider).await;
    drop(registry);
    Arc::get_mut(&mut shared).unwrap().token_hash = "discord_0123456789abcdef".into();
    for (index, id) in ids.into_iter().enumerate() {
        let channel = ChannelId::new(id);
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .session_id = None;
        let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(id);
        let namespace = ChannelNamespace::new(id, root.path().join(format!("tmux-{id}")));
        let name = provider.build_tmux_session_name(&format!("s1-entry-{id}"));
        let retired = index % 2 == 1;
        if retired || (8..10).contains(&index) {
            let owner = HostedOwner {
                provider: "claude".into(),
                discord_token_hash: shared.token_hash.clone(),
                channel_id: id.to_string(),
                logical_key: name.clone(),
                owner_node: "test-node".into(),
                runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
            };
            let state = match index {
                8 => HostedState::Pending,
                9 => HostedState::Bound,
                _ => HostedState::Retired,
            };
            let mut record = hosted_execution::tests::record(&owner, "recorded", state);
            if state != HostedState::Pending {
                record.location = Some(HostedLocation {
                    host: "herdr".into(),
                    execution_node: "test-node".into(),
                    endpoint_config_key: "native".into(),
                    socket_addr: rig.socket().display().to_string(),
                    named_session: "agentdesk".into(),
                    pane_id: "old-pane".into(),
                });
            }
            let key = discord::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &provider,
                &name,
            );
            discord::inflight::seed_session_row_hashed(
                &pool,
                &key,
                id,
                &shared.token_hash,
                Some(serde_json::to_value(record).unwrap()),
            )
            .await;
        }
        let mut lifetime = None;
        if (2..4).contains(&index) {
            let fixture = SessionLifetime::new(root.path(), id);
            let output = namespace
                .start_command()
                .args(["new-session", "-d", "-s", &name, &fixture.command])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            lifetime = Some(fixture);
        } else if (4..6).contains(&index) {
            let live_name = format!("s1-restored-{id}");
            shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(&live_name, channel);
            let fixture = SessionLifetime::new(root.path(), id);
            let output = namespace
                .start_command()
                .args(["new-session", "-d", "-s", &live_name, &fixture.command])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            lifetime = Some(fixture);
        } else if (6..8).contains(&index) {
            use crate::services::tui_prompt_dedupe::binding_events as binding;
            let historical = format!("old-name-{id}");
            let path = root.path().join(format!("old-{id}.jsonl"));
            std::fs::write(&path, "{}\n").unwrap();
            let source =
                crate::services::tui_o::shadow::binding_reader::source_id_for("parent", &path)
                    .unwrap();
            let now = chrono::Utc::now();
            let event = binding::BindingEvent {
                seq: 1,
                channel_id: id,
                provider: "claude".into(),
                tmux_session: historical.clone(),
                execution_nonce: Some("historical".into()),
                old: None,
                new: binding::BindingTarget::Source(source),
                cause: binding::BindingCause::Startup,
                parent_hint: None,
                evidence: binding::BindingEvidence {
                    hook_event: Some("SessionStart".into()),
                    received_at: now,
                },
                committed_at: now,
            };
            let dir = root.path().join(binding::BINDING_EVENTS_DIR);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{id}.log")),
                format!("{}\n", serde_json::to_string(&event).unwrap()),
            )
            .unwrap();
            let marker = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
                &historical,
                "host_kind",
            ));
            std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
            std::fs::write(marker, "process").unwrap();
        } else if index >= 10 {
            let marker = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
                &name,
                "host_kind",
            ));
            std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
            std::fs::write(
                marker,
                if index == 10 {
                    "process"
                } else {
                    "unknown-host"
                },
            )
            .unwrap();
        }
        let context = discord::queue_persistence_context(&shared, &provider, channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![row(id + 1_000_000)], context)
            .await;
        let requests_before = rig.requests().len();
        let before = rig
            .requests()
            .iter()
            .filter(|r| r["method"] == "pane.send_input")
            .count();
        let tmux = Arc::new(AtomicUsize::new(0));
        let count = tmux.clone();
        *provider_dispatch::CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((
            id,
            Box::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
                Err("Herdr incorrectly selected tmux".into())
            }),
        ));
        let (complete, completion) = tokio::sync::oneshot::channel();
        *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap() = Some((channel, complete));
        let deps = IntakeDeps {
            http: &ctx.http,
            cache: Some(&ctx.cache),
            ctx_for_chained_dispatch: Some(&ctx),
            shared: &shared,
            token: "test-token",
        };
        let observed =
            discord::turn_presence::activity::activity_now(&shared, &provider, channel).await;
        let key = discord::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &provider,
            &name,
        );
        let host =
            crate::services::turn_host::for_turn(Some(&pool), &provider, id, Some(&key)).await;
        eprintln!(
            "S1_HERDR_PREFLIGHT case={index} activity={observed:?} host={host:?} readiness_blocked={}",
            hosted_tui_promote_readiness_blocked(&shared, &provider, channel).await
        );
        let result =
            discord::queue_dispatch::kickoff::kickoff_idle_queue_channel(&deps, &provider, channel)
                .await;
        let positive = index < 2;
        assert_eq!(result.started, positive, "case={index} result={result:?}");
        let completed = if positive {
            Some(tokio::time::timeout(Duration::from_secs(40), completion).await)
        } else {
            assert_eq!(
                discord::mailbox_snapshot(&shared, channel)
                    .await
                    .intervention_queue[0]
                    .message_id
                    .get(),
                id + 1_000_000
            );
            discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
                .lock()
                .unwrap()
                .take();
            None
        };
        let sends = rig
            .requests()
            .iter()
            .filter(|r| r["method"] == "pane.send_input")
            .count()
            - before;
        assert_eq!(
            sends,
            usize::from(positive),
            "Started must reach the selected Herdr execution I/O"
        );
        let requests = rig.requests();
        let created: Vec<_> = requests
            .iter()
            .skip(requests_before)
            .filter(|r| r["method"] == "workspace.create")
            .collect();
        assert_eq!(
            created.len(),
            usize::from(positive),
            "unexpected Herdr workspace effect"
        );
        if positive {
            assert_eq!(
                created[0]["params"]["cwd"],
                root.path().display().to_string()
            );
            let inputs: Vec<_> = requests
                .iter()
                .skip(requests_before)
                .filter(|r| r["method"] == "pane.send_input")
                .collect();
            assert_eq!(inputs[0]["params"]["pane_id"], "w1:p1");
            assert_eq!(inputs[0]["params"]["keys"], serde_json::json!(["enter"]));
            assert!(!inputs[0]["params"]["text"].as_str().unwrap().is_empty());
        }
        assert_eq!(tmux.load(Ordering::SeqCst), 0, "Herdr fell back to tmux");
        if let Some(completed) = completed {
            completed
                .expect("selected Herdr worker must finish its evidence read")
                .unwrap();
        }
        eprintln!(
            "S1_HERDR_IO case={index} retired={retired} started={} herdr={sends} tmux={} skip=0",
            result.started,
            tmux.load(Ordering::SeqCst)
        );
        provider_dispatch::CLAUDE_LEGACY_PROBE
            .lock()
            .unwrap()
            .take();
        if let Some(lifetime) = &mut lifetime {
            lifetime.release();
        }
        shared.mailboxes.remove_fixture_for_test(channel);
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    hook_server.abort();
    assert!(hook_server.await.unwrap_err().is_cancelled());
    api_server.abort();
    assert!(api_server.await.unwrap_err().is_cancelled());
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s1_codex_unproven_and_changed_observations_hold_real_human_start_pg() {
    if !discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::session_strategy_lifecycle_tests::s1_entry_tests::",
        "s1_codex_unproven_and_changed_observations_hold_real_human_start_pg"
    )) {
        return;
    }
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _injection = injection_off();
    let root = tempfile::tempdir().unwrap();
    let _namespace_root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        root.path(),
    );
    let _binding = activity::BindingRoot::enter(root.path());
    let config = serde_json::from_value(serde_json::json!({"server":{},"agents":[]})).unwrap();
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let api = discord::admin_host_guard::tests::Recorder::start().await;
    discord::internal_api::init(api.port, None);
    let mock = discord_mock::DiscordMockState::new();
    let ids: Vec<u64> = (69_184_401..69_184_408).collect();
    for id in &ids {
        mock.allow_channel(*id);
    }
    let (proxy, gateway, server) = discord_mock::start(mock).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let provider = ProviderKind::Codex;
    let (shared, registry) =
        runtime(pool.clone(), api.port, &ctx, &ids, root.path(), &provider).await;
    let event = |kind: &str, id: Option<&str>| {
        let mut row = serde_json::json!({"type":"event_msg","payload":{"type":kind}});
        if let Some(id) = id {
            row["payload"]["turn_id"] = id.into();
        }
        row
    };
    for (index, id) in ids.into_iter().enumerate() {
        let channel = ChannelId::new(id);
        let _limits = activity::small_tail_limits(id);
        let path = root.path().join(format!("{id}.jsonl"));
        let mut rows = vec![
            serde_json::json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}}),
            event("task_started", Some("A")),
        ];
        if index == 4 {
            rows.push(
                serde_json::json!({"type":"response_item","payload":{"type":"unknown_item"}}),
            );
        }
        rows.push(event("task_complete", Some("A")));
        let text: String = rows.iter().map(|row| format!("{row}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let cut = std::fs::metadata(&path).unwrap().len();
        if index == 3 {
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap(),
                "{}",
                event("task_started", Some("B"))
            )
            .unwrap();
        }
        let name = provider.build_tmux_session_name(&format!("s1-entry-{id}"));
        let _turn = activity::bind_turn_mode_transcript(&shared, &provider, id, &name, &path);
        let shrink = path.clone();
        let tamper = path.clone();
        let hook = activity::HandoffMutation::new(
            id,
            (index == 3).then(|| {
                Box::new(move || {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(shrink)
                        .unwrap()
                        .set_len(cut)
                        .unwrap()
                }) as Box<dyn FnOnce() + Send>
            }),
            (index == 4).then(|| {
                Box::new(move || {
                    let mut bytes = std::fs::read(&tamper).unwrap();
                    let at = bytes.windows(3).position(|w| w == b"\"A\"").unwrap();
                    bytes[at + 1] = b'B';
                    std::fs::write(tamper, bytes).unwrap();
                }) as Box<dyn FnOnce() + Send>
            }),
        );
        let _ = activity::settled_reason(&shared, &provider, id).await;
        if [1, 2, 5].contains(&index) {
            let next = match index {
                1 => event("task_started", None),
                2 => event("task_complete", None),
                _ => event("task_complete", Some("B")),
            };
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{next}").unwrap();
            writeln!(file, "{}", event("task_complete", Some("A"))).unwrap();
        } else if index == 6 {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"{\"type\":").unwrap();
        }
        let (mut entered, release, completion, extra) = arm(id);
        let delivered =
            turn_deliver::deliver_human_input(&registry, request_for(channel, provider.clone()))
                .await
                .unwrap();
        if index == 0 {
            assert!(
                matches!(delivered, HumanInputDelivery::Started { .. }),
                "{delivered:?}"
            );
            finish_driver(id, entered, release, completion, extra).await;
        } else {
            assert!(
                matches!(delivered,HumanInputDelivery::Queued{ref reason,..} if reason==turn_deliver::ExternalHold::Unknown.reason()),
                "case={index}: {delivered:?}"
            );
            assert!(
                matches!(
                    entered.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ),
                "held source executed case={index}"
            );
            assert_eq!(extra.load(Ordering::SeqCst), 0);
            assert!(discord::inflight::load_inflight_state(&provider, id).is_none());
            let snapshot = discord::mailbox_snapshot(&shared, channel).await;
            assert!(snapshot.cancel_token.is_none());
            assert_eq!(snapshot.intervention_queue.len(), 1);
            assert!(
                hook.calls() == 1 || ![3, 4].contains(&index),
                "race hook did not run"
            );
            provider_dispatch::INPUT_EFFECT_PROBE.lock().unwrap().take();
            provider_dispatch::CLAUDE_LEGACY_PROBE
                .lock()
                .unwrap()
                .take();
            discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
                .lock()
                .unwrap()
                .take();
            drop(release);
            drop(completion);
            eprintln!("S1_HUMAN_HOLD case={index} selected_io=0 queue=1 skip=0");
        }
        shared.mailboxes.remove_fixture_for_test(channel);
    }
    server.abort();
    let _ = server.await;
    pool.close().await;
    db.drop().await;
}
