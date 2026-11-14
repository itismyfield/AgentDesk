use super::*;
use crate::services::provider_hosting::{self, ProviderSessionDriver};

fn config(channel: u64, bound: bool) -> crate::config::Config {
    let mut agents = vec![json!({"id": "selected", "name": "Selected", "channels": {
        "codex": {"id": (channel + 1).to_string(), "runtime": "tui"}
    }})];
    if bound {
        agents.push(
            json!({"id": "custody-agent", "name": "Fixture", "channels": {
                "claude": {"id": channel.to_string(), "runtime": "tui"}
            }}),
        );
    }
    serde_json::from_value(json!({
        "server": {}, "providers": {"claude": {"runtime": "pipe"}},
        "cluster": {"enabled": true, "instance_id": "mac-a", "gateway_preferred_instance_id": "mac-a"},
        "agents": agents, "tui_o": {"writer": {"channels": [channel + 1]}}
    }))
    .unwrap()
}

fn boot(config: &crate::config::Config, channel: u64) -> cutover::test_override::ChannelsGuard {
    let root = crate::config::runtime_root().unwrap();
    std::fs::write(
        root.join("isolated-boot.json"),
        serde_json::to_vec(config).unwrap(),
    )
    .unwrap();
    provider_hosting::install_provider_hosting_config(config);
    cutover::test_override::force_boot(
        BootChannels::validate(config)
            .unwrap()
            .seeded(config, |selected, _| {
                Ok(crate::services::tui_o::channel_policy::stored(
                    Some(&root),
                    &selected
                        .union(&std::collections::BTreeSet::from([channel]))
                        .copied()
                        .collect(),
                ))
            })
            .unwrap(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn unbound_member_live_checkpoint_survives_rebind_restart_and_catchup_once_pg() {
    scenario(
        "boot_input_gateway_tests::unbound_gateway_tests::unbound_member_live_checkpoint_survives_rebind_restart_and_catchup_once_pg",
        73_369_543, "mac-a", |f| Box::pin(async move {
            let subscriber = tracing_subscriber::fmt().with_test_writer()
                .with_max_level(tracing::Level::INFO).with_ansi(false).finish();
            let _trace = tracing::subscriber::set_default(subscriber);
            prepare_gateway(f).await;
            f.shared.core.lock().await.sessions.get_mut(&serenity::ChannelId::new(f.channel))
                .unwrap().restore_provider_session(Some("restored-direct-session".into()));
            // Retire the previous process's writer before booting with its binding removed.
            f.release.add_permits(1);
            for handle in f.handles.drain(..) {
                handle.stop_and_join().await.unwrap();
            }
            let before = config(f.channel, true);
            provider_hosting::install_provider_hosting_config(&before);
            assert_eq!(provider_hosting::resolve_provider_session_selection_with_channel(
                &ProviderKind::Claude, true, Some(f.channel)).driver, ProviderSessionDriver::TuiHosting);
            let missing = boot(&config(f.channel, false), f.channel);
            let snapshot = cutover::boot_snapshot().unwrap();
            assert!(snapshot.channels().contains(&f.channel));
            assert!(snapshot.kind(f.channel).is_none());
            assert_eq!(provider_hosting::resolve_provider_session_selection_with_channel(
                &ProviderKind::Claude, true, Some(f.channel)).driver, ProviderSessionDriver::LegacyPrompt);
            let channel = serenity::ChannelId::new(f.channel);
            assert!(!router::live_checkpoint_waits(&ProviderKind::Claude, channel));
            let event = gateway_message(f);
            crate::services::discord::queue_io::input_ingress::ADOPTS.scope(true, async {
                assert_eq!(crate::services::discord::queue_io::input_ingress::adopt(
                    &f.shared, &ProviderKind::Claude, channel).await, Ok(false));
                assert!(matches!(for_input_placement("claude", f.channel).await, IntakeRoute::Hold(_)));
                let checkpoint = crate::services::discord::runtime_store::last_message_root().unwrap()
                    .join("claude").join(format!("{}.txt", f.channel));
                crate::services::discord::advance_last_message_checkpoint(
                    &f.shared, &ProviderKind::Claude, channel, serenity::MessageId::new(f.message - 1));
                let saved = std::fs::read(&checkpoint).unwrap();
                for _ in 0..2 {
                    deliver_gateway(f, &event).await;
                    assert_eq!(f.shared.last_message_ids.get(&channel).map(|id| *id),
                        Some(f.message - 1), "Hold must preserve the live checkpoint before custody");
                    assert_eq!(std::fs::read(&checkpoint).unwrap(), saved);
                    assert!(f.disk().is_empty());
                    assert_eq!(f.effects.load(Ordering::SeqCst), 0);
                }
                assert!(f.shared.dispatch.intake_dedup.contains_key(&format!("mid:{}", f.message)),
                    "the live adapter observed the original before suppressing redelivery");
            }).await;
            drop(missing);

            // Rebuild the boot snapshot and runtime over the same durable store/checkpoint.
            let rebound = boot(&config(f.channel, true), f.channel);
            let pool = f.shared.pg_pool.as_ref().unwrap().clone();
            let permit = execution::read(&pool, "claude", f.channel, "mac-a", || Ok(false))
                .await.unwrap().unwrap();
            cutover::boot_snapshot().unwrap().install_execution(f.channel, Some(Ok(permit)));
            let (registry, shared) = runtime(&pool, ProviderKind::Claude, f.channel, "gateway").await;
            f.shared = shared;
            let server = gateway_context(&f.shared, f.channel).await;
            prepare_gateway(f).await;
            assert!(!f.shared.last_message_ids.contains_key(&channel));
            assert!(!f.shared.dispatch.intake_dedup.contains_key(&format!("mid:{}", f.message)));
            let entered = Arc::new(tokio::sync::Notify::new());
            f.release = Arc::new(tokio::sync::Semaphore::new(0));
            let parts = parts_with_rest(&f.shared, &ProviderKind::Claude, None, Some(f.discord.http.clone()));
            f.handles = host::start_managed(ShadowProvider::Claude, true, cutover::boot_ownership(), || host::HostParts {
                io: Arc::new(Paused { io: parts.io, entered: entered.clone(), release: f.release.clone(), once: AtomicBool::new(false) }),
                runtime_root: parts.runtime_root, gate: parts.gate, readiness: host::process_readiness(),
            });
            assert_eq!(f.handles.len(), 1, "only the committed rebound channel starts");
            tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.unwrap();
            assert!(f.global.current().epoch().is_some(), "rebound gateway lease is owned");
            assert!(cutover::boot_snapshot().unwrap().execution(f.channel).unwrap()
                .permit.unwrap().gate().current().epoch().is_some(), "rebound execution lease is owned");
            f.release.add_permits(1);
            tokio::time::timeout(Duration::from_secs(5), async {
                while !host::channel_accepts(f.channel) {
                    tokio::task::yield_now().await;
                }
            }).await.expect("the rebound writer becomes ready before catch-up");
            let expected = f.request();
            let effects = f.effects.clone();
            let _effect = herdr_consent::override_boot_effect_for_tests(move |request| {
                assert_eq!(request.user_msg_id, expected.user_msg_id);
                assert_eq!(request.user_text, expected.user_text);
                effects.fetch_add(1, Ordering::SeqCst);
                Ok(crate::services::discord::IntakeExecution::Executed)
            });
            let history = HistoryHttp::start(f).await;
            history.state.release.add_permits(1);
            tokio::time::timeout(Duration::from_secs(10), Box::pin(
                crate::services::discord::catch_up::catch_up_missed_messages(
                    &history.http, &f.shared, &ProviderKind::Claude)))
                .await.expect("restart catch-up finishes");
            assert!(history.state.fetched.load(Ordering::SeqCst) > 0);
            assert!(history.state.returned.lock().unwrap().contains(&f.message),
                "the original input must actually be rediscovered from REST history");
            assert!(history.state.unhandled.lock().unwrap().is_empty());
            tokio::time::timeout(Duration::from_secs(5), async {
                while f.effects.load(Ordering::SeqCst) == 0 || !f.disk().is_empty() {
                    tokio::task::yield_now().await;
                }
            }).await.expect("the recovered original executes");
            assert_eq!(f.effects.load(Ordering::SeqCst), 1);
            assert_eq!(f.shared.last_message_ids.get(&channel).map(|id| *id), Some(f.message));
            crate::services::discord::catch_up::catch_up_missed_messages(
                &history.http, &f.shared, &ProviderKind::Claude).await;
            assert_eq!(f.effects.load(Ordering::SeqCst), 1, "repeated catch-up executes the original once");
            assert!(f.disk().is_empty());
            for handle in f.handles.drain(..) {
                handle.stop_and_join().await.unwrap();
            }
            server.abort();
            drop((registry, rebound));
        }),
    ).await;
}
