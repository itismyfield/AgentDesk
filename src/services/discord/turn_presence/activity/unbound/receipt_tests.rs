//! A fresh routine's actual starter receipt, through ownership publication and reset/start.
use super::super::review_tests::Rig;
use super::*;
use crate::services::discord::router::message_handler::{
    CLAUDE_LEGACY_PROBE, INPUT_EFFECT_PROBE, InputEffectProbe,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE;
use crate::services::routines::RoutineAgentExecutor;
use crate::services::routines::store::{NewRoutine, NextDueAtUpdate, RoutineStore};
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ProviderTurn {
    entered: Option<tokio::sync::oneshot::Receiver<std::thread::ThreadId>>,
    release: Option<std::sync::mpsc::Sender<()>>,
    done: Option<tokio::sync::oneshot::Receiver<()>>,
    extra: Arc<AtomicUsize>,
}
impl ProviderTurn {
    fn arm(channel: u64, unexpected_channel: u64) -> Self {
        let (entered, ran) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let probe = InputEffectProbe {
            channel,
            entered,
            release: wait,
            terminal_before_release: false,
        };
        assert!(INPUT_EFFECT_PROBE.lock().unwrap().replace(probe).is_none());
        let extra = Arc::new(AtomicUsize::new(0));
        let count = extra.clone();
        assert!(
            CLAUDE_LEGACY_PROBE
                .lock()
                .unwrap()
                .replace((
                    unexpected_channel,
                    Box::new(move || {
                        count.fetch_add(1, Ordering::SeqCst);
                        Err("unexpected real provider start".into())
                    })
                ))
                .is_none()
        );
        let (completed, done) = tokio::sync::oneshot::channel();
        assert!(
            BRIDGE_COMPLETION_PROBE
                .lock()
                .unwrap()
                .replace((ChannelId::new(channel), completed))
                .is_none()
        );
        Self {
            entered: Some(ran),
            release: Some(release),
            done: Some(done),
            extra,
        }
    }
    async fn entered(&mut self) {
        let ran = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.entered.take().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(ran, std::thread::current().id());
        assert!(INPUT_EFFECT_PROBE.lock().unwrap().is_none());
    }
    async fn finish(mut self, pool: &sqlx::PgPool) {
        self.release.take().unwrap().send(()).unwrap();
        let delivered = AtomicUsize::new(0);
        // Stand in for the outbox worker while the real bridge/postlude completes.
        let worker = async {
            loop {
                let rows = sqlx::query("UPDATE message_outbox SET status='sent', sent_at=NOW() WHERE status='pending' AND source='headless_turn'")
                    .execute(pool).await.unwrap().rows_affected();
                delivered.fetch_add(rows as usize, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            result = tokio::time::timeout(std::time::Duration::from_secs(10), self.done.take().unwrap()) => {
                result.unwrap().unwrap();
            },
            _ = worker => unreachable!(),
        }
        assert!(delivered.load(Ordering::SeqCst) > 0);
        assert_eq!(self.extra.load(Ordering::SeqCst), 0);
    }
}
impl Drop for ProviderTurn {
    fn drop(&mut self) {
        self.release.take();
        INPUT_EFFECT_PROBE.lock().unwrap().take();
        CLAUDE_LEGACY_PROBE.lock().unwrap().take();
        BRIDGE_COMPLETION_PROBE.lock().unwrap().take();
    }
}

async fn owned(pool: &sqlx::PgPool, run: &str) -> Option<String> {
    sqlx::query_scalar("SELECT owned_tmux_session FROM routine_runs WHERE id = $1")
        .bind(run)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn routine_receipt_survives_reset_and_starts_one_provider_turn_pg() {
    const CHILD: &str = "ADK_RECEIPT_E2E_CHILD";
    const TEST: &str = "services::discord::turn_presence::activity::unbound::live::owner_tests::receipt_tests::routine_receipt_survives_reset_and_starts_one_provider_turn_pg";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env(
                crate::services::tui_o::cutover::test_override::CHILD_ENV,
                "1",
            )
            .env(
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[]",
            )
            .output()
            .unwrap();
        let logs = format!(
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        eprintln!("{logs}");
        assert!(result.status.success(), "{logs}");
        assert!(
            logs.contains("RECEIPT_E2E producer=2 process=1 consumed=1 route=1 extra=0 skip=0")
        );
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .try_init();
    let mut rig = Rig::new(69_183_106, false, false).await;
    Arc::get_mut(&mut rig.shared).unwrap().token_hash = "discord_0123456789abcdef".into();
    let _turn_mode = TestConfirmation::new(rig.channel.get());
    let host = HostSessions::new(&rig).await;
    host.start(MANUAL);
    let pool = rig.shared.pg_pool.clone().unwrap();
    let parent = rig.channel.get();
    let thread = THREAD + 1;
    let thread_id = ChannelId::new(thread);
    let agent = "receipt-agent";
    crate::db::agents::insert_agent_channels_for_tests(
        &pool,
        agent,
        Some(&parent.to_string()),
        None,
    )
    .await;
    let workspace = rig.root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    crate::services::discord::host_defer_gate::tests::map_channel(
        &rig.shared,
        thread_id,
        "receipt-primary",
    )
    .await;
    {
        let mut core = rig.shared.core.lock().await;
        for id in [rig.channel, thread_id] {
            core.sessions.get_mut(&id).unwrap().current_path =
                Some(workspace.display().to_string());
        }
    }
    {
        let mut settings = rig.shared.settings.write().await;
        settings.owner_user_id = Some(100);
        settings.allowed_user_ids = vec![200];
        settings.allowed_channel_ids = vec![parent, thread];
    }
    let mock = discord_mock::DiscordMockState::new();
    mock.park_first_placeholder.store(false, Ordering::SeqCst);
    mock.allow_channel(parent);
    mock.add_thread(thread, parent);
    let (proxy, gateway, discord_server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    rig.shared.http.cached_serenity_ctx.set(ctx).unwrap();
    rig.shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    let registry = Arc::new(HealthRegistry::new());
    registry.register("claude".into(), rig.shared.clone()).await;
    registry
        .register_http(
            "claude".into(),
            rig.shared
                .http
                .cached_serenity_ctx
                .get()
                .unwrap()
                .http
                .clone(),
        )
        .await;
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).unwrap();
    let broadcast = crate::eventbus::new_broadcast();
    let buffer = crate::eventbus::spawn_batch_flusher(broadcast.clone());
    let app = crate::server::routes::api_router_with_pg(
        engine,
        config,
        broadcast,
        buffer,
        Some(registry.clone()),
        Some(pool.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    crate::services::discord::internal_api::init(port, Some(pool.clone()));
    let serving = axum::Router::new().nest("/api", app.clone());
    let api_server = tokio::spawn(async move {
        axum::serve(
            listener,
            serving.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        256 * 1024,
    );
    let routine = store
        .attach_routine(NewRoutine {
            agent_id: Some(agent.into()),
            fallback_agent_id: None,
            max_retries: Some(0),
            script_ref: "receipt.js".into(),
            name: "family-morning-briefing-with-a-label-longer-than-forty-four-bytes".into(),
            status: None,
            execution_strategy: "fresh".into(),
            schedule: Some("0 3 * * *".into()),
            next_due_at: Some(chrono::Utc::now() - chrono::Duration::minutes(5)),
            checkpoint: None,
            discord_thread_id: Some(thread.to_string()),
            timeout_secs: None,
        })
        .await
        .unwrap();
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), Some(registry.clone()), 60);
    let mut receipt = String::new();
    let mut tmux_name = String::new();
    let mut old_run = String::new();
    for attempt in 0..2 {
        if attempt == 1 {
            sqlx::query(
                "UPDATE routines SET next_due_at = NOW() - INTERVAL '1 minute' WHERE id = $1",
            )
            .bind(&routine.id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let claimed = store
            .claim_due_runs(1)
            .await
            .unwrap()
            .pop()
            .expect("due routine");
        let run = claimed.run_id.clone();
        let mut turn = ProviderTurn::arm(thread, parent);
        let outcome = executor
            .start_agent_run(
                &store,
                claimed,
                "brief".into(),
                None,
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!(outcome.status, "running", "{outcome:?}");
        assert_eq!(
            outcome.result_json.as_ref().unwrap()["channel_id"],
            thread.to_string(),
            "routine must use its real thread"
        );
        turn.entered().await;
        let row = inflight::load_inflight_state(&ProviderKind::Claude, thread).unwrap();
        let name = row
            .tmux_session_name
            .as_deref()
            .expect("actual seeded name");
        let expected = adk_session::build_namespaced_session_key(
            &rig.shared.token_hash,
            &ProviderKind::Claude,
            name,
        );
        assert_eq!(
            owned(&pool, &run).await.as_deref(),
            Some(expected.as_str()),
            "starter receipt missing or guessed"
        );
        let session: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT thread_channel_id, channel_id FROM sessions WHERE session_key = $1",
        )
        .bind(&expected)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            session,
            (None, Some(thread.to_string())),
            "synthetic row must reproduce the resolver gap"
        );
        assert!(!name.ends_with(&format!("-t{thread}")));
        if attempt == 0 {
            receipt = expected;
            tmux_name = name.to_string();
            old_run = run.clone();
        } else {
            assert_eq!(
                expected, receipt,
                "same routine and agent reuse the actual name"
            );
            assert_ne!(run, old_run);
            assert_eq!(
                owned(&pool, &old_run).await.as_deref(),
                Some(receipt.as_str())
            );
        }
        turn.finish(&pool).await;
        assert!(
            inflight::load_inflight_state(&ProviderKind::Claude, thread).is_none(),
            "completed producer must not leave inflight behind"
        );
        assert!(
            store
                .complete_agent_run(
                    &run,
                    outcome.result_json,
                    None,
                    None,
                    NextDueAtUpdate::Preserve
                )
                .await
                .unwrap()
        );
        rig.shared.tmux_watchers.remove(&thread_id);
    }
    sqlx::query("UPDATE routines SET next_due_at = NOW() - INTERVAL '1 minute' WHERE id = $1")
        .bind(&routine.id)
        .execute(&pool)
        .await
        .unwrap();
    let claimed = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    let process_run = claimed.run_id.clone();
    let mut process_turn = ProviderTurn::arm(thread, parent);
    let process = executor
        .clone()
        .with_headless_fresh_agents(true)
        .start_agent_run(
            &store,
            claimed,
            "process brief".into(),
            None,
            None,
            None,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(process.status, "running", "{process:?}");
    process_turn.entered().await;
    assert!(
        inflight::load_inflight_state(&ProviderKind::Claude, thread)
            .unwrap()
            .tmux_session_name
            .is_none()
    );
    assert_eq!(owned(&pool, &process_run).await, None);
    process_turn.finish(&pool).await;
    assert!(
        store
            .complete_agent_run(
                &process_run,
                process.result_json,
                None,
                None,
                NextDueAtUpdate::Preserve
            )
            .await
            .unwrap()
    );
    host.start(&tmux_name);
    assert!(
        rig.shared
            .tmux_watchers
            .owner_channel_for_tmux_session(&tmux_name)
            .is_none()
    );
    assert_eq!(
        unbound(&rig).await.0,
        observed(Activity::Idle, "no_session")
    );
    let unknown = observed(Activity::Unknown, "session_identity_unresolved");
    let other_key =
        adk_session::build_namespaced_session_key("other-token", &ProviderKind::Claude, &tmux_name);
    sqlx::query(
        "UPDATE routine_runs SET owned_tmux_session = $1 WHERE owned_tmux_session IS NOT NULL",
    )
    .bind(other_key)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        unbound(&rig).await.0,
        unknown,
        "different token must never establish ownership"
    );
    sqlx::query(
        "UPDATE routine_runs SET owned_tmux_session = $1 WHERE owned_tmux_session IS NOT NULL",
    )
    .bind(&receipt)
    .execute(&pool)
    .await
    .unwrap();
    let (_, stamp, watch) = unbound(&rig).await;
    sqlx::query("UPDATE routine_runs SET result_json = jsonb_set(result_json, '{channel_id}', to_jsonb($1::text)) WHERE id = $2")
        .bind(parent.to_string()).bind(&old_run).execute(&pool).await.unwrap();
    assert!(stamp.publish(&watch, || ()).await.is_none());
    assert_eq!(
        unbound(&rig).await.0,
        observed(Activity::Unknown, "watcher_unbound"),
        "old self run must win over newer foreign run"
    );
    sqlx::query("UPDATE routine_runs SET result_json = jsonb_set(result_json, '{channel_id}', to_jsonb($1::text)) WHERE id = $2")
        .bind(thread.to_string()).bind(&old_run).execute(&pool).await.unwrap();

    let bound = ProviderKind::Claude.build_tmux_session_name(&format!("s1e-{parent}"));
    let bound_output = rig.root.path().join("reset-bound.jsonl");
    std::fs::write(&bound_output, b"").unwrap();
    crate::services::discord::turn_presence::activity::tests::companion_attach_log_watcher(
        &rig.shared,
        parent,
        &bound,
        &bound_output,
    );
    assert!(
        rig.shared
            .tmux_watchers
            .channel_binding(&rig.channel)
            .is_some()
    );
    crate::services::discord::host_teardown_gate::test_support::busy_turn(
        &rig.shared,
        rig.channel,
        &bound,
    )
    .await;
    let target = TurnLifecycleTarget {
        provider: Some(ProviderKind::Claude),
        channel_id: Some(rig.channel),
        tmux_name: bound,
    };
    let stopped = force_kill_turn(Some(&registry), &target, "receipt reset", "receipt_test").await;
    assert_eq!(stopped.lifecycle_path, "runtime-fallback", "{stopped:?}");
    assert_eq!(stopped.mailbox_foreground_free, Some(true));
    assert!(
        rig.shared
            .tmux_watchers
            .channel_binding(&rig.channel)
            .is_none()
    );
    let mut turn = ProviderTurn::arm(parent, parent);
    let (status, body) = post_start(&app, agent).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["status"], "started");
    turn.entered().await;
    let row = inflight::load_inflight_state(&ProviderKind::Claude, parent).unwrap();
    assert_eq!(
        body["turn_id"],
        format!("discord:{parent}:{}", row.user_msg_id)
    );
    let (again, refused) = post_start(&app, agent).await;
    assert_eq!(again, axum::http::StatusCode::CONFLICT, "{refused}");
    turn.finish(&pool).await;
    let mut codex =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut codex).unwrap().provider = ProviderKind::Codex;
    codex.settings.write().await.provider = ProviderKind::Codex;
    codex
        .http
        .cached_serenity_ctx
        .set(rig.shared.http.cached_serenity_ctx.get().unwrap().clone())
        .unwrap();
    codex
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    registry
        .register_http(
            "codex".into(),
            codex.http.cached_serenity_ctx.get().unwrap().http.clone(),
        )
        .await;
    registry.register("codex".into(), codex).await;
    sqlx::query("UPDATE agents SET provider = 'codex', discord_channel_cdx = discord_channel_cc WHERE id = $1")
        .bind(agent).execute(&pool).await.unwrap();
    sqlx::query("UPDATE routines SET next_due_at = NOW() - INTERVAL '1 minute' WHERE id = $1")
        .bind(&routine.id)
        .execute(&pool)
        .await
        .unwrap();
    let claimed = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    let consumed_run = claimed.run_id.clone();
    let _unexpected_provider = ProviderTurn::arm(thread, parent);
    let consumed = executor
        .start_agent_run(
            &store,
            claimed,
            "/goal pause".into(),
            None,
            None,
            None,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(consumed.status, "succeeded", "{consumed:?}");
    assert_eq!(consumed.result_json.as_ref().unwrap()["status"], "consumed");
    assert_eq!(owned(&pool, &consumed_run).await, None);
    assert!(
        INPUT_EFFECT_PROBE.lock().unwrap().is_some(),
        "a consumed command must not enter its provider"
    );
    drop(_unexpected_provider);
    assert!(
        mock.unhandled.lock().unwrap().is_empty(),
        "{:?}",
        mock.unhandled.lock().unwrap()
    );
    eprintln!("RECEIPT_E2E producer=2 process=1 consumed=1 route=1 extra=0 skip=0");
    api_server.abort();
    discord_server.abort();
}
