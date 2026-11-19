use super::*;
use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
use crate::services::discord::tmux::tmux_watcher::{WatcherDispatch, finalize_dispatch};
use crate::services::discord::turn_bridge::RuntimeDatabase;
use axum::http::StatusCode;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const WORK: &str = "native-work";

async fn seed(pool: &sqlx::PgPool, id: &str, kind: &str) {
    sqlx::query("INSERT INTO agents (id,name) VALUES ('worker','worker') ON CONFLICT DO NOTHING")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO kanban_cards (id,title,status,assigned_agent_id) VALUES ($1,$1,'in_progress','worker')")
        .bind(id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO task_dispatches (id,kanban_card_id,to_agent_id,dispatch_type,status) VALUES ($1,$1,'worker',$2,'pending')")
        .bind(id).bind(kind).execute(pool).await.unwrap();
}

async fn state(pool: &sqlx::PgPool, id: &str) -> (String, Option<String>, Option<String>, i64) {
    sqlx::query_as(
        "SELECT status, result::jsonb->>'completion_source', result::jsonb->>'completed_commit',
        (SELECT count(*) FROM dispatch_events WHERE dispatch_id=$1 AND to_status='completed')
        FROM task_dispatches WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(path)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

async fn watcher(
    shared: &SharedData,
    row: &inflight::InflightTurnState,
    held: bool,
    body: &str,
) -> (Option<String>, bool) {
    finalize_dispatch(WatcherDispatch {
        shared,
        channel_id: ChannelId::new(row.channel_id),
        watcher_provider: &ProviderKind::Claude,
        tmux_session_name: row.tmux_session_name.as_deref().unwrap(),
        pin: (held, Some(row)),
        late: (held, Some(row)),
        full_response: body,
        has_assistant_response: true,
        lifecycle_stage_paused: false,
        completion_is_stale_for_newer_turn: false,
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_dispatch_custody_completes_work_once_from_native_body_pg() {
    if !api_child(concat!(
        module_path!(),
        "::native_8148_dispatch_custody_completes_work_once_from_native_body_pg"
    )) {
        return;
    }
    use super::super::super::native_supply::native_completion::record;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let live = RuntimeDatabase::after_env_lock();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    live.install(&db.database_url);
    seed(&pool, WORK, "implementation").await;
    let done = Arc::new(AtomicBool::new(false));
    let snapshot = done.clone();
    let api = Recorder::start_with_status(Arc::new(move |_, uri| match uri.path() {
        "/api/dispatches/native-work" => Some((
            StatusCode::OK,
            json!({"dispatch":{
                "status": if snapshot.load(Ordering::SeqCst) {"completed"} else {"pending"},
                "dispatch_type":"implementation","kanban_card_id":WORK
            }}),
        )),
        "/api/internal/card-thread" => {
            Some((StatusCode::OK, json!({"dispatch_type":"implementation"})))
        }
        _ => None,
    }))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    let policies = tempfile::tempdir().unwrap();
    std::fs::write(policies.path().join("completion.js"),
        "agentdesk.registerPolicy({name:'NativeCompletionProbe',onDispatchCompleted:function(){const key='native-completion-hook';agentdesk.kv.set(key,Number(agentdesk.kv.get(key)||0)+1,0);}});").unwrap();
    let mut config = crate::config::Config::default();
    config.policies.dir = policies.path().to_path_buf();
    config.policies.hot_reload = false;
    let mut shared = b2b_shared(Some(pool.clone()));
    let engine = Arc::get_mut(&mut Arc::get_mut(&mut shared).unwrap().engine).unwrap();
    engine.api_port = api.port;
    engine.policy.engine =
        Some(crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone())).unwrap());
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    git(path, &["init"]);
    git(path, &["commit", "--allow-empty", "-m", "native fixture"]);
    let native_sha = git(path, &["rev-parse", "HEAD"]);
    git(path, &["commit", "--allow-empty", "-m", "watcher fixture"]);
    let watcher_sha = git(path, &["rev-parse", "HEAD"]);

    let f = Fixture::at(
        runtime.path().join("runtime/injected_frames.json"),
        81480610,
        true,
    );
    let (key, actor) = b2b_active(&f, &shared).await;
    let mut row = b2b_row(&f);
    let turn_id = turn(&row);
    row.dispatch_id = Some(WORK.into());
    row.worktree_path = Some(path.display().to_string());
    row.scheduled = ScheduledTurnMark::Scheduled {
        turn_id: turn_id.clone(),
        isolated: false,
        outcome: None,
    };
    inflight::save_inflight_state(&row).unwrap();
    launch_agent_delivery(&pool, WORK, &turn_id, false, "fail").await;
    supply_register(&f, &shared, key, Some(actor)).await;
    assert!(
        crate::services::discord::turn_finalizer::native_prepare::holds_row(&shared, &row).await
    );
    assert_eq!(
        watcher(&shared, &row, true, &watcher_sha).await,
        (Some(WORK.into()), true)
    );
    assert_eq!(
        state(&pool, WORK).await,
        ("pending".into(), None, None, 0),
        "watcher raced before native"
    );
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id).is_some()
    );

    let evidence = proof(&f, Some(&native_sha), Disposition::Executed);
    durable(&f, &evidence);
    record(&shared, &f.history, &evidence, key, Some(&row), None).await;
    let completed = state(&pool, WORK).await;
    assert_eq!(
        completed,
        (
            "completed".into(),
            Some("turn_bridge_explicit".into()),
            Some(native_sha),
            1
        )
    );
    done.store(true, Ordering::SeqCst);
    let first = scheduled(&pool, &turn_id)
        .await
        .expect("first scheduled outcome");
    assert!(
        !f.history
            .native_settlement(NS, &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    record(&shared, &f.history, &evidence, key, Some(&row), None).await;
    assert_eq!(scheduled(&pool, &turn_id).await, Some(first));
    assert_eq!(state(&pool, WORK).await, completed);
    let hooks: String =
        sqlx::query_scalar("SELECT value FROM kv_meta WHERE key='native-completion-hook'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(hooks, "1");
    drop(shared);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_watcher_dispatch_fallback_preserves_work_types_pg() {
    if !api_child(concat!(
        module_path!(),
        "::native_8148_watcher_dispatch_fallback_preserves_work_types_pg"
    )) {
        return;
    }
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let live = RuntimeDatabase::after_env_lock();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    live.install(&db.database_url);
    for (kind, completes) in [
        ("implementation", true),
        ("rework", true),
        ("plan", false),
        ("plan-review", false),
    ] {
        let api = Recorder::start_with_status(Arc::new(move |_, uri| match uri.path() {
            "/api/internal/pending-dispatch-for-thread" => {
                Some((StatusCode::OK, json!({"dispatch_id":kind})))
            }
            "/api/internal/card-thread" => Some((StatusCode::OK, json!({"dispatch_type":kind}))),
            _ => None,
        }))
        .await;
        crate::services::discord::internal_api::init(api.port, None);
        seed(&pool, kind, kind).await;
        let f = Fixture::at(
            runtime.path().join(kind).join("frames.json"),
            81480611,
            true,
        );
        let row = b2b_row(&f);
        let shared = b2b_shared(Some(pool.clone()));
        assert!(row.dispatch_id.is_none());
        assert_eq!(
            watcher(&shared, &row, false, "watcher fallback").await,
            (Some(kind.into()), true)
        );
        let calls = api.take();
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("GET /api/internal/pending-dispatch-for-thread "))
        );
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("GET /api/internal/card-thread "))
        );
        assert_eq!(state(&pool, kind).await.3, i64::from(completes), "{kind}");
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_dispatch_snapshot_failure_settles_policy_and_cleans_row_pg() {
    if !api_child(concat!(
        module_path!(),
        "::native_8148_dispatch_snapshot_failure_settles_policy_and_cleans_row_pg"
    )) {
        return;
    }
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let live = RuntimeDatabase::after_env_lock();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    live.install(&db.database_url);
    seed(&pool, WORK, "implementation").await;
    let api = Recorder::start_with_status(Arc::new(|method, uri| {
        (method == axum::http::Method::GET && uri.path() == "/api/dispatches/native-work")
            .then(|| (StatusCode::SERVICE_UNAVAILABLE, json!({})))
    }))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    let root = runtime.path().join("runtime");
    let f = Fixture::at(root.join("injected_frames.json"), 81480612, true);
    let mut row = b2b_row(&f);
    row.dispatch_id = Some(WORK.into());
    inflight::save_inflight_state(&row).unwrap();
    let evidence = proof(&f, Some("native answer"), Disposition::Executed);
    durable(&f, &evidence);
    let mut shared = b2b_shared(Some(pool.clone()));
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 2;
    super::super::super::super::native_prepare::BootInputs::prepare(&root, 2)
        .unwrap()
        .restore(&shared)
        .await
        .unwrap();
    assert_eq!(state(&pool, WORK).await.0, "failed");
    let failures: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dispatch_events WHERE dispatch_id=$1 AND transition_source='turn_bridge_dispatch_failure'",
    ).bind(WORK).fetch_one(&pool).await.unwrap();
    assert_eq!(failures, 1);
    assert!(transcript(&pool, &turn(&row)).await.is_some());
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id).is_none()
    );
    assert!(
        f.history
            .native_settlement(NS, &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    assert!(
        api.take()
            .iter()
            .any(|call| call.starts_with("GET /api/dispatches/native-work "))
    );
    pool.close().await;
    db.drop().await;
}
