//! HTTP diagnostics and provider-auth routes on every stored host case, through their handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde_json::Value;

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

/// Seeds `case` as a working turn of `agent` on a remote-named host, read by heartbeat.
async fn seed_turn(pool: &sqlx::PgPool, case: Case, agent: &str, name: &str, channel: u64) -> i64 {
    let key = format!("claude/p4c2/p4c2-remote:{name}");
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

// Diag, session output and turn status show a non-legacy session as unsupported without a
// tmux probe or capture by its name; a legacy row reads tmux as in main.
#[tokio::test]
async fn diagnostics_routes_never_probe_a_non_legacy_session_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let state = state(pool.clone());
    let provider = crate::services::provider::ProviderKind::Claude;
    for (n, case) in Case::ALL.into_iter().enumerate() {
        if !case.has_row() {
            continue;
        }
        let legacy = case == Case::Stored(Stored::Legacy);
        let channel = 1_479_671_302_387_065_000 + n as u64;
        let (agent, name) = (
            format!("p4c2-agent-{n}"),
            provider.build_tmux_session_name(&format!("p4c2-diag-{n}")),
        );
        let id = seed_turn(&pool, case, &agent, &name, channel).await;
        tmux.take_calls();

        let diag = super::agent_diag(State(state.clone()), Path(channel.to_string())).await;
        let Ok((StatusCode::OK, Json(diag))) = diag else {
            panic!("{case:?}: diag failed");
        };
        let observed = |field: &str| diag[field]["state"].as_str().map(str::to_string);
        let unsupported = Some("host_unsupported".to_string());
        assert_eq!(
            observed("tmux_relay_adoption") == unsupported,
            !legacy,
            "{case:?}: {diag}"
        );
        assert_eq!(
            observed("tui_prompt_readiness") == unsupported,
            !legacy,
            "{case:?}: {diag}"
        );

        let query = Query(crate::services::dispatched_sessions::TmuxOutputQuery { lines: None });
        let output = super::super::dispatched_sessions::tmux_output;
        let (status, Json(output)) =
            output(State(state.clone()), HeaderMap::new(), Path(id), query).await;
        let expected = if legacy {
            StatusCode::OK
        } else {
            StatusCode::CONFLICT
        };
        assert_eq!(status, expected, "{case:?}: {output}");

        let turn = crate::services::agents::turn::load_agent_turn_status_pg(&pool, &agent).await;
        let turn: Value = turn.expect("turn status");
        assert_eq!(
            turn["host_unsupported"].is_string(),
            !legacy,
            "{case:?}: {turn}"
        );

        let calls = tmux.take_calls();
        assert_eq!(
            naming(&calls, &name).is_empty(),
            !legacy,
            "{case:?}: {calls:?}"
        );
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
