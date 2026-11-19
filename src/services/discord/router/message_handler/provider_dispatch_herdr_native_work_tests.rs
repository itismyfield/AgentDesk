use super::*;
use crate::services::discord::admin_host_guard::tests::api_child;
use crate::services::discord::inflight::o_seed_observation;
use tokio::task::JoinHandle;

const WORK: &str = "native-http-work";
const PROBE: &str = "native-completion-watcher-offset";

fn fixture_commit(path: &Path) -> String {
    let run = |args: &[&str]| {
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
        String::from_utf8(output.stdout).unwrap()
    };
    run(&["init"]);
    run(&["commit", "--allow-empty", "-m", "fixture"]);
    run(&["rev-parse", "HEAD"]).trim().to_owned()
}

async fn work_state(pool: &PgPool) -> (String, Option<String>, i64) {
    sqlx::query_as(
        "SELECT status, result::jsonb->>'completion_source',
         (SELECT count(*) FROM dispatch_events WHERE dispatch_id=$1 AND to_status='completed')
         FROM task_dispatches WHERE id=$1",
    )
    .bind(WORK)
    .fetch_one(pool)
    .await
    .unwrap()
}

struct Loopback {
    task: JoinHandle<()>,
    calls: Arc<Mutex<Vec<(axum::http::Method, String)>>>,
}

impl Loopback {
    async fn start(api: Router) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let api = Router::new()
            .nest("/api", api)
            .layer(axum::middleware::from_fn(
                move |mut request: axum::extract::Request, next: axum::middleware::Next| {
                    let observed = observed.clone();
                    async move {
                        let uri = request
                            .extensions()
                            .get::<axum::extract::OriginalUri>()
                            .map_or(request.uri(), |original| &original.0);
                        observed
                            .lock()
                            .unwrap()
                            .push((request.method().clone(), uri.path().to_owned()));
                        request
                            .extensions_mut()
                            .insert(crate::server::routes::auth::loopback_peer());
                        next.run(request).await
                    }
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        discord::internal_api::init(listener.local_addr().unwrap().port(), None);
        let task = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
        Self { task, calls }
    }

    fn work_calls(&self, method: axum::http::Method) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen, path)| *seen == method && path == "/api/dispatches/native-http-work")
            .count()
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn work_at_closer(has_id: bool) {
    use axum::http::Method;
    let fx = Fixture::new(
        if has_id {
            "native-work"
        } else {
            "native-no-work-id"
        },
        None,
    );
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let policies = tempfile::tempdir().unwrap();
    std::fs::write(
        policies.path().join("completion.js"),
        "agentdesk.registerPolicy({name:'NativeRecoveryProbe',onDispatchCompleted:function(){const key='native-recovery-hook';agentdesk.kv.set(key,Number(agentdesk.kv.get(key)||0)+1,0);}});",
    ).unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let sha = fixture_commit(checkout.path());
    let mut config = crate::config::Config::default();
    config.policies.dir = policies.path().to_path_buf();
    config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(fx.pool.clone())).unwrap();
    let intake = Intake::with_engine(&fx, &started, Some(engine));
    let start = std::fs::metadata(&started.path).unwrap().len();
    let heads = answer_on_enter(&fx, &intake, &started, true);
    let observed = o_seed_observation::Guard::new(&ProviderKind::Claude, CHANNEL);
    observed.register_source(&started.path);
    let fallback = FallbackCount::new();
    intake.runtime.block_on(async {
        let api = app(&fx, &intake).await;
        let loopback = Loopback::start(api.clone()).await;
        sqlx::query("INSERT INTO kanban_cards (id,title,status,assigned_agent_id) VALUES ($1,$1,'in_progress','busy-entry')")
            .bind(WORK).execute(&fx.pool).await.unwrap();
        sqlx::query("INSERT INTO task_dispatches (id,kanban_card_id,to_agent_id,dispatch_type,status,thread_id)
                     VALUES ($1,$1,'busy-entry','implementation','pending',$2)")
            .bind(WORK).bind(CHANNEL.to_string()).execute(&fx.pool).await.unwrap();
        let (status, response) = call_source(&api, "start", "native-work-origin", "api").await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let turn_id = response["turn_id"].as_str().unwrap();
        let mut row = tokio::time::timeout(LIMIT, async {
            loop {
                if heads.lock().unwrap().len() == 2
                    && std::fs::metadata(&started.path).unwrap().len() > start
                    && let Some(row) = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
                {
                    break row;
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("the original is keyed with a live row");
        assert_eq!(turn_id, format!("discord:{CHANNEL}:{}", row.user_msg_id));
        assert!(discord::turn_finalizer::native_prepare::holds_row(&intake.shared, &row).await);
        row.dispatch_id = has_id.then(|| WORK.into());
        row.user_text = format!("DISPATCH: {WORK} fixture");
        row.worktree_path = Some(checkout.path().display().to_string());
        row.scheduled = discord::scheduled_turn::ScheduledTurnMark::Scheduled {
            turn_id: turn_id.to_owned(),
            isolated: false,
            outcome: None,
        };
        inflight::save_inflight_state(&row).unwrap();
        use crate::services::scheduled_messages::turn_completion::tests::{launch_agent_delivery, stored_outcome};
        launch_agent_delivery(&fx.pool, "fixture-native-http", turn_id, false, "fail").await;

        // Hold the transcript transaction until the concurrent watcher has consumed the probe.
        let mut fence = crate::db::session_transcripts::begin_channel_clear_boundary_tx(&fx.pool).await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(4533, hashtext($1))")
            .bind(CHANNEL.to_string()).execute(&mut *fence).await.unwrap();
        let answer = format!("HTTP session answered 1\n{sha}");
        answer_with_body(&started.path, &started.session, &answer);
        let closer_end = std::fs::metadata(&started.path).unwrap().len();
        append(&started.path, &[json!({"type":"system","subtype":PROBE,"sessionId":started.session})]);
        let probe_end = std::fs::metadata(&started.path).unwrap().len();
        let read = tokio::time::timeout(LIMIT, async {
            loop {
                let progress = observed.snapshot();
                if progress.decoded_chunks.concat().contains(PROBE) {
                    break progress;
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("the real watcher decodes the record after the closer");
        assert_eq!(read.source_reads, [(closer_end, probe_end)]);
        assert_eq!(read.event_count("watcher_dispatch_call"), 0);
        let http = intake.shared.serenity_http_or_token_fallback().expect("fixture Discord proxy");
        tokio::time::timeout(
            LIMIT,
            Box::pin(discord::recovery_engine::restore_inflight_turns(&http, &intake.shared, &ProviderKind::Claude)),
        ).await.expect("recovery inspects the actual native closer");
        assert_eq!(work_state(&fx.pool).await, ("pending".into(), None, 0));
        assert_eq!(stored_outcome(&fx.pool, turn_id).await, None);
        let transcripts: i64 = sqlx::query_scalar("SELECT count(*) FROM session_transcripts WHERE turn_id=$1")
            .bind(turn_id).fetch_one(&fx.pool).await.unwrap();
        assert_eq!(transcripts, 0, "recovery cannot record while native custody holds the turn");
        let hook_count = || sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key='native-recovery-hook'")
            .fetch_optional(&fx.pool);
        assert_eq!(hook_count().await.unwrap(), None);
        assert!(inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).is_some());
        assert!(!discord::terminal_delivery_custody::retains_channel(CHANNEL).unwrap());
        fence.rollback().await.unwrap();
        completed(&intake, 1).await;
        tokio::time::timeout(LIMIT, async {
            while observed.snapshot().event_count("bridge_native_exit") == 0 {
                tokio::task::yield_now().await;
            }
        }).await.expect("the native bridge reaches its neutral exit");
        assert_eq!(observed.snapshot().event_count("watcher_dispatch_call"), 0);
        assert!(!discord::terminal_delivery_custody::retains_channel(CHANNEL).unwrap());
        assert_eq!(stored_outcome(&fx.pool, turn_id).await.as_deref(), Some("delivered"));
        let bodies: Vec<String> = sqlx::query_scalar("SELECT assistant_message FROM session_transcripts WHERE turn_id=$1")
            .bind(turn_id).fetch_all(&fx.pool).await.unwrap();
        assert_eq!(bodies.iter().map(|body| body.trim()).collect::<Vec<_>>(), [answer.as_str()]);
        let state = work_state(&fx.pool).await;
        let expected = if has_id { ("completed", Some("turn_bridge_explicit"), 1) } else { ("pending", None, 0) };
        assert_eq!((state.0.as_str(), state.1.as_deref(), state.2), expected);
        assert_eq!(hook_count().await.unwrap().as_deref(), has_id.then_some("1"));
        assert_eq!(loopback.work_calls(Method::GET), usize::from(has_id));
        assert_eq!(loopback.work_calls(Method::PATCH), 0);
        fallback.assert_none();
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn native_8148_bound_herdr_watcher_reads_after_closer_and_leaves_work_to_native_pg() {
    if api_child(concat!(
        module_path!(),
        "::native_8148_bound_herdr_watcher_reads_after_closer_and_leaves_work_to_native_pg"
    )) {
        work_at_closer(true);
        work_at_closer(false);
    }
}
