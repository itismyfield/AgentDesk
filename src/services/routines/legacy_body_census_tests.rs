use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::routines::{
    RoutineDiscordLogger, RoutineScriptLoader, run_due_tick, store::NewRoutine,
};
use axum::{
    Router,
    response::IntoResponse,
    routing::{get, post},
};
use std::sync::{Arc, Mutex};

const CUT: i64 = 240_000;

fn observation(host: &str, case: &str) -> Value {
    let ids = [
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    ];
    let anchor = json!({"boot_id":ids[0],"started_at_ms":60_000});
    let proof = json!({"schema_version":1,"complete":true,"manifest_error":null,"pending":0,"census_hash":"b".repeat(64)});
    let mut files = Vec::new();
    for i in 0usize..2 {
        let mut targets = vec![];
        if i == 1 && case == "body" {
            targets.push(json!({"kind":"target","phase":"begin","op":"POST","at_ms":CUT,
                "census":{"site":"W11","binding_mode":"TUI","body_owner":"Legacy","actual_runtime":"codex_tui",
                "source_incarnation":{"file":1,"generation":1},"output_class":"LegacyBody"}}));
        }
        let na = case == "na";
        files.push(json!({"header":{"schema_version":1,"observation_policy_version":1,"process_role":"server",
            "host":host,"boot_id":ids[i],"doctor_boot_id":format!("doctor-{i}"),"started_at_ms":60_000*(i+1),
            "predecessor_boot_id":i.checked_sub(1).map(|n| &ids[n]),"coverage_start":anchor,
            "build_sha":"a".repeat(40),"census_hash":"b".repeat(64),"static_instrumentation":proof,"pending_probes":0},
            "window":{"cut":1,"drained":1,"invalid":false,"boot_id":ids[i],"closed":i==0,"status":"COMPLETE"},
            "end":if i==0 {json!({"outputs_drained":true,"ended_at_ms":119_999})} else {Value::Null},
            "selection":{"installed":true,"tui_bindings":if na {vec![]} else {vec![1]},
                "effective":if na {vec![]} else {vec![1]},"dormant":[]},"targets":targets,
            "buckets":if i==1 {vec![json!({"complete":true,"bucket_start_ms":120_000,"bucket_end_ms":180_000,
                "tui_turns":if na {0} else if case=="quiet" {9} else {10},"o_ack":if na {0} else {1},"opportunities":{"W11":1}})]} else {vec![]}}));
    }
    json!({"schema_version":1,"policy_version":1,"process_role":"server","host":host,
        "build_sha":"a".repeat(40),"census_hash":"b".repeat(64),"static_instrumentation":proof,
        "cut_ms":CUT,"served_at_ms":CUT,"monotonic_age_ms":0,"coverage_start":anchor,
        "evidence_truncated":false,"errors":[],"current_boot_id":ids[1],
        "live_journal":{"boot_id":ids[1],"enqueued_cut":1,"invalid":false},"boot_files":files,
        "inventory_source":{"source":"doctor/startup","version":1,"available":true,"from_ms":60_000,"to_ms":CUT},
        "doctor_starts":[{"doctor_boot_id":"doctor-0","started_at_ms":60_000},{"doctor_boot_id":"doctor-1","started_at_ms":120_000}]})
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn server(router: Router) -> (String, Server, std::net::SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), Server(task), address)
}

#[tokio::test(flavor = "current_thread")]
async fn due_census_tick_collects_both_hosts_and_posts_one_verdict_pg() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        256 * 1024,
    );
    for (case, expected) in [
        ("valid", "PASS"),
        ("body", "FAIL"),
        ("quiet", "NO_TRAFFIC"),
        ("na", "NO_TUI_BINDING"),
        ("missing", "INCONCLUSIVE"),
        ("empty", "INCONCLUSIVE"),
        ("non_object", "INCONCLUSIVE"),
        ("auth", "INCONCLUSIVE"),
        ("truncated", "INCONCLUSIVE"),
        ("wrong_host", "INCONCLUSIVE"),
        ("wrong_host_body", "FAIL"),
        ("timeout", "INCONCLUSIVE"),
        ("publish_failed", "PASS"),
        ("wrong_build", "INCONCLUSIVE"),
        ("wrong_census", "FAIL"),
        ("stale", "INCONCLUSIVE"),
        ("oversized", "INCONCLUSIVE"),
        ("redirect", "INCONCLUSIVE"),
        ("ordinary", "INCONCLUSIVE"),
    ] {
        sqlx::query("UPDATE routines SET next_due_at = NOW() + INTERVAL '7 days'")
            .execute(&pool)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = Arc::new(Mutex::new(Vec::<bool>::new()));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some("fixture-control-token".into());
        let mut servers = vec![];
        let mut origins = vec![];
        sqlx::query("DELETE FROM worker_nodes")
            .execute(&pool)
            .await
            .unwrap();
        for (host, id) in HOSTS {
            let mut data = observation(
                host,
                if case == "wrong_host_body" && host == "mac-book" {
                    "body"
                } else {
                    case
                },
            );
            if host == "mac-book" {
                match case {
                    "wrong_build" => data["build_sha"] = json!("c".repeat(40)),
                    "wrong_census" => data["census_hash"] = json!("c".repeat(64)),
                    "stale" => {
                        data["cut_ms"] = json!(CUT - 120_001);
                        data["served_at_ms"] = json!(CUT - 120_001);
                    }
                    _ => {}
                }
            }
            if matches!(case, "wrong_host" | "wrong_host_body") && host == "mac-book" {
                data["host"] = json!("mac-mini");
            }
            let path = root.path().join(format!("{host}.json"));
            std::fs::write(&path, json!({"legacy_body_census":data}).to_string()).unwrap();
            let (request_log, posted, both) = (requests.clone(), received.clone(), barrier.clone());
            let router = Router::new()
                .route(
                    "/api/health/detail",
                    get(move |headers: axum::http::HeaderMap| {
                        let (request_log, path, both) =
                            (request_log.clone(), path.clone(), both.clone());
                        async move {
                            request_log.lock().unwrap().push(
                                headers
                                    .get("authorization")
                                    .is_some_and(|v| v == "Bearer fixture-control-token"),
                            );
                            if case != "missing" {
                                both.wait().await;
                            }
                            if host == "mac-book" {
                                match case {
                                    "auth" => {
                                        return axum::http::StatusCode::UNAUTHORIZED
                                            .into_response();
                                    }
                                    "empty" => return axum::Json(json!({})).into_response(),
                                    "non_object" => {
                                        return axum::Json(json!({"legacy_body_census":"invalid"}))
                                            .into_response();
                                    }
                                    "oversized" => {
                                        return "x".repeat(MAX_RESPONSE_BYTES + 1).into_response();
                                    }
                                    "redirect" => {
                                        return (
                                            axum::http::StatusCode::FOUND,
                                            [("location", "/api/health/detail?redirected")],
                                        )
                                            .into_response();
                                    }
                                    "truncated" => {
                                        return ([("content-type", "application/json")], "{")
                                            .into_response();
                                    }
                                    "timeout" => tokio::time::sleep(Duration::from_secs(11)).await,
                                    _ => {}
                                }
                            }
                            let value: Value =
                                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                            axum::Json(value).into_response()
                        }
                    }),
                )
                .route(
                    "/channels/{channel}/messages",
                    post(
                        move |axum::extract::Path(channel): axum::extract::Path<String>,
                              axum::Json(body): axum::Json<Value>| {
                            let posted = posted.clone();
                            async move {
                                posted
                                    .lock()
                                    .unwrap()
                                    .push(json!({"channel":channel,"body":body}));
                                if case == "publish_failed" {
                                    return axum::http::StatusCode::FORBIDDEN.into_response();
                                }
                                axum::Json(json!({"id":"987654321"})).into_response()
                            }
                        },
                    ),
                );
            let (origin, server, _address) = server(router).await;
            servers.push(server);
            config.cluster.nodes.insert(
                id.into(),
                crate::config::ClusterNodeConfig {
                    trusted_forward_origin: Some(origin.clone()),
                    allow_private_forwarding: true,
                    allow_insecure_http_forwarding: true,
                    ..Default::default()
                },
            );
            if !(case == "missing" && host == "mac-book") {
                sqlx::query("INSERT INTO worker_nodes(instance_id,hostname,status,capabilities,last_heartbeat_at) VALUES ($1,$2,'online',$3,NOW())")
                    .bind(id).bind(host).bind(json!({"agentdesk_api":{"base_url":origin,"node_probe_v1":true}})).execute(&pool).await.unwrap();
            }
            origins.push(origin);
        }
        crate::runtime_layout::ensure_credential_layout(root.path()).unwrap();
        std::fs::write(
            crate::runtime_layout::credential_token_path(root.path(), "notify"),
            "fixture-discord-token",
        )
        .unwrap();
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let _api_env = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_DISCORD_API_BASE_URL",
            std::ffi::OsStr::new(&origins[0]),
        );
        let directory = root.path().join("monitoring");
        std::fs::create_dir(&directory).unwrap();
        let script_ref = if case == "ordinary" {
            "monitoring/ordinary.js"
        } else {
            SCRIPT_REF
        };
        let script = root.path().join(script_ref);
        std::fs::write(
            &script,
            include_str!("../../../monitoring/legacy-body-census.js"),
        )
        .unwrap();
        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(
            loader.load_script(root.path(), &script).unwrap(),
            script_ref
        );
        let routine = store
            .attach_routine(NewRoutine {
                agent_id: None,
                fallback_agent_id: None,
                max_retries: Some(0),
                script_ref: script_ref.into(),
                name: case.into(),
                status: None,
                execution_strategy: "persistent".into(),
                schedule: Some("0 9 * * *".into()),
                next_due_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
                checkpoint: None,
                discord_thread_id: Some("76510001".into()),
                timeout_secs: None,
            })
            .await
            .unwrap();
        let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);
        let context = session_forwarding::ForwardCallerContext {
            pg_pool: Some(pool.clone()),
            config: Arc::new(config),
            cluster_instance_id: Some("mac-mini-release".into()),
        };
        let outcomes = TEST_INPUT
            .scope(
                (
                    context,
                    json!({"build_sha":"a".repeat(40),"census_hash":"b".repeat(64)}),
                    CUT,
                ),
                run_due_tick(
                    &store,
                    &loader,
                    &[root.path().to_path_buf()],
                    None,
                    if case == "ordinary" {
                        None
                    } else {
                        Some(&logger)
                    },
                    1,
                    true,
                ),
            )
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 1, "{case}");
        assert_eq!(outcomes[0].status, "succeeded", "{case}: {:?}", outcomes[0]);
        let result = outcomes[0].result_json.as_ref().unwrap();
        assert_eq!(result["stage1"], expected, "{case}: {result}");
        let persisted: (String,Option<chrono::DateTime<chrono::Utc>>,Option<Value>) = sqlx::query_as("SELECT status,next_due_at,(SELECT result_json FROM routine_runs WHERE id=$2) FROM routines WHERE id=$1")
            .bind(&routine.id).bind(&outcomes[0].run_id).fetch_one(&pool).await.unwrap();
        assert_eq!(persisted.0, "enabled", "{case}");
        assert!(persisted.1.is_some(), "{case}");
        assert_eq!(persisted.2.as_ref().unwrap()["stage1"], expected);
        if case == "ordinary" {
            assert_eq!(result["reasons"], json!(["REPORT_OBSERVATION_MISSING"]));
            assert!(received.lock().unwrap().is_empty());
            assert!(requests.lock().unwrap().is_empty());
            continue;
        }
        let posts = received.lock().unwrap();
        assert_eq!(posts.len(), 1, "{case}: {posts:?}");
        assert_eq!(posts[0]["channel"], "76510001");
        let content = posts[0]["body"]["content"].as_str().unwrap();
        assert_eq!(content.lines().count(), 1, "{case}: {content}");
        assert!(content.contains(expected), "{case}: {content}");
        assert!(
            !content.contains("72h")
                && !content.contains("fixture-")
                && !result.to_string().contains("fixture-")
        );
        let failure = store
            .get_run_discord_log_failure(&outcomes[0].run_id)
            .await
            .unwrap();
        if case == "publish_failed" {
            assert!(failure.is_some(), "posting failure must persist");
        } else {
            assert!(failure.is_none(), "{case}: {failure:?}");
        }
        assert_eq!(
            requests.lock().unwrap().len(),
            if case == "missing" { 1 } else { 2 },
            "{case}"
        );
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|authorized| *authorized)
        );
    }
    assert!(applies(SCRIPT_REF));
    for ordinary in [
        "other.js",
        "./monitoring/legacy-body-census.js",
        "monitoring/legacy-body-census.js?extra",
    ] {
        assert!(!applies(ordinary));
    }
    pool.close().await;
    db.drop().await;
}
