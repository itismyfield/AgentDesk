//! The operator route's Inspect sent to a real actor over a real store: what reaches the actor,
//! what it writes, which reply the route consumes and what it reports of the execution.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::StatusCode;
use serde_json::{Value, json};

use super::*;
use crate::server::routes::o_transition::{send as routed_send, test_hooks as route};
use crate::services::claude_tui::source_verify::Left;
use crate::services::cluster::channel_home::register_for_test;
use crate::services::discord::health::o_transition::home_admission;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::writer::host::Readiness;

/// The watches readiness reads, the caller's to move.
struct Watches {
    resumed: watch::Sender<bool>,
    stopping: watch::Sender<bool>,
}

impl Watches {
    fn new(resumed: bool, stopping: bool) -> Self {
        let (resumed, stopping) = (watch::channel(resumed).0, watch::channel(stopping).0);
        Self { resumed, stopping }
    }

    fn host(
        &self,
        ready: &Arc<Readiness>,
        gate: &Arc<OwnershipGate>,
        port: &TransitionPort,
    ) -> impl Drop + use<> {
        let watched = (self.resumed.subscribe(), self.stopping.subscribe());
        ready.host_port_for_test(CHANNEL, gate, port.clone(), watched)
    }
}

type Routed = tokio::task::JoinHandle<(StatusCode, Json<Value>)>;

fn route(ready: &Arc<Readiness>, context: Context, request: Request) -> Routed {
    let ready = Arc::clone(ready);
    tokio::spawn(
        async move { routed_send(&ready, CHANNEL, context, Operation::Inspect(request)).await },
    )
}

/// Runs the actor, letting each probe worker finish, until the route answers.
async fn routed(asked: Routed) -> (StatusCode, Value) {
    for _ in 0..300 {
        if asked.is_finished() {
            break;
        }
        polls(1).await;
        hooks::settle_workers();
    }
    let (status, Json(answer)) = asked.await.unwrap();
    (status, answer)
}

fn sends() -> usize {
    route::SENDS.with(std::cell::Cell::get)
}

fn verdicts(answer: &Value) -> Vec<String> {
    let hops = answer["reply"]["hops"].as_array().unwrap();
    let verdict = |hop: &Value| match &hop["judged"]["verdict"] {
        Value::Object(map) => map.keys().next().unwrap().clone(),
        other => other.to_string(),
    };
    hops.iter().map(verdict).collect()
}

fn pinned(source: &SourceId) -> Value {
    serde_json::to_value(source).unwrap()
}

/// The pane's execution: the current source pinned, and `left` sessions it moved on from unpinned.
fn moved_from(current: &SourceId, left: &[&SourceId]) -> impl Drop + use<> {
    let (pin, mut history) = execution(current, Utc::now());
    let unpinned = |source: &&SourceId| {
        let left = Left {
            pin: None,
            left_at: Utc::now(),
        };
        (source.session_id.clone(), left)
    };
    history.left = left.iter().map(unpinned).collect::<BTreeMap<_, _>>();
    force_execution_for_tests(Ok((pin, history)))
}

// Inspect through the route reaches the actor once, writes no store file though both old hops
// judge eligible, and reports the execution the judgment read with the request's nonce and the
// identity and pane of each old source.
#[tokio::test(start_paused = true)]
async fn transition_route_inspect_writes_nothing_and_reports_the_execution_it_judged() {
    let chain = chain(2);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let (ready, watches) = (Arc::new(Readiness::default()), Watches::new(true, false));
    let _hosted = watches.host(&ready, &chain.harness.gate, &actor.port);
    let (before, sent) = (files(&chain.dir()), sends());
    let asked = route(&ready, chain.context(), chain.request(JOB));
    let (status, answer) = routed(asked).await;
    assert_eq!(
        (status, &answer["result"]),
        (StatusCode::OK, &json!("inspected")),
        "{answer}"
    );
    assert_eq!(sends() - sent, 1, "[R1:sent_once]");
    assert_eq!(files(&chain.dir()), before, "[R1:writes_nothing]");
    assert_eq!(
        verdicts(&answer),
        ["Eligible", "Eligible"],
        "[R1:would_retire]"
    );
    let diagnostics = &answer["reply"]["diagnostics"];
    assert_eq!(diagnostics["nonce"], "nonce", "[R1:nonce]");
    let current = chain.sources.last().unwrap();
    let sessions = json!([["current", current.session_id, pinned(current)]]);
    assert_eq!(
        diagnostics["history"]["Ok"]["sessions"], sessions,
        "[R1:history]"
    );
    let named = |n: usize| {
        let source = &chain.sources[n];
        json!({"key": chain.key(n), "source": pinned(source), "pane": chain.pane})
    };
    let mut candidates = diagnostics["candidates"].as_array().unwrap().clone();
    candidates.sort_by_key(|candidate| candidate["key"].to_string());
    let mut expected = vec![named(0), named(1)];
    expected.sort_by_key(|candidate| candidate["key"].to_string());
    assert_eq!(candidates, expected, "[R1:candidates]");
    halt(actor.stop, actor.task).await;
}

// Every old source an unpinned left session may be is listed, though the execution's error names
// only the first; no hop judges eligible and nothing is written.
#[tokio::test(start_paused = true)]
async fn transition_route_inspect_lists_every_unpinned_session_a_source_may_be() {
    let chain = chain(2);
    let (old, current) = (&chain.sources[..2], &chain.sources[2]);
    let _moved = moved_from(current, &[&old[0], &old[1]]);
    let actor = run(&chain);
    settle(&chain).await;
    let (ready, watches) = (Arc::new(Readiness::default()), Watches::new(true, false));
    let _hosted = watches.host(&ready, &chain.harness.gate, &actor.port);
    let before = files(&chain.dir());
    let asked = route(&ready, chain.context(), chain.request(JOB));
    let (status, answer) = routed(asked).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let execution = &answer["reply"]["execution"];
    assert!(
        execution["Err"].is_string(),
        "[R2:execution_error] {execution}"
    );
    assert_eq!(
        verdicts(&answer),
        ["Unknown", "Unknown"],
        "[R2:no_eligible]"
    );
    let mut overlaps = answer["reply"]["diagnostics"]["overlaps"]
        .as_array()
        .unwrap()
        .clone();
    overlaps.sort_by_key(|overlap| overlap["key"].to_string());
    let listed =
        |n: usize| json!({"key": chain.key(n), "held_by": [], "unpinned": [old[n].session_id]});
    let mut expected = vec![listed(0), listed(1)];
    expected.sort_by_key(|overlap| overlap["key"].to_string());
    assert_eq!(overlaps, expected, "[R2:every_unpinned]");
    assert_eq!(files(&chain.dir()), before, "[R2:writes_nothing]");
    halt(actor.stop, actor.task).await;
}

// The route reports the history the judgment read, not the one the pane moved to before the
// reply was consumed.
#[tokio::test(start_paused = true)]
async fn transition_route_inspect_reports_the_history_of_the_judgment_not_a_later_one() {
    let chain = chain(1);
    let current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let (ready, watches) = (Arc::new(Readiness::default()), Watches::new(true, false));
    let _hosted = watches.host(&ready, &chain.harness.gate, &actor.port);
    let moved = Rc::new(RefCell::new(None));
    let (later, old) = (Rc::clone(&moved), chain.sources[0].clone());
    route::on_reply(move || {
        *later.borrow_mut() = Some(force_execution_for_tests(Ok(execution(&old, Utc::now()))))
    });
    let (status, answer) = routed(route(&ready, chain.context(), chain.request(JOB))).await;
    assert!(moved.borrow().is_some(), "[R3:moved_before_consumed]");
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(verdicts(&answer), ["Eligible"], "[R3:judged]");
    let judged = chain.sources.last().unwrap();
    let sessions = json!([["current", judged.session_id, pinned(judged)]]);
    let history = &answer["reply"]["diagnostics"]["history"]["Ok"]["sessions"];
    assert_eq!(*history, sessions, "[R3:same_snapshot]");
    drop(moved.borrow_mut().take());
    drop(current);
    halt(actor.stop, actor.task).await;
}

// A reply that comes back after the channel's run was replaced, or stopped taking work, is
// answered as stale and never reported or shown as unsent, whatever it says.
#[tokio::test(start_paused = true)]
async fn transition_route_consumes_no_reply_of_a_run_replaced_meanwhile() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let (ready, watches) = (
        Arc::new(Readiness::default()),
        Rc::new(Watches::new(true, false)),
    );
    let hosted = watches.host(&ready, &chain.harness.gate, &actor.port);
    let hosted = Rc::new(RefCell::new(Some(hosted)));
    let (replaced, again, gate) = (
        Rc::clone(&hosted),
        Arc::clone(&ready),
        Arc::clone(&chain.harness.gate),
    );
    let kept = Rc::clone(&watches);
    let wall = Arc::new(SystemTime::now);
    let (next, _receiver) =
        TransitionPort::new(1, Arc::clone(&gate), wall, watch::channel(false).1);
    route::on_reply(move || {
        drop(replaced.borrow_mut().take());
        *replaced.borrow_mut() = Some(kept.host(&again, &gate, &next));
    });
    let (status, answer) = routed(route(&ready, chain.context(), chain.request(JOB))).await;
    assert!(hosted.borrow().is_some(), "[R4:replaced]");
    assert_eq!(status, StatusCode::CONFLICT, "[R4:stale] {answer}");
    assert_eq!(answer["result"], "stale_actor_reply", "[R4:stale]");
    assert!(answer.get("reply").is_none(), "[R4:not_reported] {answer}");
    let (ready, watches) = (
        Arc::new(Readiness::default()),
        Rc::new(Watches::new(true, false)),
    );
    let _hosted = watches.host(&ready, &chain.harness.gate, &actor.port);
    let stopping = Rc::clone(&watches);
    route::on_reply(move || stopping.stopping.send_modify(|stop| *stop = true));
    let (status, answer) = routed(route(&ready, chain.context(), chain.request(JOB))).await;
    let stale = (status, &answer["result"]);
    let expected = (StatusCode::CONFLICT, &json!("stale_actor_reply"));
    assert_eq!(stale, expected, "[R4:stopped_after_send] {answer}");
    halt(actor.stop, actor.task).await;
}

// A channel whose actor is not hosted, is stopping, is not resumed, has lost its resume watch or
// is not owned gets no request: nothing is sent and the actor reads nothing.
#[tokio::test(start_paused = true)]
async fn transition_route_sends_nothing_to_an_actor_that_takes_no_work() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let (owned, unowned) = (&chain.harness.gate, &Arc::new(OwnershipGate::default()));
    let cases = [
        ("unhosted", None),
        ("stopping", Some((true, true, owned))),
        ("unresumed", Some((false, false, owned))),
        ("resume_gone", Some((true, false, owned))),
        ("unowned", Some((true, false, unowned))),
    ];
    for (case, hosting) in cases {
        let ready = Arc::new(Readiness::default());
        let (mut hosted, mut kept) = (None, None);
        if let Some((resumed, stopping, gate)) = hosting {
            let watches = Watches::new(resumed, stopping);
            hosted = Some(watches.host(&ready, gate, &actor.port));
            kept = (case != "resume_gone").then_some(watches);
        }
        let (sent, read) = (sends(), hooks::snapshots());
        let (status, answer) = routed(route(&ready, chain.context(), chain.request(JOB))).await;
        let refused = (status, answer["result"].clone());
        assert_eq!(
            refused,
            (StatusCode::CONFLICT, json!("actor_unavailable")),
            "[R5:{case}]"
        );
        let reached = (sends() - sent, hooks::snapshots() - read);
        assert_eq!(reached, (0, 0), "[R5:{case}] sent, read");
        drop((hosted, kept));
    }
    halt(actor.stop, actor.task).await;
}

// A job the actor took keeps the home's command permit after the HTTP requester went away, until
// the job itself ends.
#[tokio::test(start_paused = true)]
async fn transition_route_job_keeps_the_home_permit_after_the_requester_left() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let home = register_for_test(CHANNEL, Some(HomeState::Worker));
    let (home_epoch, permit) = home_admission(CHANNEL).unwrap();
    assert_eq!(home.commands_in_flight(), 1, "[R6:admitted]");
    let context = Context {
        home_epoch,
        permit,
        ..chain.context()
    };
    let (ready, watches) = (Arc::new(Readiness::default()), Watches::new(true, false));
    let _hosted = watches.host(&ready, &chain.harness.gate, &actor.port);
    let base = hooks::started();
    let asked = route(&ready, context, chain.request(JOB));
    for _ in 0..5 {
        polls(1).await;
        hooks::settle_workers();
        if hooks::started() > base {
            break;
        }
    }
    assert_eq!(hooks::started() - base, 1, "[R6:taken]");
    asked.abort();
    polls(1).await;
    assert_eq!(home.commands_in_flight(), 1, "[R6:requester_left]");
    for _ in 0..40 {
        polls(1).await;
        hooks::settle_workers();
    }
    assert_eq!(home.commands_in_flight(), 0, "[R6:released_at_end]");
    halt(actor.stop, actor.task).await;
}

#[cfg(unix)]
mod pg {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::dispatched_sessions::hosted_execution::tests::record;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLocation, HostedOwner, HostedState,
    };
    use crate::services::discord::host_teardown_gate::test_support::runtime;
    use crate::services::session_host::herdr_socket_rig_tests::{
        HerdrRig, KEY, NODE, PANE, SESSION,
    };
    use crate::services::tui_o::writer::host::process_readiness;
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    use tower::ServiceExt;

    const KEY_OF_ROW: &str = "claude/hrot-c1/mac-mini:row";

    use crate::config::Config;

    fn app(
        registry: Arc<crate::services::discord::health::HealthRegistry>,
        pool: sqlx::PgPool,
        port: u16,
    ) -> axum::Router {
        let mut engine_config = crate::config::Config::default();
        engine_config.policies.hot_reload = false;
        let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
        let tx = crate::server::ws::new_broadcast();
        let buffer = crate::server::ws::spawn_batch_flusher(tx.clone());
        let mut config = crate::config::Config::default();
        config.server.port = port;
        crate::server::routes::api_router_with_pg(
            engine,
            config,
            tx,
            buffer,
            Some(registry),
            Some(pool),
        )
    }

    async fn post(app: &axum::Router, body: &Value) -> (StatusCode, Value) {
        let origin = format!(
            "http://127.0.0.1:{}",
            crate::config::Config::default().server.port
        );
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/o/transition");
        let request = request
            .header("content-type", "application/json")
            .header("origin", origin);
        let mut request = request
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let peer: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn store(pool: &sqlx::PgPool, owner: &HostedOwner, record: &Value) {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution)
             VALUES ($1, $2, 'idle', 'discord_channel', $3, $4, $5)
             ON CONFLICT (session_key) DO UPDATE SET provider = $2, discord_token_hash = $3, channel_id = $4, hosted_execution = $5",
        )
        .bind(KEY_OF_ROW)
        .bind(&owner.provider)
        .bind(&owner.discord_token_hash)
        .bind(&owner.channel_id)
        .bind(record)
        .execute(pool)
        .await
        .unwrap();
    }

    // Through the router and a real PG row: a row that is not the target's own execution reaches no
    // actor; the row's own execution reaches the hosted actor once, and its Inspect writes nothing.
    #[test]
    fn transition_route_reaches_the_actor_only_for_the_rows_own_execution_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let threaded = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        threaded.block_on(async {
            let chain = chain(1);
            let _current = chain.current();
            let actor = run(&chain);
            settle(&chain).await;
            let rig = HerdrRig::start();
            let _endpoints = rig.registry_on_this_thread();
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let (shared, registry) = runtime(&pool).await;
            let root = crate::config::runtime_root().unwrap().display().to_string();
            let nonce = uuid::Uuid::new_v4().simple().to_string();
            let launch = BindingContext {
                schema: 1,
                provider: "claude".into(),
                created_at: Utc::now(),
                execution_nonce: nonce.clone(),
                tmux_session: chain.pane.clone(),
                channel_id: Some(CHANNEL),
                owner_runtime_root: root.clone(),
                host: None,
                expected_native_session_id: None,
                launch_mode: "fresh".into(),
                provider_root: None,
                first_prompt_digest: None,
                source_policy: None,
            };
            PreparedIncarnation::create(launch).unwrap();
            let marker =
                crate::services::tmux_common::session_temp_path(&chain.pane, "spawn_nonce");
            std::fs::write(&marker, &nonce).unwrap();
            let owner = HostedOwner {
                provider: "claude".into(),
                discord_token_hash: shared.token_hash.clone(),
                channel_id: CHANNEL.to_string(),
                logical_key: chain.pane.clone(),
                owner_node: NODE.into(),
                runtime_root: root,
            };
            let located = |owner: &HostedOwner, node: &str, key: &str| {
                let mut bound = record(owner, &nonce, HostedState::Bound);
                bound.location = Some(HostedLocation {
                    host: "herdr".into(),
                    execution_node: node.into(),
                    endpoint_config_key: key.into(),
                    socket_addr: rig.socket().display().to_string(),
                    named_session: SESSION.into(),
                    pane_id: PANE.into(),
                });
                serde_json::to_value(bound).unwrap()
            };
            let (ready, watches) = (process_readiness(), Watches::new(true, false));
            let hosted = watches.host(&ready, &chain.harness.gate, &actor.port);
            let app = app(registry, pool.clone(), Config::default().server.port);
            let request = chain.request(JOB);
            let ended = match &request.termination {
                Some(Termination::Confirmed(end)) => json!({"Confirmed": end}),
                _ => unreachable!(),
            };
            let body = |nonce: &str, channel: u64| {
                json!({
                    "operation": "inspect", "channel": channel, "session_key": KEY_OF_ROW,
                    "execution_nonce": nonce, "cutoff": request.cutoff, "job_id": JOB,
                    "operator": "operator", "reason": "herdr transition", "termination": ended,
                })
            };
            let with = |change: fn(&mut HostedOwner)| {
                let mut changed = owner.clone();
                change(&mut changed);
                changed
            };
            let codex = with(|o| o.provider = "codex".into());
            let other_node = with(|o| o.owner_node = "mac-book".into());
            let other_bot = with(|o| o.discord_token_hash = "another-bot".into());
            let other_root = with(|o| o.runtime_root = "/elsewhere".into());
            // The row and its launch name channel 7; `other_channel` names 8 for that same row.
            let cases = [
                ("missing_row", None, nonce.as_str(), CHANNEL),
                (
                    "codex",
                    Some((&codex, located(&codex, NODE, KEY))),
                    &nonce,
                    CHANNEL,
                ),
                (
                    "other_node",
                    Some((&other_node, located(&other_node, "mac-book", KEY))),
                    &nonce,
                    CHANNEL,
                ),
                (
                    "other_bot",
                    Some((&other_bot, located(&other_bot, NODE, KEY))),
                    &nonce,
                    CHANNEL,
                ),
                (
                    "other_root",
                    Some((&other_root, located(&other_root, NODE, KEY))),
                    &nonce,
                    CHANNEL,
                ),
                (
                    "unregistered_endpoint",
                    Some((&owner, located(&owner, NODE, "elsewhere"))),
                    &nonce,
                    CHANNEL,
                ),
                (
                    "other_nonce",
                    Some((&owner, located(&owner, NODE, KEY))),
                    "0123456789abcdef0123456789abcdef",
                    CHANNEL,
                ),
                (
                    "other_channel",
                    Some((&owner, located(&owner, NODE, KEY))),
                    &nonce,
                    CHANNEL + 1,
                ),
            ];
            let before = files(&chain.dir());
            for (case, row, asked, channel) in cases {
                if let Some((owner, record)) = row {
                    store(&pool, owner, &record).await;
                }
                let (sent, read) = (sends(), hooks::snapshots());
                let (status, answer) = post(&app, &body(asked, channel)).await;
                let refused = (status, answer["result"].clone());
                assert_eq!(
                    refused,
                    (StatusCode::CONFLICT, json!("refused")),
                    "[P1:{case}] {answer}"
                );
                assert_eq!(
                    (sends() - sent, hooks::snapshots() - read),
                    (0, 0),
                    "[P1:{case}] sent, read"
                );
                assert_eq!(files(&chain.dir()), before, "[P1:{case}] writes");
            }
            std::fs::write(&marker, "0123456789abcdef0123456789abcdef").unwrap();
            let (status, answer) = post(&app, &body(&nonce, CHANNEL)).await;
            assert_eq!(
                (status, answer["result"].clone()),
                (StatusCode::CONFLICT, json!("refused")),
                "[P1:other_marker]"
            );
            std::fs::write(&marker, &nonce).unwrap();
            let sent = sends();
            let (status, answer) = post(&app, &body(&nonce, CHANNEL)).await;
            assert_eq!(
                (status, answer["result"].clone()),
                (StatusCode::OK, json!("inspected")),
                "[P2:own] {answer}"
            );
            assert_eq!(sends() - sent, 1, "[P2:sent_once]");
            assert_eq!(verdicts(&answer), ["Eligible"], "[P2:would_retire]");
            assert_eq!(
                answer["reply"]["diagnostics"]["nonce"],
                json!(nonce),
                "[P2:nonce]"
            );
            assert_eq!(files(&chain.dir()), before, "[P2:writes_nothing]");
            let _ = std::fs::remove_file(&marker);
            drop(hosted);
            halt(actor.stop, actor.task).await;
            pool.close().await;
            db.drop().await;
        });
    }

    /// `agentdesk o …` as the binary parses it.
    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        o: crate::cli::o::OArgs,
    }

    async fn cli(args: Vec<String>) -> Result<(), String> {
        let ran = tokio::task::spawn_blocking(move || {
            let parsed = <Cli as clap::Parser>::try_parse_from(args).unwrap();
            crate::cli::o::run(parsed.o)
        });
        ran.await.unwrap()
    }

    // The CLI's Inspect manifest, applied over TCP through the router to a real PG row's actor,
    // retires the one hop its scope holds with its audit; the hop past it and the manifest stay.
    #[test]
    fn transition_cli_apply_retires_only_the_scope_its_inspect_approved_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        // The runtime root's guard holds the shared env lock this base is set under.
        let base = std::ffi::OsString::from(format!("http://127.0.0.1:{port}"));
        let _base = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_API_URL",
            &base,
        );
        let threaded = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        threaded.block_on(async {
            let chain = chain(2);
            let _current = chain.current();
            let actor = run(&chain);
            settle(&chain).await;
            let rig = HerdrRig::start();
            let _endpoints = rig.registry_on_this_thread();
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let (shared, registry) = runtime(&pool).await;
            let root = crate::config::runtime_root().unwrap().display().to_string();
            let nonce = uuid::Uuid::new_v4().simple().to_string();
            PreparedIncarnation::create(BindingContext {
                schema: 1,
                provider: "claude".into(),
                created_at: Utc::now(),
                execution_nonce: nonce.clone(),
                tmux_session: chain.pane.clone(),
                channel_id: Some(CHANNEL),
                owner_runtime_root: root.clone(),
                host: None,
                expected_native_session_id: None,
                launch_mode: "fresh".into(),
                provider_root: None,
                first_prompt_digest: None,
                source_policy: None,
            })
            .unwrap();
            let marker =
                crate::services::tmux_common::session_temp_path(&chain.pane, "spawn_nonce");
            std::fs::write(&marker, &nonce).unwrap();
            let owner = HostedOwner {
                provider: "claude".into(),
                discord_token_hash: shared.token_hash.clone(),
                channel_id: CHANNEL.to_string(),
                logical_key: chain.pane.clone(),
                owner_node: NODE.into(),
                runtime_root: root,
            };
            let mut bound = record(&owner, &nonce, HostedState::Bound);
            bound.location = Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: KEY.into(),
                socket_addr: rig.socket().display().to_string(),
                named_session: SESSION.into(),
                pane_id: PANE.into(),
            });
            store(&pool, &owner, &serde_json::to_value(bound).unwrap()).await;
            let (ready, watches) = (process_readiness(), Watches::new(true, false));
            let hosted = watches.host(&ready, &chain.harness.gate, &actor.port);
            // Nested under `/api` as the server's web surface mounts it.
            let service = axum::Router::new().nest("/api", app(registry, pool.clone(), port));
            let service = service.into_make_service_with_connect_info::<std::net::SocketAddr>();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let served = tokio::spawn(async move { axum::serve(listener, service).await });
            let dir = tempfile::tempdir().unwrap();
            let manifest = dir.path().join("manifest.json").display().to_string();
            let inspect = [
                "agentdesk",
                "transition",
                "inspect",
                "--channel",
                &CHANNEL.to_string(),
                "--session-key",
                KEY_OF_ROW,
                "--execution-nonce",
                &nonce,
                "--cutoff",
                "2",
                "--operator",
                "operator",
                "--reason",
                "herdr transition",
                "--job-id",
                JOB,
                "--ended",
                "mac-mini",
                &chain.pane,
                "2",
                "2",
                "the tmux pane is gone",
                "--manifest",
                &manifest,
            ];
            let inspect = inspect.map(str::to_owned).to_vec();
            assert_eq!(cli(inspect).await, Ok(()), "[PA:inspect]");
            let written = std::fs::read(&manifest).unwrap();
            let shown: Value = serde_json::from_slice(&written).unwrap();
            let hops = &shown["answer"]["reply"]["scope"]["Ok"]["hops"];
            let listed = (hops.as_array().map(Vec::len), &hops[0]["key"]);
            assert_eq!(
                listed,
                (Some(1), &json!(chain.key(0))),
                "[PA:manifest_scope]"
            );
            let (outside, before) = (chain.cursor_file(1), chain.cursor_file(0));
            let apply = [
                "agentdesk",
                "transition",
                "apply",
                "--manifest",
                &manifest,
                "--confirm",
            ];
            let apply = apply.map(str::to_owned).to_vec();
            assert_eq!(cli(apply).await, Ok(()), "[PA:apply]");
            assert_ne!(chain.cursor_file(0), before, "[PA:in_scope_written]");
            let retired = (chain.cursor(0).retired, chain.audit(0));
            assert_eq!(retired, (true, Some(JOB.to_string())), "[PA:in_scope]");
            assert_eq!(chain.cursor_file(1), outside, "[PA:out_of_scope]");
            assert_eq!(chain.audit(1), None, "[PA:out_of_scope]");
            assert_eq!(
                std::fs::read(&manifest).unwrap(),
                written,
                "[PA:manifest_kept]"
            );
            served.abort();
            let _ = std::fs::remove_file(&marker);
            drop(hosted);
            halt(actor.stop, actor.task).await;
            pool.close().await;
            db.drop().await;
        });
    }
}
