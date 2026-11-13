use super::*;
use crate::services::tui_o::cutover::intake_route::for_claimed_input;
use crate::services::tui_o::store::ledger::PieceOutcome;
use crate::services::tui_o::store::rotation::Boundary;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::binding_events as events;

const REPLY: &str = "fresh Claude reply after missing source";

fn session_start(channel: u64, name: &str, id: &str, path: &Path) -> events::Committed {
    let path = path.to_str().unwrap();
    let hook = events::HookSignal::from_payload(
        "SessionStart",
        &json!({
            "source":"startup", "session_id":id, "transcript_path":path
        }),
    );
    events::record_source(&events::Proposal {
        channel_id: channel,
        provider: "claude",
        tmux_session: name,
        session_id: Some(id),
        path,
        replaced: None,
        cause: events::CauseSource::Launch,
        hook: Some(&hook),
    })
    .unwrap()
}

async fn reply_scenario(restart: bool, dormant: bool) {
    let root = crate::config::TestRuntimeRootGuard::new();
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    let _namespace = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        &root.path().canonicalize().unwrap(),
    );
    // A tmux client prefers `TMUX` over `TMUX_TMPDIR`, so an inherited pane would expose the
    // operator's sessions to the producer-absence probe and keep a missing source from parking.
    let _pane = crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock("TMUX");
    let channel = 78_450_101 + u64::from(restart) + 100 * u64::from(dormant);
    let name = format!("park-reply-{channel}");
    let pane = ProviderKind::Claude.build_tmux_session_name(&name);
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-a"), &[]);
    let config: crate::config::Config = serde_json::from_value(json!({"server":{},
        "cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-a"},
        "agents":[{"id":"park-agent","name":"fixture","provider":"claude",
            "channels":{"claude":{"id":channel.to_string(),"name":name,"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel], "dormant_channels": if dormant { vec![channel] } else { vec![] }}}})).unwrap();
    crate::config_live_reload::install(config.clone());
    let _selected = cutover::test_override::force_boot(
        BootChannels::validate(&config)
            .unwrap()
            .adopted(Adoption::Committed),
    );
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, channel, "gateway").await;
    crate::services::discord::host_defer_gate::tests::map_channel(
        &shared,
        serenity::ChannelId::new(channel),
        &name,
    )
    .await;
    let (store, era, old) = missing_store(root.path(), channel);
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let serial = store
        .open_channel(&era, channel)
        .unwrap()
        .unwrap()
        .ledger()
        .next_serial();
    let original = retained_bytes(root.path(), channel, &old);
    let discord = Discord::new().await;
    let gate = crate::services::tui_o::ownership::gate("claude");
    gate.acquired();
    let mut handles = host::start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        || {
            parts_with_rest(
                &shared,
                &ProviderKind::Claude,
                None,
                Some(discord.http.clone()),
            )
        },
    );
    assert_eq!(handles.len(), 1);
    accepts(channel).await;
    assert_eq!(
        for_input_placement("claude", channel).await,
        IntakeRoute::Gateway
    );
    assert_eq!(
        for_claimed_input("claude", channel, 1, "claimed").await,
        IntakeRoute::Gateway
    );
    let home = root.path().join("claude-home");
    let resolution = crate::services::claude::resolve_claude_tui_session_for_launch(
        root.path(),
        Some(&old.session_id),
        Some(&home),
    )
    .unwrap();
    assert_ne!(resolution.session_id, old.session_id);
    assert!(uuid::Uuid::parse_str(&resolution.session_id).is_ok());
    assert!(!resolution.resume);
    assert!(!resolution.transcript_path.exists());
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let context = PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.clone(),
        tmux_session: pane.clone(),
        channel_id: Some(channel),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: Some("mac-a".into()),
        expected_native_session_id: Some(resolution.session_id.clone()),
        launch_mode: "fresh".into(),
        provider_root: Some(home.join("projects")),
        first_prompt_digest: None,
        source_policy: None,
        resume_start: None,
    })
    .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&pane, "spawn_nonce");
    std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, &nonce).unwrap();
    let (prompt, receiver) = tokio::sync::oneshot::channel();
    let (id, path, provider_pane) = (
        resolution.session_id.clone(),
        resolution.transcript_path.clone(),
        pane.clone(),
    );
    let provider = tokio::spawn(async move {
        assert_eq!(receiver.await.unwrap(), "first prompt");
        assert!(
            !path.exists(),
            "the provider starts writing only after input arrives"
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows = [
            json!({"type":"assistant","uuid":"fresh-answer","sessionId":id,"apiBlockIndex":0,
                "message":{"id":"fresh-answer","content":[{"type":"text","text":REPLY}]}}),
        ];
        let bytes: String = rows.iter().map(|row| format!("{row}\n")).collect();
        std::fs::write(&path, bytes.as_bytes()).unwrap();
        assert!(matches!(
            session_start(channel, &provider_pane, &id, &path),
            events::Committed::Appended
        ));
        (path, bytes.len() as u64)
    });
    prompt.send("first prompt").unwrap();
    let (path, through) = provider.await.unwrap();
    let fresh = crate::services::tui_o::shadow::binding_reader::source_id_for(
        &resolution.session_id,
        &path,
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let channel_store = store.open_channel(&era, channel).unwrap().unwrap();
            if channel_store
                .ledger()
                .piece(serial)
                .is_some_and(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
            {
                assert_eq!(
                    channel_store
                        .rotation()
                        .unwrap()
                        .link(&fresh)
                        .unwrap()
                        .boundary,
                    Boundary::Owed { from: 0 }
                );
                let ledger = channel_store.ledger();
                let posted = (serial..ledger.next_serial())
                    .filter_map(|serial| ledger.piece(serial))
                    .filter(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
                    .count();
                assert_eq!(
                    posted, 1,
                    "the durable ledger must contain one Posted piece"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the admitted fresh session must post its first reply exactly once");
    assert_eq!(discord.posts.lock().unwrap().len(), 1);
    assert_eq!(discord.posts.lock().unwrap()[0]["content"], REPLY);
    let committed = events::binding_events_since(channel, 0).unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].cause, events::BindingCause::Startup);
    for _ in 0..3 {
        super::super::super::super::super::owner_cleanup::refresh_writers(
            &shared,
            &ProviderKind::Claude,
            Some(&TuiOConfig::default()),
        )
        .await;
        assert!(!matches!(
            session_start(channel, &pane, &resolution.session_id, &path),
            events::Committed::Appended
        ));
    }
    let readiness = host::process_readiness();
    let debt = readiness
        .undelivered_after(channel, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        (
            debt.owed,
            debt.prepared,
            debt.unsealed,
            debt.uncaptured,
            debt.binding_pending,
            debt.verified_work,
            debt.posting
        ),
        (0, 0, 0, 0, 0, 0, 0)
    );
    assert!(
        debt.captured
            .iter()
            .any(|(source, end)| source == &path && *end >= through)
    );
    assert_eq!(retained_bytes(root.path(), channel, &old), original);
    if restart {
        handles.pop().unwrap().stop_and_join().await.unwrap();
        let _producer_non_absent =
            super::super::super::super::fence_tests::force_producer_non_absent_for_tests(channel);
        let new_readiness = Arc::new(host::Readiness::default());
        let new_health = Arc::new(crate::services::tui_o::alarm::AlarmHealth::default());
        let mut parts = parts_with_rest(
            &shared,
            &ProviderKind::Claude,
            None,
            Some(discord.http.clone()),
        );
        parts.readiness = new_readiness.clone();
        Arc::get_mut(&mut parts.io).unwrap().alarms =
            Arc::new(AlarmRouter::new(None, None, new_health.clone()));
        let mut restarted = host::start_managed(
            ShadowProvider::Claude,
            true,
            cutover::boot_ownership(),
            || parts,
        );
        assert_eq!(restarted.len(), 1);
        tokio::time::timeout(Duration::from_secs(12), async {
            while !new_readiness.accepts(channel) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a fresh process must park the superseded missing source");
        let debt = new_readiness
            .undelivered_after(channel, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            (debt.owed, debt.uncaptured, debt.binding_pending),
            (0, 0, 0)
        );
        assert_eq!(discord.posts.lock().unwrap().len(), 1);
        assert_eq!(retained_bytes(root.path(), channel, &old), original);
        assert!(
            !new_health
                .current_at(std::time::Instant::now())
                .contains(&format!("tui_o:halted:{channel}"))
        );
        restarted.pop().unwrap().stop_and_join().await.unwrap();
    } else {
        handles.pop().unwrap().stop_and_join().await.unwrap();
    }
    assert!(
        !crate::services::tui_o::alarm::health_reasons()
            .contains(&format!("tui_o:halted:{channel}"))
    );
    assert_eq!(discord.posts.lock().unwrap().len(), 1);
    drop(context);
    gate.lost();
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn parked_channel_admits_claude_input_and_posts_first_reply_once_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::parked_channel_admits_claude_input_and_posts_first_reply_once_pg"
    )) {
        return;
    }
    reply_scenario(false, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn restart_after_new_source_keeps_old_parked_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::restart_after_new_source_keeps_old_parked_pg"
    )) {
        return;
    }
    reply_scenario(true, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn reader_config_parked_channel_admits_claude_input_and_posts_first_reply_once_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::reader_config_parked_channel_admits_claude_input_and_posts_first_reply_once_pg"
    )) {
        return;
    }
    reply_scenario(false, true).await;
}
