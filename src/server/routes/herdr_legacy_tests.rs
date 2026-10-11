//! The session routes against a Herdr row the termination service retired: delete and cleanup
//! keep their Retired-and-owned rule, force-kill and kill-tmux reach neither tmux nor Herdr.

use std::os::unix::fs::PermissionsExt;
use std::path::Path as FsPath;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde_json::Value;

use super::AppState;
use super::dispatched_sessions::{
    DeleteSessionQuery, ForceKillOptions, KillTmuxOptions, cleanup_sessions, delete_session,
    force_kill_session, kill_tmux_session,
};
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::db::dispatched_sessions::hosted_execution::tests::{
    TOKEN, future_schema, owner, record, wire,
};
use crate::services::discord::herdr_terminate::{
    OperatorTerminate, TerminationResult, terminate_explicit_herdr,
};
use crate::services::session_host::herdr_termination_rig as herdr;

const CHANNEL: u64 = 1_479_671_301_387_059_402;
const HERDR_TMUX: &str = "AgentDesk-claude-p11-t8";
/// The runtime token hash `make_shared_data_for_tests` gives its shared state.
const RUNTIME_TOKEN: &str = "test-token-hash";

fn state(pool: sqlx::PgPool) -> AppState {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).expect("construct policy engine");
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

/// A `tmux` on PATH that logs every call to `calls` beside it and answers success.
fn recording_tmux(dir: &FsPath) -> crate::config::TestEnvVarGuard {
    let tmux = dir.join("tmux");
    let script = "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/calls\"\nexit 0\n";
    std::fs::write(&tmux, script).unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(dir)
}

fn tmux_calls(dir: &FsPath) -> Vec<String> {
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap_or_default();
    calls.lines().map(str::to_owned).collect()
}

/// One disconnected row on its own channel; `None` leaves the record NULL (legacy).
async fn seed(pool: &sqlx::PgPool, key: &str, channel: &str, raw: Option<Value>) {
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, last_heartbeat, identity_kind,
                               discord_token_hash, channel_id, hosted_execution)
         VALUES ($1, 'claude', 'disconnected', NOW(), 'discord_channel', $2, $3, $4)",
    )
    .bind(key)
    .bind(TOKEN)
    .bind(channel)
    .bind(raw)
    .execute(pool)
    .await
    .unwrap();
}

async fn row(pool: &sqlx::PgPool, key: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT to_jsonb(s) - 'updated_at' FROM sessions s WHERE session_key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap()
}

// Each route's real handler deletes only the service's own Retired row and kills no Herdr or tmux
// pane for Bound, Pending, Unknown or foreign rows, while a legacy row is still killed and cleaned.
#[test]
fn m2t_t8_legacy_delete_and_force_routes_preserve_host_guards_pg() {
    let path = concat!(
        module_path!(),
        "::m2t_t8_legacy_delete_and_force_routes_preserve_host_guards_pg"
    );
    if !herdr::in_child(path, CHANNEL) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL);
    let install = rig.thread_installer();
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(scenario(rig));
}

async fn scenario(rig: herdr::P11TerminationRig) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = tempfile::tempdir().unwrap();
    let _path = recording_tmux(tmux.path());
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let app = state(pool.clone());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let host = crate::services::platform::hostname_short();
    let token_hash = RUNTIME_TOKEN.to_string();
    let herdr_key = format!("claude/{token_hash}/{host}:{HERDR_TMUX}");
    let bound = herdr::HostedState::Bound;
    let switches = rig.bind(&pool, &token_hash, &herdr_key, bound).await;
    let key = |name: &str| format!("{host}:AgentDesk-claude-t8-{name}");
    let foreign = record(&owner("5340903"), "n1", HostedState::Retired);
    let others = [
        (
            "pending",
            "5340901",
            record(&owner("5340901"), "n1", HostedState::Pending),
        ),
        ("foreign", "5340904", foreign),
    ];
    for (name, channel, record) in others {
        seed(&pool, &key(name), channel, Some(wire(&record))).await;
    }
    seed(
        &pool,
        &key("unknown"),
        "5340902",
        Some(future_schema(&owner("5340902"))),
    )
    .await;
    seed(&pool, &key("legacy"), "5340905", None).await;
    let kept = ["pending", "unknown", "foreign"].map(key);

    let delete = |session_key: String| {
        let query = DeleteSessionQuery {
            session_key,
            provider: None,
        };
        delete_session(State(app.clone()), Query(query))
    };
    let force_kill = |session_key: String| {
        let options = ForceKillOptions {
            retry: false,
            reason: Some("operator cleanup".into()),
        };
        force_kill_session(
            State(app.clone()),
            HeaderMap::new(),
            Path(session_key),
            Json(options),
        )
    };
    let kill_tmux = |session_key: String| {
        let options = KillTmuxOptions {
            reason: Some("operator cleanup".into()),
            minimum_idle_minutes: None,
        };
        kill_tmux_session(
            State(app.clone()),
            HeaderMap::new(),
            Path(session_key),
            Json(options),
        )
    };
    let kept_by_host = |label: &str, (status, Json(body)): (StatusCode, Json<Value>)| {
        assert_eq!(status, StatusCode::CONFLICT, "{label}: {body}");
        assert!(
            body.to_string().contains("session host is not legacy tmux"),
            "{label}: {body}"
        );
    };
    let tmux_preserved =
        |label: &str, reason: &str, (status, Json(body)): (StatusCode, Json<Value>)| {
            assert_eq!(status, StatusCode::OK, "{label}: {body}");
            assert_eq!(body["tmux_killed"], false, "{label}: {body}");
            assert_eq!(body["preserved_reason"], reason, "{label}: {body}");
        };

    // Bound: the delete is refused and neither kill reaches tmux.
    let before = row(&pool, &herdr_key).await;
    let refused = delete(herdr_key.clone())
        .await
        .expect_err("a Bound row is kept");
    assert!(
        format!("{refused:?}").contains("hosted execution"),
        "{refused:?}"
    );
    kept_by_host("bound force-kill", force_kill(herdr_key.clone()).await);
    let preserved = kill_tmux(herdr_key.clone()).await;
    tmux_preserved("bound kill-tmux", "host_not_legacy_tmux", preserved);
    assert_eq!(
        row(&pool, &herdr_key).await,
        before,
        "the Bound row is unchanged"
    );
    assert_eq!(
        tmux_calls(tmux.path()),
        Vec::<String>::new(),
        "no tmux call for Bound"
    );

    // Positive control: a legacy row is killed through tmux and then cleaned up.
    let (status, Json(body)) = force_kill(key("legacy")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let calls = tmux_calls(tmux.path());
    assert!(
        calls.iter().any(|call| call.contains("kill-session")),
        "{calls:?}"
    );
    let (_, Json(cleaned)) = cleanup_sessions(State(app.clone())).await.unwrap();
    assert_eq!(cleaned["deleted"], 1, "only the legacy row: {cleaned}");
    assert_eq!(row(&pool, &key("legacy")).await, None);

    let request = OperatorTerminate {
        session_key: herdr_key.clone(),
        execution_nonce: herdr::EXECUTION.into(),
    };
    let result = terminate_explicit_herdr(request, shared.clone(), &pool).await;
    assert_eq!(result, TerminationResult::Retired);
    let retired = herdr::row(&pool, &herdr_key).await;
    assert_eq!(
        retired,
        Some((HostedState::Retired, herdr::EXECUTION.into()))
    );
    let close = rig.writes();
    let one_close = matches!(close.as_slice(), [(_, pane)] if pane == "w1-1");
    assert!(one_close, "the service's close is the one write: {close:?}");

    // Retired by the service, Pending, Unknown and a foreign Retired row: no kill reaches tmux.
    // The service's row loses its host marker first, so each row is kept by its record alone.
    let marker = crate::services::tmux_common::session_temp_path(HERDR_TMUX, "host_kind");
    std::fs::remove_file(&marker).expect("the launch's host marker");
    let calls_before = tmux_calls(tmux.path()).len();
    for session_key in std::iter::once(&herdr_key).chain(&kept) {
        let before = row(&pool, session_key).await;
        let label = |route: &str| format!("{session_key} {route}");
        kept_by_host(&label("force-kill"), force_kill(session_key.clone()).await);
        // The foreign row's owner names another channel, so its lookup is a conflict.
        let reason = match session_key == &key("foreign") {
            true => "host_row_conflict",
            false => "host_not_legacy_tmux",
        };
        let preserved = kill_tmux(session_key.clone()).await;
        tmux_preserved(&label("kill-tmux"), reason, preserved);
        let after = row(&pool, session_key).await;
        assert_eq!(after, before, "{session_key}: the kills change no column");
    }
    assert_eq!(
        tmux_calls(tmux.path()).len(),
        calls_before,
        "no tmux call for a hosted row"
    );

    // Only the Retired row the service wrote for its own owner is deleted.
    for session_key in &kept {
        let refused = delete(session_key.clone()).await;
        assert!(refused.is_err(), "{session_key}: {refused:?}");
    }
    let (_, Json(deleted)) = delete(herdr_key.clone()).await.unwrap();
    assert_eq!(deleted["deleted"], 1, "{deleted}");
    assert_eq!(row(&pool, &herdr_key).await, None);
    let (_, Json(cleaned)) = cleanup_sessions(State(app.clone())).await.unwrap();
    assert_eq!(cleaned["deleted"], 0, "{cleaned}");
    for session_key in &kept {
        assert!(
            row(&pool, session_key).await.is_some(),
            "{session_key} is kept"
        );
    }
    assert_eq!(rig.writes(), close, "no route reached Herdr");
    drop(switches);
    pool.close().await;
    db.drop().await;
}

const AUTH_ONLY: &str = "p11-auth-only";
const CLAIMED: &str = "p11-herdr-claimed";

/// org.yaml naming both profiles as auth-only Claude accounts, each with its credential home.
fn seed_profiles(accounts: &FsPath) -> std::path::PathBuf {
    let root = crate::config::runtime_root().expect("the test runtime root");
    let org = crate::services::discord::runtime_store::org_schema_path_for_root(&root);
    std::fs::create_dir_all(org.parent().unwrap()).unwrap();
    let mut yaml = String::from("version: 1\nprovider_auth_profiles:\n");
    for profile in [AUTH_ONLY, CLAIMED] {
        let home = accounts.join(profile);
        std::fs::create_dir_all(&home).unwrap();
        let entry = format!(
            "  {profile}:\n    provider: claude\n    home: {}\n",
            home.display()
        );
        yaml.push_str(&entry);
    }
    std::fs::write(&org, yaml).unwrap();
    org
}

fn profiles(org: &FsPath) -> Vec<String> {
    let document: serde_yaml::Value = serde_yaml::from_slice(&std::fs::read(org).unwrap()).unwrap();
    let profiles = document["provider_auth_profiles"].as_mapping().unwrap();
    let ids = profiles.keys().filter_map(|key| key.as_str());
    ids.map(str::to_owned).collect()
}

// T8-b: an auth-only profile's unlink still drops it from org.yaml and kills its login tmux, while
// a login target a Herdr marker claims, beside Bound and Retired rows, is refused with nothing changed.
#[tokio::test]
async fn m2t_t8_auth_unlink_cleans_auth_only_and_refuses_a_herdr_claim_pg() {
    use super::provider_auth_profiles::remove_profile;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = tempfile::tempdir().unwrap();
    let _path = recording_tmux(tmux.path());
    let accounts = tempfile::tempdir().unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _home = set("HOME", accounts.path());
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let org = seed_profiles(accounts.path());
    let provider = crate::services::provider::ProviderKind::Claude;
    let login = |profile| {
        crate::services::provider_auth_profile::login_tmux_session_name(&provider, profile)
    };
    let (claimed, auth_only) = (login(CLAIMED), login(AUTH_ONLY));
    let marker = crate::services::tmux_common::session_temp_path(&claimed, "host_kind");
    std::fs::create_dir_all(FsPath::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, "herdr").unwrap();
    let host = crate::services::platform::hostname_short();
    let mut keys = Vec::new();
    for (channel, state) in [
        ("5340911", HostedState::Bound),
        ("5340912", HostedState::Retired),
    ] {
        let mut claimant = owner(channel);
        claimant.logical_key = claimed.clone();
        let key = format!("{host}:{claimed}-{channel}");
        seed(
            &pool,
            &key,
            channel,
            Some(wire(&record(&claimant, "n-claimed", state))),
        )
        .await;
        keys.push(key);
    }
    let mut rows = Vec::new();
    for key in &keys {
        rows.push(row(&pool, key).await.expect("a seeded Herdr row"));
    }
    let org_before = std::fs::read(&org).unwrap();

    let removed = remove_profile(Path(("claude".to_string(), CLAIMED.to_string()))).await;
    let error = removed.expect_err("the claimed target's unlink is refused");
    assert_eq!(error.status(), StatusCode::CONFLICT, "{error:?}");
    assert_eq!(
        std::fs::read(&org).unwrap(),
        org_before,
        "org.yaml is unchanged"
    );
    assert_eq!(
        tmux_calls(tmux.path()),
        Vec::<String>::new(),
        "no tmux call"
    );
    for (key, before) in keys.iter().zip(&rows) {
        assert_eq!(
            row(&pool, key).await.as_ref(),
            Some(before),
            "the Herdr rows are unchanged"
        );
    }

    // The positive control: the auth-only profile is unlinked and its login tmux killed.
    let removed = remove_profile(Path(("claude".to_string(), AUTH_ONLY.to_string()))).await;
    let (status, Json(body)) = removed.expect("the auth-only unlink succeeds");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["credentials_retained"], true, "{body}");
    assert_eq!(
        profiles(&org),
        [CLAIMED],
        "only the auth-only profile is unlinked"
    );
    let calls = tmux_calls(tmux.path());
    let on =
        |verb: &str, target: &str| calls.iter().any(|c| c.contains(verb) && c.contains(target));
    assert!(on("has-session", &auth_only), "{calls:?}");
    assert!(on("kill-session", &auth_only), "{calls:?}");
    assert!(
        calls.iter().all(|call| !call.contains(&claimed)),
        "{calls:?}"
    );
    assert!(
        accounts.path().join(AUTH_ONLY).is_dir(),
        "credentials are kept"
    );
    for (key, before) in keys.iter().zip(&rows) {
        assert_eq!(
            row(&pool, key).await.as_ref(),
            Some(before),
            "the Herdr rows are unchanged"
        );
    }
    pool.close().await;
    db.drop().await;
}
