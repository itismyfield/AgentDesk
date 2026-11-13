use super::provider_dispatch::{StreamingTurn, execute};
use crate::config::TestEnvVarGuard as Env;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::{adk_session, session_locator};
use crate::services::platform::tmux::session_inventory::tests::ChannelNamespace;
use crate::services::provider::{CancelToken, ProviderKind};
use std::{path::PathBuf, process::Command, sync::Arc, time::Duration};

struct Sessions {
    tmux: PathBuf,
    names: [String; 2],
}
impl Drop for Sessions {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = Command::new(&self.tmux)
                .args(["kill-session", "-t", &format!("={name}")])
                .output();
        }
    }
}
struct Hosting;
impl Drop for Hosting {
    fn drop(&mut self) {
        crate::services::provider_hosting::install_provider_hosting_config(
            &crate::config::Config::default(),
        );
    }
}

async fn exercise(provider: ProviderKind, channel: u64, refuse: bool) {
    use std::os::unix::fs::PermissionsExt;
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _hosting = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let set = Env::set_path_after_shared_test_env_lock;
    let _root = set("AGENTDESK_ROOT_DIR", root.path());
    let _config = set("AGENTDESK_CONFIG", &root.path().join("unused-config"));
    let _socket_env = set("TMUX_TMPDIR", root.path());
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let mut config = crate::config::Config::default();
    config
        .providers
        .entry(provider.as_str().into())
        .or_default()
        .runtime = Some("pipe".into());
    crate::services::provider_hosting::install_provider_hosting_config(&config);
    let _restore = Hosting;
    let real_tmux = crate::services::platform::binary_resolver::runtime_command("tmux").unwrap();
    let socket = root.path().join("private.sock");
    let tmux = root.path().join("tmux");
    let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/agentdesk");
    assert!(
        cli.is_file(),
        "build the isolated wrapper CLI before this test"
    );
    // The dispatch is real; only its harness executable is replaced by the CLI that runs wrappers.
    std::fs::write(
        &tmux,
        format!(
            r#"#!/usr/bin/env python3
import os, pathlib, shlex, subprocess, sys, time
args = sys.argv[1:]
with pathlib.Path({calls:?}).open("a") as out: out.write(repr(args) + "\n")
if 'new-session' in args and args[-1].startswith('bash '):
    script = pathlib.Path(shlex.split(args[-1])[1])
    text = script.read_text()
    assert 'tmux-wrapper' in text
    text = text.replace({harness:?}, {cli:?})
    script.write_text(text)
result = subprocess.run([{real:?}, '-S', {socket:?}, '-f', '/dev/null'] + args)
if 'new-session' in args and args[-1].startswith('bash ') and {refuse}:
    pathlib.Path({created:?}).write_text('created')
    until = time.monotonic() + 30
    while not pathlib.Path({release:?}).exists():
        assert time.monotonic() < until, 'create barrier release'
        time.sleep(.02)
sys.exit(result.returncode)
"#,
            harness = std::env::current_exe().unwrap().to_str().unwrap(),
            cli = cli.to_str().unwrap(),
            real = real_tmux.get_program().to_str().unwrap(),
            socket = socket.to_str().unwrap(),
            calls = root.path().join("tmux.calls").to_str().unwrap(),
            refuse = if refuse { "True" } else { "False" },
            created = root.path().join("created").to_str().unwrap(),
            release = root.path().join("release").to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _path = Env::prepend_path_after_shared_test_env_lock(root.path());
    let _namespace = ChannelNamespace::new(channel, socket);
    assert_eq!(
        crate::services::platform::binary_resolver::runtime_command("tmux")
            .unwrap()
            .get_program(),
        tmux.as_os_str()
    );
    let fake = root.path().join(provider.as_str());
    std::fs::write(
        &fake,
        format!(
            r#"#!/usr/bin/env python3
import json, os, pathlib, sys
if '--version' in sys.argv or '--help' in sys.argv:
    print('fixture 1.0 --json'); sys.exit(0)
root = pathlib.Path({root:?})
(root / 'provider.ready').write_text('ready')
sid = '11111111-1111-4111-8111-111111111111'
def emit(value): print(json.dumps(value), flush=True)
if {codex}:
    emit({{'type':'thread.started', 'thread_id':sid}})
    emit({{'type':'item.completed', 'item':{{'type':'agent_message','text':'pipe ready'}}}})
    emit({{'type':'turn.completed','usage':{{'input_tokens':1,'output_tokens':1}}}})
else:
    emit({{'type':'system','subtype':'init','session_id':sid}})
    for line in sys.stdin:
        emit({{'type':'result','subtype':'success','result':'pipe ready','session_id':sid}})
"#,
            root = root.path().to_str().unwrap(),
            codex = if provider == ProviderKind::Codex {
                "True"
            } else {
                "False"
            }
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _binary = set(
        if provider == ProviderKind::Claude {
            "AGENTDESK_CLAUDE_PATH"
        } else {
            "AGENTDESK_CODEX_PATH"
        },
        &fake,
    );
    let old_name = format!("s2pipe-old-{channel}");
    let name = format!("s2pipe-renamed-{channel}");
    let _sessions = Sessions {
        tmux: tmux.clone(),
        names: [old_name.clone(), name.clone()],
    };
    let raw = |args: &[&str]| {
        Command::new(&tmux)
            .args(args)
            .env_remove("AGENTDESK_IDENTITY")
            .output()
            .unwrap()
    };
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (shared, registry) =
        crate::services::discord::host_teardown_gate::test_support::runtime(&pool).await;
    shared.settings.write().await.provider = provider.clone();
    registry
        .register(provider.as_str().into(), shared.clone())
        .await;
    assert_eq!(
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            &provider,
            true,
            Some(channel)
        )
        .driver,
        crate::services::provider_hosting::ProviderSessionDriver::LegacyPrompt
    );
    let old = adk_session::build_namespaced_session_key(&shared.token_hash, &provider, &old_name);
    let new = adk_session::build_namespaced_session_key(&shared.token_hash, &provider, &name);
    let id: i64 = sqlx::query_scalar("INSERT INTO sessions(session_key, provider, identity_kind, discord_token_hash, channel_id, active_turn_nonce, status) VALUES ($1,$2,'discord_channel',$3,$4,'old','idle') RETURNING id")
        .bind(&old).bind(provider.as_str()).bind(&shared.token_hash).bind(channel.to_string()).fetch_one(&pool).await.unwrap();
    assert!(
        raw(&["new-session", "-d", "-s", &old_name, "cat"])
            .status
            .success()
    );
    assert_eq!(
        session_locator::channel_key(&shared, &provider, channel, old.clone()).await,
        Some(old.clone())
    );
    assert_eq!(
        session_locator::channel_key(&shared, &provider, channel, new.clone()).await,
        Some(old.clone()),
        "a channel rename must keep a live old execution"
    );
    assert!(
        raw(&["kill-session", "-t", &format!("={old_name}")])
            .status
            .success()
    );
    assert_eq!(
        session_locator::channel_key(&shared, &provider, channel, new.clone()).await,
        Some(new.clone())
    );
    let token = Arc::new(CancelToken::new());
    let nonce = token.turn_nonce().unwrap().to_owned();
    let run_pool = pool.clone();
    let run_shared = shared.clone();
    let run_provider = provider.clone();
    let run_name = name.clone();
    let cwd = root.path().to_str().unwrap().to_owned();
    let run = tokio::task::spawn_blocking(move || {
        let (sender, _receiver) = std::sync::mpsc::channel();
        let host = crate::services::turn_host::TurnHost::Tmux;
        let turn = StreamingTurn {
            pool: Some(&run_pool),
            registration_origin: Some(false),
            shared: Some(&run_shared),
            provider: &run_provider,
            prompt: "pipe registration",
            session_id: None,
            working_dir: &cwd,
            system_prompt: Some(""),
            allowed_tools: &[],
            cancel: token,
            codex_observation: None,
            tmux_session_name: Some(&run_name),
            teardown: None,
            host: &host,
            channel_id: channel,
            model: None,
            native_fast_mode: None,
            effort: None,
            codex_goals: None,
            compact_percent: None,
            compact_lower_bound_tokens: 0,
            compact_token_limit: None,
            cache_ttl_minutes: None,
            dispatch_type: None,
            force_fresh: false,
            submission: None,
        };
        let start =
            crate::services::discord::router::intake_dispatch::replay_admission::dormant_start(
                "pipe registration",
            )
            .unwrap();
        execute(turn, start, sender, None)
    });
    let before = if refuse {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !root.path().join("created").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        sqlx::query(
            "UPDATE sessions SET active_turn_nonce='replacement', status='turn_active' WHERE id=$1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        let row: serde_json::Value =
            sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        std::fs::write(root.path().join("release"), "release").unwrap();
        Some(row)
    } else {
        None
    };
    let result = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .unwrap()
        .unwrap();
    if !refuse {
        assert_eq!(
            result,
            Ok(()),
            "actual pipe dispatch must reach the wrapper"
        );
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        while !root.path().join("provider.ready").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "provider not run: {result:?}; calls: {:?}; pane: {}",
            std::fs::read_to_string(root.path().join("tmux.calls")),
            String::from_utf8_lossy(
                &raw(&["capture-pane", "-p", "-t", &format!("={name}:")]).stdout
            )
        )
    });
    assert!(
        raw(&["has-session", "-t", &format!("={name}")])
            .status
            .success(),
        "the real wrapper session must exist"
    );
    let pane = raw(&[
        "display-message",
        "-p",
        "-t",
        &format!("={name}:"),
        "#{pane_dead}",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&pane.stdout).trim(),
        "0",
        "wrapper must be alive, not a dead test-harness pane"
    );
    if let Some(before) = before {
        assert!(result.unwrap_err().contains("Held(registration_refused)"));
        let after: serde_json::Value =
            sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            before, after,
            "refused pipe registration must preserve the complete row"
        );
        assert!(
            raw(&["has-session", "-t", &format!("={name}")])
                .status
                .success(),
            "refusal must not kill the fresh wrapper"
        );
    } else {
        assert_eq!(result, Ok(()));
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT session_key,active_turn_nonce FROM sessions WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let alias: Option<i64> =
            sqlx::query_scalar("SELECT session_id FROM session_key_aliases WHERE session_key=$1")
                .bind(&old)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            (row.clone(), alias),
            ((new.clone(), Some(nonce)), Some(id)),
            "actual pipe dispatch must commit primary, alias, and nonce together"
        );
        config.policies.hot_reload = false;
        config.policies.dir = root.path().join("policies");
        std::fs::create_dir_all(&config.policies.dir).unwrap();
        let broadcast = crate::server::ws::new_broadcast();
        let state = crate::app_state::AppState {
            pg_pool: Some(pool.clone()),
            engine: crate::engine::PolicyEngine::new(&config).unwrap(),
            config: Arc::new(config),
            batch_buffer: crate::server::ws::spawn_batch_flusher(broadcast.clone()),
            broadcast_tx: broadcast,
            health_registry: Some(registry),
            cluster_instance_id: None,
        };
        let killed = crate::services::dispatched_sessions::force_kill_session_impl_with_reason(
            &state,
            &row.0,
            false,
            "manual: isolated pipe primary",
        )
        .await;
        assert_eq!(killed.0, axum::http::StatusCode::OK);
        assert!(
            !raw(&["has-session", "-t", &format!("={name}")])
                .status
                .success(),
            "current-row force kill must remove the new wrapper"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_promotion_real_claude_pipe_dispatch_commits_and_kills_pg() {
    exercise(ProviderKind::Claude, 81190501, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_promotion_real_codex_pipe_dispatch_commits_and_kills_pg() {
    exercise(ProviderKind::Codex, 81190502, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_promotion_real_claude_pipe_registration_refusal_holds_pg() {
    exercise(ProviderKind::Claude, 81190503, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_promotion_real_codex_pipe_registration_refusal_holds_pg() {
    exercise(ProviderKind::Codex, 81190504, true).await;
}
