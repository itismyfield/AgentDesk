//! HTTP diagnostics and provider-auth routes on every stored host case, through their handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};

use super::AppState;
use crate::services::discord::host_defer_gate::tests::{Case, ScriptedTmux, postgres};
use crate::services::discord::host_teardown_gate::test_support::Stored;

fn state(pool: sqlx::PgPool) -> AppState {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).expect("policy engine");
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    AppState {
        pg_pool: Some(pool),
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    }
}

/// Seeds `case` as a working turn of `agent` on `host`, with a fresh heartbeat.
async fn seed_turn(
    pool: &sqlx::PgPool,
    case: Case,
    (agent, name): (&str, &str),
    host: &str,
    channel: u64,
) -> i64 {
    let key = format!("claude/p4c2/{host}:{name}");
    case.seed(pool, &key, name, channel).await;
    sqlx::query("INSERT INTO agents (id, name, provider) VALUES ($1, $1, 'claude')")
        .bind(agent)
        .execute(pool)
        .await
        .expect("seed agent");
    sqlx::query(
        "UPDATE sessions SET agent_id = $2, thread_channel_id = $3, status = 'turn_active',
                last_heartbeat = NOW(), instance_id = NULL
          WHERE session_key = $1 RETURNING id",
    )
    .bind(&key)
    .bind(agent)
    .bind(channel.to_string())
    .fetch_one(pool)
    .await
    .map(|row| sqlx::Row::get(&row, "id"))
    .expect("working session row")
}

fn naming<'a>(calls: &'a [String], name: &str) -> Vec<&'a String> {
    calls.iter().filter(|call| call.contains(name)).collect()
}

async fn row_status(pool: &sqlx::PgPool, id: i64) -> String {
    let status = sqlx::query_scalar("SELECT status FROM sessions WHERE id = $1").bind(id);
    status.fetch_one(pool).await.expect("session status")
}

// Diag, output, turn status and stop show a non-legacy session, local or remote, as unsupported
// with no tmux probe, capture or stop by its name; a legacy row reads and stops as in main.
#[tokio::test]
async fn diagnostics_routes_never_probe_a_non_legacy_session_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let state = state(pool.clone());
    let provider = crate::services::provider::ProviderKind::Claude;
    let local = crate::services::platform::hostname_short();
    let hosts = [local.as_str(), "p4c2-remote"];
    for (h, host) in hosts.into_iter().enumerate() {
        for (n, case) in Case::ALL.into_iter().enumerate() {
            if !case.has_row() {
                continue;
            }
            let legacy = case == Case::Stored(Stored::Legacy);
            let what = (host, case);
            let (agent, name) = (
                format!("p4c2-agent-{h}-{n}"),
                provider.build_tmux_session_name(&format!("p4c2-diag-{h}-{n}")),
            );
            let channel = 1_479_671_302_387_065_000 + (h * 10 + n) as u64;
            let id = seed_turn(&pool, case, (&agent, &name), host, channel).await;
            tmux.take_calls();

            let diag = super::agent_diag(State(state.clone()), Path(agent.clone())).await;
            let Ok((StatusCode::OK, Json(diag))) = diag else {
                panic!("{what:?}: diag failed");
            };
            let observed = |field: &str| diag[field]["state"].as_str().map(str::to_string);
            let unsupported = Some("host_unsupported".to_string());
            let adoption = observed("tmux_relay_adoption") == unsupported;
            assert_eq!(adoption, !legacy, "{what:?}: {diag}");
            let readiness = observed("tui_prompt_readiness") == unsupported;
            assert_eq!(readiness, !legacy, "{what:?}: {diag}");

            let query =
                Query(crate::services::dispatched_sessions::TmuxOutputQuery { lines: None });
            let output = super::super::dispatched_sessions::tmux_output;
            let (status, Json(output)) =
                output(State(state.clone()), HeaderMap::new(), Path(id), query).await;
            let expected = if legacy {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            assert_eq!(status, expected, "{what:?}: {output}");

            let turn = super::agent_turn(State(state.clone()), Path(agent.clone())).await;
            let Ok((StatusCode::OK, Json(turn))) = turn else {
                panic!("{what:?}: turn status failed");
            };
            let turn_unsupported = turn["host_unsupported"].is_string();
            assert_eq!(turn_unsupported, !legacy, "{what:?}: {turn}");

            let calls = tmux.take_calls();
            let named = naming(&calls, &name).is_empty();
            assert_eq!(named, !legacy, "{what:?}: {calls:?}");

            let (status, Json(stop)) =
                super::stop_agent_turn(State(state.clone()), Path(agent.clone())).await;
            let refused = stop["unsupported"].is_string();
            assert_eq!(refused, !legacy, "{what:?}: {status} {stop}");
            if !legacy {
                assert_eq!(status, StatusCode::CONFLICT, "{what:?}: {stop}");
                let row = row_status(&pool, id).await;
                assert_eq!(row, "turn_active", "{what:?}: the row is not marked");
                let calls = tmux.take_calls();
                assert!(naming(&calls, &name).is_empty(), "{what:?}: {calls:?}");

                // A stale heartbeat reads idle, still named unsupported rather than tmux-dead.
                let stale =
                    "UPDATE sessions SET last_heartbeat = NOW() - INTERVAL '1 hour' WHERE id = $1";
                sqlx::query(stale)
                    .bind(id)
                    .execute(&pool)
                    .await
                    .expect("stale");
                let turn = super::agent_turn(State(state.clone()), Path(agent.clone())).await;
                let Ok((StatusCode::OK, Json(turn))) = turn else {
                    panic!("{what:?}: idle turn status failed");
                };
                let idle = (
                    turn["status"].as_str(),
                    turn["host_unsupported"].is_string(),
                );
                assert_eq!(idle, (Some("idle"), true), "{what:?}: {turn}");
                // An idle stop names the host refusal too, not a missing active turn.
                let (status, Json(stop)) =
                    super::stop_agent_turn(State(state.clone()), Path(agent.clone())).await;
                let refused = (status, stop["unsupported"].as_str());
                let expected = (StatusCode::CONFLICT, Some("session_host_not_tmux"));
                assert_eq!(refused, expected, "{what:?}: {stop}");
                let row = row_status(&pool, id).await;
                assert_eq!(row, "turn_active", "{what:?}: the idle row is not marked");
                let calls = tmux.take_calls();
                assert!(naming(&calls, &name).is_empty(), "{what:?}: {calls:?}");
            }
        }
    }
    db.drop().await;
}

// A provider-auth login target another host's marker claims is refused before the profile
// home or org.yaml changes and before any tmux call.
#[tokio::test]
async fn auth_login_routes_refuse_a_target_another_host_claims() {
    use super::super::provider_auth_profiles::{LoginStartBody, login_start, remove_profile};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    // A regression must not reach the real profile root under the user's home.
    let scratch_home = tempfile::tempdir().expect("scratch home");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _home = set("HOME", scratch_home.path());
    let provider = crate::services::provider::ProviderKind::Claude;
    let profile = "p4c2-herdr-login";
    let session =
        crate::services::provider_auth_profile::login_tmux_session_name(&provider, profile);
    let marker = crate::services::tmux_common::session_temp_path(&session, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, "herdr").unwrap();
    let home = crate::services::provider_auth_profile::extra_account_home(&provider, profile);

    let body = Some(Json(LoginStartBody {
        profile_id: Some(profile.to_string()),
    }));
    let started = login_start(Path("claude".to_string()), body).await;
    let error = started.expect_err("the login start is refused");
    assert_eq!(error.status(), StatusCode::CONFLICT, "{error:?}");
    assert!(
        !home.expect("profile home").exists(),
        "no profile home is created"
    );

    let removed = remove_profile(Path(("claude".to_string(), profile.to_string()))).await;
    let error = removed.expect_err("the unlink is refused");
    assert_eq!(error.status(), StatusCode::CONFLICT, "{error:?}");
    assert_eq!(tmux.take_calls(), Vec::<String>::new(), "no tmux call");

    std::fs::remove_file(&marker).unwrap();
    let removed = remove_profile(Path(("claude".to_string(), profile.to_string()))).await;
    let error = removed.expect_err("main's unlink finds no such profile");
    assert_ne!(error.status(), StatusCode::CONFLICT, "{error:?}");
}

// A dispatch cancel and an agent stop of a runtime turn on another host answer a conflict and
// change no turn, session row or dispatch; a legacy turn's stop marks its row as in main.
#[tokio::test]
async fn stop_routes_keep_a_runtime_turn_on_another_host_pg() {
    use crate::services::discord::host_teardown_gate::test_support::{nameless_turn, runtime};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let (shared, registry) = runtime(&pool).await;
    let mut state = state(pool.clone());
    state.health_registry = Some(registry.clone());
    let provider = crate::services::provider::ProviderKind::Claude;
    // A remote row reads working by its heartbeat, so no tmux probe decides the route.
    let remote = "p6asb-remote";
    for (n, other_host) in [true, false].into_iter().enumerate() {
        let (agent, name) = (
            format!("p6asb-agent-{n}"),
            provider.build_tmux_session_name(&format!("p6asb-stop-{n}")),
        );
        let channel = 1_479_671_302_387_066_000 + n as u64;
        let legacy = Case::Stored(Stored::Legacy);
        let id = seed_turn(&pool, legacy, (&agent, &name), remote, channel).await;
        let dispatch = format!("p6asb-dispatch-{n}");
        sqlx::query("INSERT INTO task_dispatches (id, title, dispatch_type, status) VALUES ($1, 'p6asb', 'implementation', 'dispatched')")
            .bind(&dispatch)
            .execute(&pool)
            .await
            .expect("seed dispatch");
        sqlx::query(
            "UPDATE sessions SET provider = 'claude', active_dispatch_id = $2 WHERE id = $1",
        )
        .bind(id)
        .bind(&dispatch)
        .execute(&pool)
        .await
        .expect("bind the dispatch");
        let channel_id = poise::serenity_prelude::ChannelId::new(channel);
        let token = nameless_turn(&shared, channel_id).await;
        if other_host {
            // The runtime's turn moved to a session whose marker names another host.
            let moved = format!("{name}-moved");
            let marker = crate::services::tmux_common::session_temp_path(&moved, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(&marker, "herdr").unwrap();
            token.bind_claude_tmux_session(&moved);
        }

        let (status, Json(stop)) =
            super::stop_agent_turn(State(state.clone()), Path(agent.clone())).await;
        let what = format!("other_host={other_host}");
        let cancelled = || token.cancelled.load(std::sync::atomic::Ordering::SeqCst);
        if other_host {
            assert_eq!(status, StatusCode::CONFLICT, "{what}: {stop}");
            assert_eq!(
                stop["unsupported"], "session_host_not_tmux",
                "{what}: {stop}"
            );
            assert_eq!(row_status(&pool, id).await, "turn_active", "{what}");
            assert!(!cancelled(), "{what}: the turn runs on");
        } else {
            assert_eq!(status, StatusCode::OK, "{what}: {stop}");
            assert_eq!(row_status(&pool, id).await, "disconnected", "{what}");
            assert!(cancelled(), "{what}");
        }

        let service = crate::services::queue::QueueService::new(Some(pool.clone()));
        let cancel = service.cancel_dispatch(Some(&registry), &dispatch).await;
        let dispatch_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(&dispatch)
                .fetch_one(&pool)
                .await
                .expect("dispatch status");
        if other_host {
            let error = cancel.expect_err("the dispatch cancel is refused");
            assert_eq!(error.status(), StatusCode::CONFLICT, "{what}: {error:?}");
            assert_eq!(dispatch_status, "dispatched", "{what}");
            assert_eq!(row_status(&pool, id).await, "turn_active", "{what}");
            assert!(!cancelled(), "{what}: the turn runs on");
        } else {
            assert!(cancel.is_ok(), "{what}: {cancel:?}");
            assert_eq!(dispatch_status, "cancelled", "{what}");
        }
    }
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn command_agent_and_dispatch_refusal_leave_session_and_dispatch_working_pg() {
    use crate::db::o_channel_homes::HomeState;
    use crate::services::cluster::channel_home;
    use crate::services::discord::host_teardown_gate::test_support::{nameless_turn, runtime};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let (shared, registry) = runtime(&pool).await;
    let mut app = state(pool.clone());
    app.health_registry = Some(registry.clone());
    let channel = 9200000000000106;
    let name =
        crate::services::provider::ProviderKind::Claude.build_tmux_session_name("home-agent-stop");
    let id = seed_turn(
        &pool,
        Case::Stored(Stored::Legacy),
        ("home-agent-stop", &name),
        "remote-home",
        channel,
    )
    .await;
    let dispatch = "home-agent-stop-dispatch";
    sqlx::query("INSERT INTO task_dispatches (id,title,dispatch_type,status) VALUES ($1,'home-command','implementation','dispatched')")
        .bind(dispatch).execute(&pool).await.unwrap();
    sqlx::query("UPDATE sessions SET provider='claude',active_dispatch_id=$2 WHERE id=$1")
        .bind(id)
        .bind(dispatch)
        .execute(&pool)
        .await
        .unwrap();
    let token = nameless_turn(&shared, poise::serenity_prelude::ChannelId::new(channel)).await;
    let home = channel_home::register_for_test(channel, Some(HomeState::Releasing));
    tmux.take_calls();
    let (status, Json(body)) =
        super::stop_agent_turn(State(app), Path("home-agent-stop".into())).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("home_draining"),
        "{body}"
    );
    let error = crate::services::queue::QueueService::new(Some(pool.clone()))
        .cancel_dispatch(Some(&registry), dispatch)
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::CONFLICT);
    assert!(error.to_string().contains("home_draining"), "{error}");
    assert_eq!(row_status(&pool, id).await, "turn_active");
    let status: String = sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id=$1")
        .bind(dispatch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "dispatched");
    assert!(!token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
    assert!(
        crate::services::discord::host_teardown_gate::test_support::mailbox_turn_active(
            &shared,
            poise::serenity_prelude::ChannelId::new(channel)
        )
        .await
    );
    assert!(tmux.take_calls().iter().all(|call| !call.contains("kill")));
    channel_home::unregister(home.channel_id());
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn command_unattributed_legacy_stop_and_dispatch_keep_off_fallback_pg() {
    use crate::db::o_channel_homes::HomeState;
    use crate::services::cluster::channel_home;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    assert!(
        channel_home::admit_unattributed().is_ok(),
        "fresh registry is off"
    );
    for registered in [false, true] {
        let gate = registered
            .then(|| channel_home::register_for_test(9200000000000200, Some(HomeState::Worker)));
        for dispatch_case in [false, true] {
            let label = format!("home-unattributed-{registered}-{dispatch_case}");
            let provider = crate::services::provider::ProviderKind::Claude;
            let name = provider.build_tmux_session_name(&label);
            let id = seed_turn(
                &pool,
                Case::Stored(Stored::Legacy),
                (&label, &name),
                "remote-home",
                9200000000000201 + u64::from(registered) * 2 + u64::from(dispatch_case),
            )
            .await;
            sqlx::query("UPDATE sessions SET thread_channel_id=NULL WHERE id=$1")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO task_dispatches (id,title,dispatch_type,status) VALUES ($1,'unattributed','implementation','dispatched')").bind(&label).execute(&pool).await.unwrap();
            sqlx::query("UPDATE sessions SET active_dispatch_id=$2 WHERE id=$1")
                .bind(id)
                .bind(&label)
                .execute(&pool)
                .await
                .unwrap();
            tmux.take_calls();
            if dispatch_case {
                let result = crate::services::queue::QueueService::new(Some(pool.clone()))
                    .cancel_dispatch(None, &label)
                    .await;
                if registered {
                    assert_eq!(result.unwrap_err().status(), StatusCode::CONFLICT);
                } else {
                    assert!(result.unwrap()["ok"].as_bool().unwrap());
                }
            } else {
                let (status, Json(body)) =
                    super::stop_agent_turn(State(state(pool.clone())), Path(label.clone())).await;
                assert_eq!(
                    status,
                    if registered {
                        StatusCode::CONFLICT
                    } else {
                        StatusCode::OK
                    },
                    "{body}"
                );
            }
            assert_eq!(
                row_status(&pool, id).await,
                if registered {
                    "turn_active"
                } else {
                    "disconnected"
                }
            );
            let status: String =
                sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id=$1")
                    .bind(&label)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                status,
                if !registered && dispatch_case {
                    "cancelled"
                } else {
                    "dispatched"
                }
            );
            let calls = tmux.take_calls();
            if registered {
                assert!(calls.iter().all(|call| !call.contains("kill")), "{calls:?}");
            } else {
                assert!(
                    calls.iter().any(|call| call.contains(&name)),
                    "Legacy tmux-name fallback: {calls:?}"
                );
            }
        }
        if let Some(gate) = gate {
            channel_home::unregister(gate.channel_id());
        }
    }
    pool.close().await;
    db.drop().await;
}
