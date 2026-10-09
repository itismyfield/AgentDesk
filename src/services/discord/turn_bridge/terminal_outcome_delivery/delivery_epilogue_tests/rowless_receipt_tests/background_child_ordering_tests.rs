//! Real tool registration, transcript collection, and terminal HTTP write races.

use super::*;
use crate::services::discord::turn_bridge::{
    BridgeCompletionSignal, TurnBridgeContext, spawn_turn_bridge,
};
use crate::services::dispatched_sessions::{HookSessionBody, HookSessionQuery};

const JOB: &str = "toolu_ordered_job";
const BG_INPUT: &str = r#"{"command":"sleep 600","run_in_background":true}"#;
const INSERT_GATE: i64 = 6_691_301;
type TerminalBodyRx = tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>;

fn isolated(name: &str, env: &str) -> bool {
    if std::env::var_os(env).is_some() {
        return false;
    }
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{}::{name}", module_path!().split_once("::").unwrap().1),
            "--nocapture",
        ])
        .env(env, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
    true
}

fn append_records(path: &std::path::Path, records: &[serde_json::Value]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
}

fn native_launch() -> serde_json::Value {
    serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":[{
        "type":"tool_use", "id":JOB, "name":"Bash", "input":{
            "command":"sleep 600", "run_in_background":true}}]}})
}

fn native_completion() -> serde_json::Value {
    native_completion_for(JOB)
}

fn native_completion_for(tool_use_id: &str) -> serde_json::Value {
    let notification = format!(
        "<task-notification>\n<task-id>ordered-job</task-id>\n\
        <tool-use-id>{tool_use_id}</tool-use-id>\n<status>completed</status>\n\
        <summary>Background command completed</summary>\n</task-notification>"
    );
    serde_json::json!({"type":"user", "isMeta":true,
        "message":{"role":"user", "content":notification}})
}

async fn bind_turn(
    driver: &mut TerminalDeliveryDriver,
    pool: &sqlx::PgPool,
    case: &str,
) -> (String, String, std::path::PathBuf) {
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
    let tmux = format!("AgentDesk-claude-bgchild-order-{case}");
    let path = driver._temp.path().join(format!("{case}.jsonl"));
    std::fs::write(&path, "").unwrap();
    let parent = crate::services::discord::adk_session::build_namespaced_session_key(
        &driver.shared.token_hash,
        &ProviderKind::Claude,
        &tmux,
    );
    driver.inflight.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    driver.inflight.tmux_session_name = Some(tmux.clone());
    driver.inflight.output_path = Some(path.display().to_string());
    driver.inflight.turn_start_offset = Some(0);
    driver.inflight.last_offset = 0;
    inflight::save_inflight_state(&driver.inflight).unwrap();
    sqlx::query("INSERT INTO sessions (session_key, provider, status, active_turn_nonce) VALUES ($1, 'claude', 'turn_active', $2)")
        .bind(&parent).bind(driver.inflight.turn_nonce.as_deref())
        .execute(pool).await.unwrap();
    tui_prompt_dedupe::register_tmux_runtime_binding(
        &tmux,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    if let Some((channel, mut watcher)) = driver
        .shared
        .tmux_watchers
        .remove(&ChannelId::new(DRIVER_CHANNEL_ID))
    {
        watcher.tmux_session_name = tmux.clone();
        watcher.output_path = path.display().to_string();
        driver.shared.tmux_watchers.insert(channel, watcher);
    }
    (parent, tmux, path)
}

async fn hook_api(
    pool: &sqlx::PgPool,
    pause_terminal: bool,
) -> (
    TerminalBodyRx,
    Arc<tokio::sync::Semaphore>,
    tokio::task::JoinHandle<()>,
) {
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
    let (capture, captured) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let app = axum::Router::new()
        .route(
            "/api/dispatched-sessions/webhook",
            axum::routing::post(move |axum::extract::Query(query): axum::extract::Query<HookSessionQuery>, axum::Json(body): axum::Json<HookSessionBody>| {
                let state = state.clone();
                let capture = capture.clone();
                let gate = gate.clone();
                async move {
                    if matches!(body.status.as_deref(), Some("idle" | "awaiting_bg")) {
                        capture.send(serde_json::json!({ "body": &body, "expected_turn_nonce": &query.expected_turn_nonce })).unwrap();
                        if pause_terminal {
                            gate.acquire().await.unwrap().forget();
                        }
                    }
                    crate::server::routes::dispatched_sessions::hook_session(
                        axum::extract::State(state),
                        axum::extract::Query(query),
                        axum::Json(body),
                    )
                    .await
                }
            }),
        )
        .fallback(|| async { axum::Json(serde_json::json!({})) });
    let loopback = crate::config::loopback();
    let listener = tokio::net::TcpListener::bind((loopback.as_str(), 0))
        .await
        .unwrap();
    crate::services::discord::internal_api::init(
        listener.local_addr().unwrap().port(),
        Some(pool.clone()),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (captured, release, server)
}

async fn start_bridge(
    driver: &TerminalDeliveryDriver,
    parent: &str,
) -> (
    std::sync::mpsc::Sender<StreamMessage>,
    tokio::sync::oneshot::Receiver<BridgeCompletionSignal>,
) {
    let cancel = Arc::new(CancelToken::new());
    let channel = ChannelId::new(DRIVER_CHANNEL_ID);
    let user = MessageId::new(DRIVER_USER_MSG_ID);
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            &driver.shared,
            channel,
            cancel.clone(),
            UserId::new(1),
            user,
        )
        .await
    );
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let ctx = TurnBridgeContext {
        provider: ProviderKind::Claude,
        gateway: driver.gateway.clone(),
        channel_id: channel,
        user_msg_id: Some(user),
        user_text_owned: "start the job".into(),
        request_owner_name: String::new(),
        role_binding: None,
        adk_session_key: Some(parent.to_string()),
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: Some(MessageId::new(DRIVER_CURRENT_MSG_ID)),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: false,
        inflight_state: driver.inflight.clone(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(StreamMessage::ToolUse {
        name: "Bash".into(),
        input: BG_INPUT.into(),
        tool_use_id: Some(JOB.into()),
    })
    .unwrap();
    tx.send(StreamMessage::ToolResult {
        content: "Command running in background with ID: order".into(),
        is_error: false,
        tool_use_id: Some(JOB.into()),
    })
    .unwrap();
    tx.send(StreamMessage::Text {
        content: "job started".into(),
    })
    .unwrap();
    spawn_turn_bridge(driver.shared.clone(), cancel, rx, ctx);
    (tx, completion_rx)
}

fn send_done(tx: &std::sync::mpsc::Sender<StreamMessage>) {
    tx.send(StreamMessage::Done {
        result: String::new(),
        session_id: None,
    })
    .unwrap();
}

async fn done(rx: tokio::sync::oneshot::Receiver<BridgeCompletionSignal>) {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx)
        .await
        .expect("real bridge terminal postlude finishes")
        .expect("completion signal");
}

async fn terminal_body(rx: &mut TerminalBodyRx) -> serde_json::Value {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("real terminal HTTP POST is reached")
        .expect("terminal body captured")
}

async fn collector(driver: &TerminalDeliveryDriver) {
    crate::services::discord::tui_prompt_relay::background_child_completion::claude_bindings_after_child_completions(
        &driver.shared,
    ).await;
}

async fn wait_for_child(pool: &sqlx::PgPool, parent: &str, closed: bool) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let found: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions child JOIN sessions parent ON parent.id = child.parent_session_id WHERE parent.session_key = $1 AND (child.closed_at IS NOT NULL) = $2)")
                .bind(parent).bind(closed).fetch_one(pool).await.unwrap();
            if found { break; }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.expect("actual bridge tool arm commits the required child state");
}

async fn child_is_closed(pool: &sqlx::PgPool, parent: &str) -> bool {
    sqlx::query_scalar(
        "SELECT closed_at IS NOT NULL FROM sessions WHERE session_key = $1 || ':child:' || $2",
    )
    .bind(parent)
    .bind(JOB)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn wait_for_stream_text() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let saved =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, DRIVER_CHANNEL_ID);
            if saved.is_some_and(|state| {
                state.has_post_tool_text && state.full_response.contains("job started")
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a frame after registration is durably processed before completion lands");
}

async fn parent_state(pool: &sqlx::PgPool, parent: &str) -> (i32, String, bool) {
    let (children, status) =
        sqlx::query_as("SELECT active_children, status FROM sessions WHERE session_key = $1")
            .bind(parent)
            .fetch_one(pool)
            .await
            .unwrap();
    let unoccupied =
        crate::services::tmux_turn_liveness::idle_cleanup_session_is_unoccupied(pool, parent).await;
    (children, status, unoccupied)
}

#[test]
fn bridge_child_completion_before_insert_commit_reconciles_both_cursors_pg() {
    if isolated(
        "bridge_child_completion_before_insert_commit_reconciles_both_cursors_pg",
        "ADK_CHILD_INSERT_ORDER_TEST",
    ) {
        return;
    }
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let cases: &[(bool, bool, bool, bool)] = match std::env::var("ADK_CHILD_CURSOR_CASE").as_deref() {
            Ok("first") => &[(false, false, false, false)],
            Ok("initialized") => &[(true, false, false, false)],
            Ok("retry") => &[(true, true, false, false)],
            Ok("partial") => &[(false, false, true, false)],
            Ok("read") => &[(true, false, false, true)],
            Err(_) => &[(true, false, false, false), (false, false, false, false), (true, true, false, false), (false, false, true, false), (true, false, false, true)],
            Ok(case) => panic!("unknown cursor case: {case}"),
        };
        for &(initialized, retry_close, partial_record, read_fault) in cases {
            let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
            let db = crate::dispatch::test_support::DispatchPostgresTestDb::create("bg_insert_order", "completion before actual child INSERT").await;
            let pool = db.connect_and_migrate_with_max_connections(6).await;
            let case = if read_fault { "read-fault" } else if partial_record { "partial-first" } else if retry_close { "retry-close" } else if initialized { "initialized" } else { "first" };
            let (parent, tmux, path) = bind_turn(&mut driver, &pool, case).await;
            let (mut captured, release, server) = hook_api(&pool, retry_close).await;
            let _gateway = crate::services::discord::turn_bridge::runtime_handoff_loop::test_gateway::connect();
            let _rest = crate::services::discord::shared_state::test_rest::recording_mock(4_710_000, DRIVER_CHANNEL_ID).await;
            if initialized { collector(&driver).await; }
            append_records(&path, &[native_launch()]);
            let partial_tail = if partial_record {
                let mut record = serde_json::to_vec(&native_completion()).unwrap();
                record.push(b'\n');
                let split = record.len() / 2;
                std::fs::OpenOptions::new().append(true).open(&path).unwrap()
                    .write_all(&record[..split]).unwrap();
                Some(record[split..].to_vec())
            } else { append_records(&path, &[native_completion()]); None };
            let mut sibling_blocker = None;
            if retry_close {
                let sibling = crate::db::session_observability::insert_background_child_pg(&pool, &crate::db::session_observability::BackgroundChildSpawn {
                    parent_session_key: parent.clone(), provider: Some("claude".into()),
                    tool_name: "Bash".into(), tool_input: BG_INPUT.into(),
                    tool_use_id: Some("toolu_sibling".into()),
                }).await.unwrap().unwrap();
                append_records(&path, &[native_completion_for("toolu_sibling")]);
                let mut locked = pool.begin().await.unwrap();
                sqlx::query("SELECT id FROM sessions WHERE id = $1 FOR UPDATE")
                    .bind(sibling).fetch_one(&mut *locked).await.unwrap();
                sibling_blocker = Some(locked);
            }
            sqlx::query(&format!("CREATE FUNCTION pause_child_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.parent_session_id IS NOT NULL THEN PERFORM pg_advisory_xact_lock({INSERT_GATE}); END IF; RETURN NEW; END $$"))
                .execute(&pool).await.unwrap();
            sqlx::query("CREATE TRIGGER pause_child_insert BEFORE INSERT ON sessions FOR EACH ROW EXECUTE FUNCTION pause_child_insert()")
                .execute(&pool).await.unwrap();
            if retry_close {
                sqlx::query("CREATE SEQUENCE child_close_attempts").execute(&pool).await.unwrap();
                sqlx::query("CREATE FUNCTION fail_child_close() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.session_key LIKE '%:child:toolu_ordered_job' AND NEW.closed_at IS NOT NULL THEN PERFORM nextval('child_close_attempts'); RAISE EXCEPTION 'fixture child close unavailable'; END IF; RETURN NEW; END $$")
                    .execute(&pool).await.unwrap();
                sqlx::query("CREATE TRIGGER fail_child_close BEFORE UPDATE ON sessions FOR EACH ROW EXECUTE FUNCTION fail_child_close()")
                    .execute(&pool).await.unwrap();
            }
            let mut blocker = pool.begin().await.unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(INSERT_GATE).execute(&mut *blocker).await.unwrap();
            let (tx, rx) = start_bridge(&driver, &parent).await;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event = 'advisory' AND query LIKE 'INSERT INTO sessions (%')")
                        .fetch_one(&pool).await.unwrap();
                    if waiting { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }).await.expect("actual child INSERT waits inside its PG transaction");
            let scan = if retry_close {
                let shared = driver.shared.clone();
                let scan = tokio::spawn(async move {
                    crate::services::discord::tui_prompt_relay::background_child_completion::claude_bindings_after_child_completions(&shared).await;
                });
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE 'SELECT parent_session_id, closed_at%')")
                            .fetch_one(&pool).await.unwrap();
                        if waiting { break; }
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }).await.expect("collector has consumed fast None and waits on the sibling row");
                Some(scan)
            } else { collector(&driver).await; None };
            assert_eq!(parent_state(&pool, &parent).await.0, i32::from(retry_close), "completion scan precedes INSERT commit");
            let unread_path = path.with_extension("read-fault.jsonl");
            if read_fault { std::fs::rename(&path, &unread_path).unwrap(); }
            blocker.commit().await.unwrap();
            wait_for_stream_text().await;
            assert_eq!(child_is_closed(&pool, &parent).await, !retry_close && !partial_record && !read_fault, "registration closes only a complete readable terminal record before the later stream frame");
            if read_fault {
                assert_eq!(parent_state(&pool, &parent).await.0, 1);
                std::fs::rename(&unread_path, &path).unwrap();
                collector(&driver).await;
                assert!(child_is_closed(&pool, &parent).await, "restored transcript retries registration's read error without rewriting the record");
                assert_eq!(parent_state(&pool, &parent).await.0, 0);
            }
            if let Some(tail) = partial_tail {
                assert_eq!(parent_state(&pool, &parent).await.0, 1);
                std::fs::OpenOptions::new().append(true).open(&path).unwrap()
                    .write_all(&tail).unwrap();
                collector(&driver).await;
                assert!(child_is_closed(&pool, &parent).await, "the completed partial record remains collectible after registration");
                assert_eq!(parent_state(&pool, &parent).await.0, 0);
            }
            send_done(&tx);
            let body = if retry_close {
                let body = terminal_body(&mut captured).await;
                let attempted: bool = sqlx::query_scalar("SELECT is_called FROM child_close_attempts")
                    .fetch_one(&pool).await.unwrap();
                assert!(attempted, "registration reconciliation attempted the failing PG close");
                assert_eq!(parent_state(&pool, &parent).await.0, 2);
                sibling_blocker.take().unwrap().commit().await.unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(10), scan.unwrap())
                    .await.expect("old collector finishes after registration rewinds").unwrap();
                assert_eq!(parent_state(&pool, &parent).await.0, 1);
                sqlx::query("DROP TRIGGER fail_child_close ON sessions").execute(&pool).await.unwrap();
                collector(&driver).await;
                assert!(child_is_closed(&pool, &parent).await, "the next collector tick retries registration's failed close");
                release.add_permits(1);
                body
            } else {
                terminal_body(&mut captured).await
            };
            done(rx).await;
            drop(tx);
            assert_eq!(body["expected_turn_nonce"].as_str(), driver.inflight.turn_nonce.as_deref());
            assert!(body["body"]["turn_start_nonce"].is_null());
            assert_eq!(parent_state(&pool, &parent).await, (0, "idle".into(), true));
            collector(&driver).await;
            assert_eq!(parent_state(&pool, &parent).await, (0, "idle".into(), true), "no retransmission or cursor reset is needed");
            tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux);
            server.abort();
            pool.close().await;
            db.drop().await;
        }
    });
}

async fn admit_successor(pool: &sqlx::PgPool, parent: &str) {
    use crate::db::dispatched_session_canonical_identity::{
        capture_hook_session_actor_pin_pg, upsert_hook_session_with_actor_pin_pg,
    };
    let pin = capture_hook_session_actor_pin_pg(pool, parent)
        .await
        .unwrap();
    upsert_hook_session_with_actor_pin_pg(
        pool,
        crate::db::dispatched_sessions::HookSessionUpsert {
            session_key: parent,
            instance_id: None,
            agent_id: None,
            provider: "claude",
            status: "turn_active",
            session_info: None,
            model: None,
            tokens: None,
            cwd: None,
            active_dispatch_id: None,
            thread_channel_id: None,
            channel_id: None,
            claude_session_id: None,
            raw_provider_session_id: None,
            turn_start_nonce: Some("successor-turn"),
            dispatched_origin: false,
        },
        &pin,
    )
    .await
    .unwrap();
    crate::db::session_observability::insert_background_child_pg(
        pool,
        &crate::db::session_observability::BackgroundChildSpawn {
            parent_session_key: parent.into(),
            provider: Some("claude".into()),
            tool_name: "Bash".into(),
            tool_input: BG_INPUT.into(),
            tool_use_id: Some("toolu_successor_live".into()),
        },
    )
    .await
    .unwrap()
    .expect("successor child is registered");
}

#[test]
fn bridge_child_close_and_terminal_http_write_converge_in_all_orders_pg() {
    if isolated(
        "bridge_child_close_and_terminal_http_write_converge_in_all_orders_pg",
        "ADK_CHILD_FINAL_ORDER_TEST",
    ) {
        return;
    }
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let cases: &[&str] = match std::env::var("ADK_CHILD_TERMINAL_CASE").as_deref() {
            Ok("before-done") => &["before-done"],
            Ok("before-write") => &["before-write"],
            Ok("after-write") => &["after-write"],
            Ok("successor") => &["successor"],
            Err(_) => &["before-done", "before-write", "after-write", "successor"],
            Ok(case) => panic!("unknown terminal case: {case}"),
        };
        for &case in cases {
            let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
            let db = crate::dispatch::test_support::DispatchPostgresTestDb::create("bg_final_order", "child close versus actual terminal HTTP PG write").await;
            let pool = db.connect_and_migrate_with_max_connections(6).await;
            let (parent, tmux, path) = bind_turn(&mut driver, &pool, case).await;
            let (mut captured, release, server) = hook_api(&pool, true).await;
            let _gateway = crate::services::discord::turn_bridge::runtime_handoff_loop::test_gateway::connect();
            let _rest = crate::services::discord::shared_state::test_rest::recording_mock(4_720_000, DRIVER_CHANNEL_ID).await;
            append_records(&path, &[native_launch()]);
            let (tx, rx) = start_bridge(&driver, &parent).await;
            wait_for_child(&pool, &parent, false).await;
            wait_for_stream_text().await;
            if case == "before-done" {
                append_records(&path, &[native_completion()]);
                collector(&driver).await;
                assert_eq!(parent_state(&pool, &parent).await, (0, "turn_active".into(), false));
            }
            send_done(&tx);
            let body = terminal_body(&mut captured).await;
            assert_eq!(body["body"]["status"], "awaiting_bg", "captured bridge Vec still contains the child");
            assert_eq!(body["expected_turn_nonce"].as_str(), driver.inflight.turn_nonce.as_deref());
            assert!(body["body"]["turn_start_nonce"].is_null());
            if matches!(case, "before-write" | "successor") {
                append_records(&path, &[native_completion()]);
                collector(&driver).await;
                assert_eq!(parent_state(&pool, &parent).await, (0, "turn_active".into(), false));
                if case == "successor" { admit_successor(&pool, &parent).await; }
            }
            release.add_permits(1);
            done(rx).await;
            drop(tx);
            if case == "after-write" {
                assert_eq!(parent_state(&pool, &parent).await, (1, "awaiting_bg".into(), false));
                append_records(&path, &[native_completion()]);
                collector(&driver).await;
            }
            if case == "successor" {
                assert_eq!(parent_state(&pool, &parent).await, (1, "turn_active".into(), false));
                let nonce: String = sqlx::query_scalar("SELECT active_turn_nonce FROM sessions WHERE session_key = $1")
                    .bind(&parent).fetch_one(&pool).await.unwrap();
                assert_eq!(nonce, "successor-turn", "stale real terminal POST preserves the new actor");
                let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions child JOIN sessions parent ON parent.id = child.parent_session_id WHERE parent.session_key = $1 AND child.closed_at IS NULL")
                    .bind(&parent).fetch_one(&pool).await.unwrap();
                assert_eq!(live, 1);
            } else {
                assert_eq!(parent_state(&pool, &parent).await, (0, "idle".into(), true), "{case}");
            }
            tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux);
            server.abort();
            pool.close().await;
            db.drop().await;
        }
    });
}
