use super::*;
use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
use crate::services::discord::tmux::tmux_watcher::{WatcherDispatch, finalize_dispatch};
use crate::services::discord::turn_bridge::RuntimeDatabase;
use axum::http::StatusCode;

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

struct WorkFixture {
    // The live DB configuration must be restored while the ROOT lock still belongs to the fixture.
    live: RuntimeDatabase,
    completion: CompletionFixture,
}

impl WorkFixture {
    async fn new() -> Self {
        let runtime = crate::config::TestRuntimeRootGuard::new();
        let live = RuntimeDatabase::after_env_lock();
        let completion = CompletionFixture::in_runtime(runtime).await;
        live.install(&completion.db.database_url);
        Self { live, completion }
    }

    async fn close(self) {
        drop(self.live);
        self.completion.close().await;
    }
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

#[tokio::test]
async fn native_8148_fresh_idle_preserves_native_custody() {
    let runtime = TestRuntimeRootGuard::new();
    let f = Fixture::at(
        runtime.path().join("runtime/injected_frames.json"),
        81480610,
        true,
    );
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    let row = b2b_row(&f);
    supply_register(&f, &shared, key, Some(actor)).await;
    assert!(
        crate::services::discord::turn_finalizer::native_prepare::holds_row(&shared, &row).await
    );
    let evidence = proof(&f, Some("native answer"), Disposition::Executed);
    let api = Recorder::start().await;
    crate::services::discord::tmux::tmux_watcher::exercise_fresh_idle_with_row(
        &shared,
        &api.http,
        &row,
        evidence.range.1,
    )
    .await;
    assert!(
        on_disk(row.channel_id).is_some(),
        "fresh idle must preserve the native row before record"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_watcher_dispatch_fallback_preserves_work_types_pg() {
    if !api_child(concat!(
        module_path!(),
        "::native_8148_watcher_dispatch_fallback_preserves_work_types_pg"
    )) {
        return;
    }
    let work = WorkFixture::new().await;
    let fixture = &work.completion;
    let pool = &fixture.pool;
    let row = b2b_row(&fixture.input(81480611));
    let shared = fixture.shared(1);
    for (kind, id, held, blocked, completes) in [
        ("implementation", "implementation", false, false, true),
        ("rework", "rework", false, false, true),
        ("plan", "plan", false, false, false),
        ("plan-review", "plan-review", false, false, false),
        ("implementation", "held-without-id", true, false, true),
        ("implementation", "held-with-id", true, true, false),
    ] {
        let mut row = row.clone();
        row.dispatch_id = blocked.then(|| id.to_owned());
        let api = Recorder::start_with_status(Arc::new(move |_, uri| match uri.path() {
            "/api/internal/pending-dispatch-for-thread" => {
                Some((StatusCode::OK, json!({"dispatch_id":id})))
            }
            "/api/internal/card-thread" => Some((StatusCode::OK, json!({"dispatch_type":kind}))),
            _ => None,
        }))
        .await;
        crate::services::discord::internal_api::init(api.port, None);
        seed(pool, id, kind).await;
        assert!(crate::services::discord::adk_session::parse_dispatch_id(&row.user_text).is_none());
        let observed =
            inflight::o_seed_observation::Guard::new(&ProviderKind::Claude, row.channel_id);
        assert_eq!(
            watcher(&shared, &row, held, "watcher fallback").await,
            (Some(id.into()), true),
            "{id}"
        );
        let progress = observed.snapshot();
        assert_eq!(
            progress.event_count("watcher_dispatch_call"),
            usize::from(!blocked),
            "{id}"
        );
        assert_eq!(
            progress.event_count("watcher_dispatch_non_work"),
            usize::from(!completes && !blocked),
            "{id}"
        );
        let calls = api.take();
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("GET /api/internal/pending-dispatch-for-thread "))
        );
        assert_eq!(
            calls
                .iter()
                .any(|call| call.starts_with("GET /api/internal/card-thread ")),
            !blocked,
            "{id}"
        );
        assert_eq!(state(pool, id).await.3, i64::from(completes), "{id}");
    }
    work.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_dispatch_snapshot_failure_settles_policy_and_cleans_row_pg() {
    if !api_child(concat!(
        module_path!(),
        "::native_8148_dispatch_snapshot_failure_settles_policy_and_cleans_row_pg"
    )) {
        return;
    }
    let work = WorkFixture::new().await;
    let fixture = &work.completion;
    let pool = &fixture.pool;
    seed(pool, WORK, "implementation").await;
    let api = Recorder::start_with_status(Arc::new(|method, uri| {
        (method == axum::http::Method::GET && uri.path() == "/api/dispatches/native-work")
            .then(|| (StatusCode::SERVICE_UNAVAILABLE, json!({})))
    }))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    let f = fixture.input(81480612);
    let mut row = b2b_row(&f);
    row.dispatch_id = Some(WORK.into());
    inflight::save_inflight_state(&row).unwrap();
    let evidence = proof(&f, Some("native answer"), Disposition::Executed);
    durable(&f, &evidence);
    let shared = fixture.shared(2);
    fixture.restore(&shared).await;
    assert_eq!(state(pool, WORK).await.0, "failed");
    let failures: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dispatch_events WHERE dispatch_id=$1 AND transition_source='turn_bridge_dispatch_failure'",
    ).bind(WORK).fetch_one(pool).await.unwrap();
    assert_eq!(failures, 1);
    assert!(transcript(pool, &turn(&row)).await.is_some());
    assert!(on_disk(row.channel_id).is_none());
    assert!(cleanup_complete(&f));
    assert!(
        api.take()
            .iter()
            .any(|call| call.starts_with("GET /api/dispatches/native-work "))
    );
    work.close().await;
}
