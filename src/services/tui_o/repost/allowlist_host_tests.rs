use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::repost::admission::{Admitter, OriginalAttempt};
use crate::services::tui_o::repost::config::RepostConfig;
use crate::services::tui_o::repost::identity::PieceKey;
use crate::services::tui_o::repost::io::probe::evidence::{FIRST_PASS_AFTER, SECOND_PASS_AFTER};
use crate::services::tui_o::repost::provenance::ProvenanceLog;
use crate::services::tui_o::shadow::{UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::LedgerEntry;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};

#[tokio::test(flavor = "current_thread")]
async fn d4b_two_hosted_channels_share_allowlist_with_empty_deny_and_wire_control_pg() {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let runtime = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime.path(),
    );
    let fixture = TestPostgresDb::create().await;
    let migrated = fixture.connect_and_migrate().await;
    let readiness = Arc::new(host::Readiness::default());
    for include in [false, true] {
        let channels = [
            63250831 + 2 * u64::from(include),
            63250832 + 2 * u64::from(include),
        ];
        let mut config = crate::config::Config::default();
        config.tui_o = Some(TuiOConfig {
            repost: RepostConfig {
                enabled: true,
                channels: if include {
                    [channels[0]].into_iter().collect()
                } else {
                    Default::default()
                },
            },
            ..TuiOConfig::default()
        });
        let path = runtime.path().join("validated.yml");
        std::fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
        assert!(matches!(
            crate::config_live_reload::reload_from_path(&path),
            crate::config_live_reload::ReloadOutcome::Applied { .. }
        ));
        let mut cases = Vec::new();
        for channel in channels {
            let root = runtime.path().join(channel.to_string());
            std::fs::create_dir_all(&root).unwrap();
            let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
                .unwrap()
                .unwrap();
            let era = store
                .begin_era(&[channel], chrono::Utc::now(), |channel| {
                    Ok(Initialized {
                        channel,
                        sources: Vec::new(),
                        initial_anchor: 100,
                        build_digest: "allowlist".into(),
                        at: chrono::Utc::now(),
                    })
                })
                .unwrap();
            let unit = |native: &str| UnitKey {
                channel_id: channel,
                provider: ShadowProvider::Claude,
                native_key: native.into(),
                kind: UnitKind::Body,
            };
            let mut ledger = store.open_channel(&era, channel).unwrap().unwrap();
            ledger
                .append_ledger(LedgerEntry::Prepared {
                    local_retry: None,
                    serial: 0,
                    unit_key: unit("proof"),
                    piece_index: 0,
                    payload: "earlier permission".into(),
                    anchor_id: 100,
                    epoch: 1,
                })
                .unwrap();
            ledger
                .append_ledger(LedgerEntry::Posted {
                    serial: 0,
                    msg_id: 900,
                })
                .unwrap();
            drop(ledger);
            let key = PieceKey::new(unit("target"), 0).unwrap();
            let mut provenance = ProvenanceLog::open(&root.join("seed.jsonl")).unwrap();
            Admitter::when_on(&mut on_switch(channel), &migrated, "seed", 42)
                .unwrap()
                .adopt_uncertain(
                    &mut provenance,
                    &OriginalAttempt {
                        serial: 0,
                        key: key.clone(),
                        payload: "absent target".into(),
                        anchor: 100,
                    },
                )
                .await
                .unwrap();
            drop(provenance);
            let server = WireServer::start(channel, &key).await;
            let (pool, statements, proxy) = observed_pool(&fixture.database_url).await;
            statements.reset();
            let turns = Arc::new(WireTurns::default());
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            let io = Arc::new(RealHost {
                gateway: GatewayHost {
                    shared,
                    alarms: Arc::new(AlarmRouter::for_process(None, None)),
                    self_id_wait: Duration::ZERO,
                    configured_id: None,
                    turn: TurnConfig::default(),
                    rest: None,
                    boot: None,
                },
                port: Arc::new(
                    GatewayPort::new(
                        Arc::new(serenity::Http::new(&format!("loopback-{channel}"))),
                        42,
                    )
                    .with_repost_base(server.base.clone()),
                ),
                completed: Arc::new(AtomicUsize::new(0)),
                notice: Arc::new(Notify::new()),
                measure: Arc::new(Notify::new()),
                send: None,
                wire_turns: Some(Arc::clone(&turns)),
            });
            let gate = Arc::new(OwnershipGate::default());
            gate.acquired();
            let mut handles = host::start_managed(
                ShadowProvider::Claude,
                true,
                vec![(
                    channel,
                    Some(ClaudeTui),
                    Some(Candidate::new(Adoption::Committed)),
                )],
                || host::HostParts {
                    io,
                    runtime_root: Some(root),
                    gate,
                    readiness: Arc::clone(&readiness),
                },
            );
            assert_eq!(
                handles.len(),
                1,
                "allowlist does not narrow hosted selection"
            );
            cases.push((
                channel,
                pool,
                statements,
                proxy,
                server,
                turns,
                handles.pop().unwrap(),
            ));
        }
        tokio::join!(cases[0].5.wait(1), cases[1].5.wait(1));
        assert!(channels.iter().all(|channel| readiness.is_hosted(*channel)));
        for (stage, delay) in [(2, FIRST_PASS_AFTER), (3, SECOND_PASS_AFTER)] {
            tokio::time::pause();
            tokio::time::advance(delay).await;
            tokio::time::resume();
            for case in &cases {
                case.5.proceed.notify_one();
            }
            tokio::join!(cases[0].5.wait(stage), cases[1].5.wait(stage));
        }
        for (channel, pool, statements, proxy, server, _, handle) in cases {
            let allowed = include && channel == channels[0];
            assert_eq!(
                statements.count() > 0,
                allowed,
                "channel={channel}: actual re-post PG executions"
            );
            assert_eq!(
                server.probe_reads.load(Ordering::SeqCst) > 0,
                allowed,
                "include={include}, channel={channel}, pg={}, probe_reads={}, all_reads={}, posts={}",
                statements.count(),
                server.probe_reads.load(Ordering::SeqCst),
                server.reads.load(Ordering::SeqCst),
                server.posts.load(Ordering::SeqCst)
            );
            assert_eq!(
                server.posts.load(Ordering::SeqCst),
                if allowed { 2 } else { 0 }
            );
            assert_eq!(
                server.valid_posts.load(Ordering::SeqCst),
                if allowed { 2 } else { 0 }
            );
            assert_eq!(
                runtime
                    .path()
                    .join("tui_o/repost")
                    .join(format!("{channel}.jsonl"))
                    .exists(),
                allowed
            );
            handle.stop_and_join().await.unwrap();
            pool.close().await;
            proxy.abort();
            server.task.abort();
        }
    }
    migrated.close().await;
    fixture.drop().await;
}
