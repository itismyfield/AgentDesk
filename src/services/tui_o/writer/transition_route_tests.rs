//! The operator route's Inspect sent to a real actor over a real store: what reaches the actor,
//! what it writes, which reply the route consumes and what it reports of the execution.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::StatusCode;
use serde_json::{Value, json};

use super::*;
use crate::server::routes::o_transition::{inspect, test_hooks as route};
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
    tokio::spawn(async move { inspect(&ready, CHANNEL, context, request).await })
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
// judge eligible, and reports the execution the judgment read with the request's nonce.
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

// A reply that comes back after the channel's run was replaced is answered as stale and never
// reported, whatever it says.
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
    let (next, _receiver) = TransitionPort::new(1, Arc::clone(&gate), Arc::new(SystemTime::now));
    route::on_reply(move || {
        drop(replaced.borrow_mut().take());
        *replaced.borrow_mut() = Some(kept.host(&again, &gate, &next));
    });
    let (status, answer) = routed(route(&ready, chain.context(), chain.request(JOB))).await;
    assert!(hosted.borrow().is_some(), "[R4:replaced]");
    assert_eq!(status, StatusCode::CONFLICT, "[R4:stale] {answer}");
    assert_eq!(answer["result"], "stale_actor_reply", "[R4:stale]");
    assert!(answer.get("reply").is_none(), "[R4:not_reported] {answer}");
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

    fn app(
        registry: Arc<crate::services::discord::health::HealthRegistry>,
        pool: sqlx::PgPool,
    ) -> axum::Router {
        let mut engine_config = crate::config::Config::default();
        engine_config.policies.hot_reload = false;
        let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
        let tx = crate::server::ws::new_broadcast();
        let buffer = crate::server::ws::spawn_batch_flusher(tx.clone());
        let config = crate::config::Config::default();
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
            let app = app(registry, pool.clone());
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

    // Through the router and a real PG row, Inspect judges the execution the hook judgment folded,
    // and reports that history and every unpinned session an old source may be, not a later one.
    #[test]
    fn transition_route_inspect_reports_the_folded_execution_it_judged_pg() {
        use crate::services::claude_tui::hook_server::adoption_retry::retry_deferred_claude_adoptions;
        use crate::services::tui_o::writer::tests::actor::host_start::{ProducerPane, session_row};
        use crate::services::tui_prompt_dedupe::binding_events::claude_history;
        let threaded = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        threaded.block_on(async {
            let [a, u1, u2, u3, u4] = [(); 5].map(|_| uuid::Uuid::new_v4().to_string());
            let hops = [&u1, &u2, &u3];
            let hop = |n: usize| {
                let session = hops[n - 1].clone();
                let body = [session_row(&session), row(&format!("n{n}"), "one")].concat();
                (session, body)
            };
            let chain = chain_named(3, true, hop);
            let path = |session: &str| chain.paths[0].with_file_name(format!("{session}.jsonl"));
            std::fs::write(path(&a), session_row(&a)).unwrap();
            let pane = Rc::new(ProducerPane::launch(&a, &path(&a)));
            let marker = crate::services::tmux_common::session_temp_path(pane.tmux, "spawn_nonce");
            let nonce = std::fs::read_to_string(&marker).unwrap().trim().to_owned();
            let root = crate::config::runtime_root().unwrap().display().to_string();
            let launch = BindingContext {
                schema: 1,
                provider: "claude".into(),
                created_at: Utc::now(),
                execution_nonce: nonce.clone(),
                tmux_session: pane.tmux.into(),
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
            // A cleared session's transcript appears with its first prompt: two clears are outlived
            // unprompted, the third is prompted and pinned, and a fourth still awaits its prompt.
            let aside = |session: &str| path(session).with_extension("aside");
            for session in hops {
                std::fs::rename(path(session), aside(session)).unwrap();
            }
            for (n, session) in hops.into_iter().enumerate() {
                let at = 10 * (n as i64 + 1);
                pane.http(
                    "SessionStart",
                    Some("clear"),
                    &a,
                    session,
                    &path(session),
                    at,
                );
            }
            std::fs::rename(aside(&u3), path(&u3)).unwrap();
            pane.http("UserPromptSubmit", None, &a, &u3, &path(&u3), 31);
            for session in [&u1, &u2] {
                std::fs::rename(aside(session), path(session)).unwrap();
            }
            // The outlived Pendings, written at last, resolve nothing once the rotation is delivered.
            crate::services::tui_prompt_dedupe::clear_claude_session_rotation(pane.tmux);
            retry_deferred_claude_adoptions();
            pane.http("SessionStart", Some("clear"), &a, &u4, &path(&u4), 40);
            let actor = run(&chain);
            settle(&chain).await;
            let rig = HerdrRig::start();
            let _endpoints = rig.registry_on_this_thread();
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let (shared, registry) = runtime(&pool).await;
            let owner = HostedOwner {
                provider: "claude".into(),
                discord_token_hash: shared.token_hash.clone(),
                channel_id: CHANNEL.to_string(),
                logical_key: pane.tmux.into(),
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
            let app = app(registry, pool.clone());
            let request = chain.request(JOB);
            let Some(Termination::Confirmed(end)) = &request.termination else {
                unreachable!()
            };
            let body = json!({
                "operation": "inspect", "channel": CHANNEL, "session_key": KEY_OF_ROW,
                "execution_nonce": nonce, "cutoff": request.cutoff, "job_id": JOB,
                "operator": "operator", "reason": "herdr transition",
                "termination": {"Confirmed": end},
            });
            let (later, u4_path) = (Rc::clone(&pane), path(&u4));
            let (launched, prompted) = (a.clone(), u4.clone());
            route::on_reply(move || {
                std::fs::write(&u4_path, session_row(&prompted)).unwrap();
                later.http("UserPromptSubmit", None, &launched, &prompted, &u4_path, 41);
            });
            let (before, sent) = (files(&chain.dir()), sends());
            let (status, answer) = post(&app, &body).await;
            let (_, moved) = claude_history(CHANNEL, pane.tmux, Some(&nonce)).unwrap();
            let moved = moved.current.map(|visit| visit.session);
            assert_eq!(moved, Some(u4.clone()), "[F:moved_before_consumed]");
            assert_eq!(
                (status, answer["result"].clone()),
                (StatusCode::OK, json!("inspected")),
                "[F:own] {answer}"
            );
            assert_eq!(sends() - sent, 1, "[F:sent_once]");
            let execution = answer["reply"]["execution"]["Err"]
                .as_str()
                .unwrap_or_default();
            assert!(
                execution.contains("has no pinned source"),
                "[F:judged_unpinned] {answer}"
            );
            assert_eq!(verdicts(&answer), ["Unknown"; 3], "[F:no_eligible]");
            assert_eq!(files(&chain.dir()), before, "[F:writes_nothing]");
            let diagnostics = &answer["reply"]["diagnostics"];
            assert_eq!(diagnostics["nonce"], json!(nonce), "[F:nonce]");
            let pin = pinned(&chain.sources[3]);
            let mut left = [&a, &u1, &u2].map(|session| json!(["left", session, null]));
            left.sort_by_key(|left| left[1].to_string());
            let mut sessions = vec![json!(["current", u3, pin]), json!(["awaiting", u4, null])];
            sessions.extend(left);
            let history = json!({"complete": true, "pinned": pin, "sessions": sessions});
            assert_eq!(diagnostics["history"]["Ok"], history, "[F:same_history]");
            let listed =
                |n: usize| json!({"key": chain.key(n), "held_by": [], "unpinned": [hops[n - 1]]});
            let mut expected = vec![listed(1), listed(2)];
            expected.sort_by_key(|overlap| overlap["key"].to_string());
            let mut overlaps = diagnostics["overlaps"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            overlaps.sort_by_key(|overlap| overlap["key"].to_string());
            assert_eq!(overlaps, expected, "[F:every_unpinned]");
            drop(hosted);
            halt(actor.stop, actor.task).await;
            pool.close().await;
            db.drop().await;
        });
    }
}
