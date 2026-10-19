//! A process ending right after its close keeps the row's nonce and hold for a restarted process
//! to retire; a clear after a Herdr restart left only another shell retires without a close.

use std::io::Write;
use std::time::Duration;

use super::{OperatorTerminate, TerminationResult, terminate_registered_herdr, test_turn};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::session_host::herdr_termination_rig as herdr;

const CHANNEL: u64 = 5_340_212;
const PHASE: &str = "ADK_HERDR_TERMINATE_EXIT_PHASE";
const DATABASE: &str = "ADK_HERDR_TERMINATE_EXIT_DATABASE";
const WITNESS: &str = "R11_EXIT_AFTER_CLOSE";

fn current_thread(rig: &herdr::P11TerminationRig) -> tokio::runtime::Runtime {
    rig.thread_installer()();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap()
}

#[test]
fn act_terminate_process_exit_after_close_recovers_pg() {
    if herdr::is_child() {
        return child(&std::env::var(PHASE).unwrap());
    }
    let path = concat!(
        module_path!(),
        "::act_terminate_process_exit_after_close_recovers_pg"
    );
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let run = |phase: &str| {
                let home = tempfile::tempdir().unwrap();
                let output = herdr::child_command(path, CHANNEL, home.path())
                    .env(PHASE, phase)
                    .env(DATABASE, &db.database_url)
                    .env("AGENTDESK_ROOT_DIR", root.path())
                    .output()
                    .unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                (output.status, stdout, stderr)
            };
            let (status, stdout, stderr) = run("close");
            assert!(status.success(), "{status}\n{stdout}\n{stderr}");
            assert!(
                stdout
                    .lines()
                    .any(|line| line == format!("{WITNESS} closes=1")),
                "{stdout}\n{stderr}"
            );
            assert!(
                !stdout.contains("test result:"),
                "the child ended through exit(0), not the harness: {stdout}"
            );
            let key = stdout
                .lines()
                .find_map(|line| line.split_once("R11_SESSION_KEY ").map(|(_, key)| key))
                .expect("the child names its row")
                .to_string();
            let bound = (herdr::HostedState::Bound, herdr::EXECUTION.to_string());
            assert_eq!(herdr::row(&pool, &key).await, Some(bound), "no CAS ran");
            assert!(herdr::held(), "the hold outlives the process");

            let (status, stdout, stderr) = run("rerun");
            assert!(
                status.success()
                    && stdout.lines().any(|line| line.starts_with(
                        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
                    )),
                "{status}\n{stdout}\n{stderr}"
            );
            let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
            assert_eq!(herdr::row(&pool, &key).await, Some(retired));
            assert!(!herdr::held());
            eprintln!(
                "R11_OBSERVED {}",
                stdout
                    .lines()
                    .find_map(|line| line.split_once("R11_OBSERVED ").map(|(_, seen)| seen))
                    .unwrap_or("none")
            );
            pool.close().await;
            db.drop().await;
        });
}

/// One process of the scenario: `close` starts the run and exits once its close is at Herdr;
/// `rerun` is the restarted process running the same request again.
fn child(phase: &str) {
    let rig = herdr::P11TerminationRig::boot(CHANNEL);
    current_thread(&rig).block_on(async {
        let url = std::env::var(DATABASE).unwrap();
        let pool = crate::db::postgres::connect_test_pool_with_max_connections(&url, "exit", 4)
            .await
            .unwrap();
        let (shared, registry) =
            super::super::host_teardown_gate::test_support::runtime(&pool).await;
        let key = format!(
            "claude/{}/{}:AgentDesk-claude-exit",
            shared.token_hash,
            herdr::NODE
        );
        let request = || OperatorTerminate {
            session_key: key.clone(),
            execution_nonce: herdr::EXECUTION.into(),
        };
        let channel = super::ChannelId::new(CHANNEL);
        if phase == "close" {
            let switches = rig
                .bind(&pool, &shared.token_hash, &key, herdr::HostedState::Bound)
                .await;
            std::mem::forget(switches);
            test_turn::start(&shared, channel, 51, "turn-a").await;
            let mut hold = rig.hold_next_write();
            let (registry, pool, request) = (registry.clone(), pool.clone(), request());
            tokio::spawn(async move {
                terminate_registered_herdr(&registry, pool, request, "operator test".into()).await
            });
            tokio::time::timeout(Duration::from_secs(10), &mut hold.reached)
                .await
                .expect("the close reached Herdr")
                .unwrap();
            println!("R11_SESSION_KEY {key}");
            println!("{WITNESS} closes={}", rig.writes().len());
            std::io::stdout().flush().unwrap();
            // The SIGTERM handler's own call: nothing after the close runs in this process.
            std::process::exit(0);
        }
        // The same Herdr serves the restarted process at this rig's socket; the pane is gone.
        let socket = rig.record(&shared.token_hash, &key).location.unwrap().socket_addr;
        sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{location,socket_addr}', to_jsonb($2::text)) WHERE session_key = $1")
            .bind(&key).bind(socket).execute(&pool).await.unwrap();
        let mut live = crate::config_live_reload::current()
            .map(|config| (*config).clone())
            .unwrap_or_default();
        live.runtime.herdr_terminate_enabled = Some(true);
        crate::config_live_reload::install(live);
        let _gone = rig.hide_panes();
        let result = terminate_registered_herdr(&registry, pool.clone(), request(), "rerun".into());
        assert_eq!(result.await, TerminationResult::Retired);
        assert_eq!(rig.writes(), Vec::new(), "an ended pane is never closed again");
        println!("R11_OBSERVED inflight={:?}", test_turn::inflight(channel));
        pool.close().await;
    });
}

const RESTORED_CHANNEL: u64 = 5_340_213;

// A restarted Herdr restored the pane as another shell alone and both recorded processes are
// gone: the user's clear closes nothing, retires the execution and releases its hold.
#[test]
fn clear_root_replaced_retires_without_close_pg() {
    use super::super::commands::control::{SoftClearNotifyMode, clear_channel_session_state};
    let path = concat!(
        module_path!(),
        "::clear_root_replaced_retires_without_close_pg"
    );
    if !herdr::in_child(path, RESTORED_CHANNEL) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(RESTORED_CHANNEL);
    let install = rig.thread_installer();
    install();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap();
    runtime.block_on(async {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool) = super::super::host_defer_gate::tests::postgres().await;
        let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let provider = crate::services::provider::ProviderKind::Claude;
        let channel = super::ChannelId::new(RESTORED_CHANNEL);
        super::super::host_defer_gate::tests::map_channel(&shared, channel, "p11-clear").await;
        let key = super::super::adk_session::build_adk_session_key;
        let key = key(&shared, channel, &provider, None).await.unwrap();
        let bound = herdr::HostedState::Bound;
        let _switches = rig.bind(&pool, &shared.token_hash, &key, bound).await;
        rig.restore_shell();

        let http = std::sync::Arc::new(poise::serenity_prelude::Http::new(""));
        let notify = SoftClearNotifyMode::Suppress;
        let clear =
            clear_channel_session_state(&http, &shared, &provider, channel, "/clear", notify);
        let reply = clear.await.expect("a clear always commits").reply();

        assert_eq!(reply, super::super::commands::SESSION_CLEARED_RESPONSE);
        assert!(rig.writes().is_empty(), "the restored shell is not closed");
        let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
        assert_eq!(herdr::row(&pool, &key).await, Some(retired));
        assert!(!herdr::held(), "the execution's hold is released");
        pool.close().await;
        db.drop().await;
    });
}
