use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use poise::serenity_prelude::ChannelId;

use super::{cleanup_expired_sessions, mark_session_disconnected_for_idle_cleanup};
use crate::db::dispatched_sessions::hosted_execution::tests::{future_schema, owner, record, wire};
use crate::db::dispatched_sessions::hosted_execution::{HostedOwner, HostedState};
use crate::services::discord::{DiscordSession, SESSION_MAX_IDLE, adk_session};
use crate::services::provider::ProviderKind;

/// Fake tmux that logs each call: `probefail` targets fail the probe, every other is gone.
fn install_fake_tmux() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
    let temp = tempfile::TempDir::new().expect("tmux dir");
    let binary = temp.path().join("tmux");
    let mut file = std::fs::File::create(&binary).expect("fake tmux");
    writeln!(
        file,
        "#!/bin/sh\n[ \"$1\" = -u ] && shift\necho \"$*\" >> \"$(dirname \"$0\")/calls\"\n\
         case \"$3\" in *probefail*) echo 'permission denied' >&2; exit 1 ;; esac\n\
         echo \"can't find session: $3\" >&2; exit 1"
    )
    .expect("fake tmux body");
    let mut permissions = std::fs::metadata(&binary).expect("meta").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&binary, permissions).expect("chmod fake tmux");
    let mut paths = vec![temp.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).expect("join PATH");
    let guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "PATH",
        std::path::Path::new(&path),
    );
    (temp, guard)
}

fn expired_session(channel_name: &str) -> DiscordSession {
    DiscordSession {
        session_id: None,
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: None,
        channel_name: Some(channel_name.to_string()),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
    }
}

async fn insert_row(
    pool: &sqlx::PgPool,
    key: &str,
    canonical: Option<(&str, &str)>,
    raw: Option<serde_json::Value>,
) {
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, last_heartbeat, identity_kind,
                               discord_token_hash, channel_id, hosted_execution)
         VALUES ($1, 'claude', 'idle', NOW() - INTERVAL '5 hours',
                 CASE WHEN $2::TEXT IS NULL THEN NULL ELSE 'discord_channel' END, $2, $3, $4)",
    )
    .bind(key)
    .bind(canonical.map(|(token, _)| token))
    .bind(canonical.map(|(_, channel)| channel))
    .bind(raw)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn idle_cleanup_expires_only_legacy_tmux_sessions_pg() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (tmux_dir, _path_guard) = install_fake_tmux();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let _root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime_root.path(),
    );
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let token = shared.token_hash.clone();
    let provider = ProviderKind::Claude;
    let tmux = |n: &str| provider.build_tmux_session_name(&format!("idle-guard-{n}"));
    let key = |n: &str| adk_session::build_namespaced_session_key(&token, &provider, &tmux(n));
    let elsewhere = |n: &str| format!("claude/{token}/other-host:AgentDesk-claude-elsewhere-{n}");
    let names = [
        "legacy",
        "noncanonical",
        "pending",
        "bound",
        "unknown",
        "traced",
        "missing",
        "herdr-elsewhere",
        "split",
        "probefail",
    ];
    let channel = |n: &str| {
        (1_500_600_700_800_900_000 + names.iter().position(|m| *m == n).unwrap() as u64).to_string()
    };
    let owner_of = |n: &str| HostedOwner {
        discord_token_hash: token.clone(),
        ..owner(&channel(n))
    };
    for n in names {
        let ch = channel(n);
        let canonical = Some((token.as_str(), ch.as_str()));
        match n {
            "noncanonical" => insert_row(&pool, &key(n), None, None).await,
            "pending" | "bound" => {
                let state = if n == "bound" {
                    HostedState::Bound
                } else {
                    HostedState::Pending
                };
                let raw = wire(&record(&owner_of(n), "n1", state));
                insert_row(&pool, &key(n), canonical, Some(raw)).await
            }
            "unknown" => {
                insert_row(&pool, &key(n), canonical, Some(future_schema(&owner_of(n)))).await
            }
            "missing" => {}
            // The channel's canonical row lives under another key; its tmux-named row is legacy.
            "herdr-elsewhere" | "split" => {
                let raw = (n == "herdr-elsewhere")
                    .then(|| wire(&record(&owner_of(n), "n1", HostedState::Bound)));
                insert_row(&pool, &elsewhere(n), canonical, raw).await;
                insert_row(&pool, &key(n), None, None).await;
            }
            _ => insert_row(&pool, &key(n), canonical, None).await,
        }
        let id = ChannelId::new(ch.parse().unwrap());
        let session = expired_session(&format!("idle-guard-{n}"));
        shared.core.lock().await.sessions.insert(id, session);
    }
    let marker = crate::services::tmux_common::session_temp_path(&tmux("traced"), "host_kind");
    std::fs::write(marker, "herdr").unwrap();
    // Every in-memory session is now past the idle limit.
    tokio::time::pause();
    tokio::time::advance(SESSION_MAX_IDLE + std::time::Duration::from_secs(60)).await;
    tokio::time::resume();

    cleanup_expired_sessions(&shared).await;

    let expired = ["legacy", "noncanonical"];
    let mut remaining: Vec<u64> = shared
        .core
        .lock()
        .await
        .sessions
        .keys()
        .map(|id| id.get())
        .collect();
    remaining.sort();
    let kept: Vec<u64> = names
        .iter()
        .filter(|n| !expired.contains(n))
        .map(|n| channel(n).parse().unwrap())
        .collect();
    assert_eq!(remaining, kept, "only legacy tmux sessions leave memory");
    let status = |key: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
                .bind(key)
                .fetch_optional(&pool)
                .await
                .unwrap()
        }
    };
    for n in names.into_iter().filter(|n| *n != "missing") {
        let expected = if expired.contains(&n) {
            "disconnected"
        } else {
            "idle"
        };
        assert_eq!(status(key(n)).await.as_deref(), Some(expected), "{n}");
    }
    for n in ["herdr-elsewhere", "split"] {
        assert_eq!(status(elsewhere(n)).await.as_deref(), Some("idle"), "{n}");
    }
    let calls = std::fs::read_to_string(tmux_dir.path().join("calls")).unwrap_or_default();
    let mut probed: Vec<&str> = calls.lines().collect();
    probed.sort();
    let mut expected: Vec<String> = ["legacy", "noncanonical", "probefail"]
        .map(|n| format!("has-session -t ={}:", tmux(n)))
        .into();
    expected.sort();
    assert_eq!(
        probed, expected,
        "no tmux name is probed for a non-legacy host"
    );

    // A row that gains a hosted record after the host check keeps its status and dispatch.
    assert!(!mark_session_disconnected_for_idle_cleanup(Some(&pool), &key("bound")).await);
    assert_eq!(status(key("bound")).await.as_deref(), Some("idle"));

    pool.close().await;
    db.drop().await;
}

/// A Herdr-configured channel's expired legacy session keeps its returned input in memory and
/// its row, while an unconfigured one beside it expires as before.
#[tokio::test(flavor = "current_thread")]
async fn idle_cleanup_keeps_a_configured_channel_and_its_returned_input_pg() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (tmux_dir, _path_guard) = install_fake_tmux();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let _root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime_root.path(),
    );
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let token = shared.token_hash.clone();
    let provider = ProviderKind::Claude;
    let tmux = |n: &str| provider.build_tmux_session_name(&format!("idle-herdr-{n}"));
    let key = |n: &str| adk_session::build_namespaced_session_key(&token, &provider, &tmux(n));
    let (configured, plain) = (1_500_600_700_800_901_001u64, 1_500_600_700_800_901_002u64);
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("mac-mini"), &[(configured, "mac-mini")]);
    for (n, channel) in [("configured", configured), ("plain", plain)] {
        let ch = channel.to_string();
        insert_row(&pool, &key(n), Some((token.as_str(), ch.as_str())), None).await;
        let mut session = expired_session(&format!("idle-herdr-{n}"));
        session.pending_uploads = vec!["returned-upload".into()];
        shared
            .core
            .lock()
            .await
            .sessions
            .insert(ChannelId::new(channel), session);
    }
    tokio::time::pause();
    tokio::time::advance(SESSION_MAX_IDLE + std::time::Duration::from_secs(60)).await;
    tokio::time::resume();

    cleanup_expired_sessions(&shared).await;

    let data = shared.core.lock().await;
    let kept = data.sessions.get(&ChannelId::new(configured));
    let uploads = kept.map(|session| session.pending_uploads.clone());
    assert_eq!(
        uploads,
        Some(vec!["returned-upload".into()]),
        "returned input kept"
    );
    assert!(
        !data.sessions.contains_key(&ChannelId::new(plain)),
        "unconfigured expires"
    );
    drop(data);
    let status = |n: &str| {
        let query = "SELECT status FROM sessions WHERE session_key = $1";
        sqlx::query_scalar::<_, String>(query)
            .bind(key(n))
            .fetch_one(&pool)
    };
    assert_eq!(status("configured").await.unwrap(), "idle");
    assert_eq!(status("plain").await.unwrap(), "disconnected");
    let calls = std::fs::read_to_string(tmux_dir.path().join("calls")).unwrap_or_default();
    assert!(!calls.contains(&tmux("configured")), "{calls}");
    pool.close().await;
    db.drop().await;
}
