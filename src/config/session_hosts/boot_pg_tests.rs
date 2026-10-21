use crate::config::session_hosts::approval_store::{ApprovalStore, FileApprovalStore};
use crate::config::session_hosts::migration::{self, NoHoldStore, PgRows, RowSource};
use crate::config::session_hosts::{self, BootSessionHosts};
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record};
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};

fn config(root: &std::path::Path, socket: &str) -> crate::config::Config {
    serde_json::from_value(serde_json::json!({
        "server": {}, "data": {"dir": root.join("data")},
        "cluster": {"instance_id": "test-node"},
        "agents": [{"id": "fixture", "name": "fixture", "channels": {
            "claude": {"id": "41", "runtime": "tui"}, "codex": {"id": "42", "runtime": "tui"}
        }}], "session_hosts": {"herdr": {
            "endpoints": {"herdr.default": {"execution_node": "test-node", "socket_path": root.join(socket),
                "herdr_home": root.join("herdr"), "herdr_session": "agentdesk"}},
            "channels": {"41": "herdr.default", "42": "herdr.default"}
        }}
    })).unwrap()
}

async fn seed(
    pool: &sqlx::PgPool,
    root: &std::path::Path,
    state: HostedState,
    located: bool,
    channel: u64,
) {
    let mut row = record(
        &owner(&channel.to_string()),
        &format!("nonce-{channel}"),
        state,
    );
    if !located {
        row.location = None;
    }
    if let Some(location) = row.location.as_mut() {
        location.socket_addr =
            config(root, "old.sock").session_hosts.herdr.endpoints["herdr.default"]
                .socket_path
                .display()
                .to_string();
    }
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)")
        .bind(format!("claude/discord_0123456789abcdef/test-node:fixture-{channel}"))
        .bind(&row.owner.discord_token_hash).bind(channel.to_string()).bind(serde_json::to_value(row).unwrap())
        .execute(pool).await.unwrap();
}

async fn herdr_health(config: &crate::config::Config, pool: &sqlx::PgPool) -> serde_json::Value {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let broadcast_tx = crate::server::ws::new_broadcast();
    let state = crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine: crate::engine::PolicyEngine::new(&engine_config).unwrap(),
        config: std::sync::Arc::new(config.clone()),
        batch_buffer: crate::server::ws::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: Some(std::sync::Arc::new(
            crate::services::discord::health::HealthRegistry::new(),
        )),
        cluster_instance_id: None,
    };
    let response = crate::server::routes::health_api::health_detail_handler(
        axum::extract::State(state),
        axum::extract::ConnectInfo("127.0.0.1:0".parse().unwrap()),
    )
    .await;
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["herdr"].is_object(), "{body}");
    body["herdr"].clone()
}

// Exercise the server-owned boot boundaries in isolated children; neither installs global state in the parent.
#[test]
fn boot_entries_select_approved_hosts_pg() {
    const CELL: &str = "ADK_TEST_HOSTS_BOOT_CELL";
    let Ok(cell) = std::env::var(CELL) else {
        for entry in ["dcserver", "runserver", "cli"] {
            for scenario in ["first", "refused", "removed-agent", "unreadable", "no-pool"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "bootstrap::tests::boot_entries_select_approved_hosts_pg",
                        "--nocapture",
                    ])
                    .env(CELL, format!("{entry}:{scenario}"))
                    .env_remove("AGENTDESK_CLAUDE_SUBMIT_GUARD_CHANNEL_IDS")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{entry}:{scenario}\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
            }
        }
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let (entry, scenario) = cell.split_once(':').unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let old_config = config(&root, "old.sock");
        let mut candidate = old_config.clone();
        let old = BootSessionHosts::from_config(&old_config).unwrap();
        let store = FileApprovalStore::runtime();
        if matches!(scenario, "refused" | "removed-agent" | "no-pool") {
            store.save(&old).unwrap();
            candidate = config(&root, "new.sock");
        }
        if matches!(scenario, "refused" | "removed-agent") {
            seed(&pool, &root, HostedState::Bound, true, 41).await;
        }
        if scenario == "removed-agent" {
            candidate.agents.clear();
            candidate.session_hosts.herdr.channels.clear();
        }
        if scenario == "unreadable" {
            std::fs::create_dir_all(store.0.as_ref().unwrap()).unwrap();
            std::fs::write(store.0.as_ref().unwrap().join("current.json"), "{").unwrap();
        }
        let passed_pool = (scenario != "no-pool").then_some(&pool);
        if entry == "cli" {
            crate::cli::herdr::install_observation_hosts(&candidate, &pool)
                .await
                .unwrap();
        } else if entry == "dcserver" && scenario != "no-pool" {
            crate::bootstrap::prepare_dcserver_boot(&candidate, &pool).await;
        } else if entry == "runserver" {
            crate::launch::after_database_ready(&candidate, passed_pool, || async { Ok(()) })
                .await
                .unwrap();
        } else {
            crate::bootstrap::prepare_and_install_boot_snapshots(&candidate, passed_pool)
                .await
                .unwrap();
        }
        if scenario == "unreadable" {
            use crate::services::turn_host::{HerdrRefusal, TurnHost, for_turn};
            let mut tmux = 0;
            let result = for_turn(
                Some(&pool),
                &crate::services::provider::ProviderKind::Claude,
                41,
                Some("no-such-session"),
            )
            .await;
            if matches!(result, TurnHost::Tmux) {
                tmux += 1;
            }
            assert!(
                matches!(
                    result,
                    TurnHost::Refused(HerdrRefusal::AdmissionStopped {
                        cause: crate::services::herdr_admission::StopCause::ApprovalSnapshot
                    })
                ),
                "{result:?}"
            );
            assert_eq!(
                tmux, 0,
                "an unreadable approval cannot send a new channel to tmux"
            );
        }
        session_hosts::with_boot(|boot| {
            let boot = boot.unwrap();
            assert_eq!(
                boot.channels(),
                old.channels(),
                "installed registry differs from the pre-change registry"
            );
            assert_eq!(boot.config(), old.config());
            assert_eq!(boot.local_node(), old.local_node());
            #[cfg(unix)]
            {
                let before = crate::services::session_host::HerdrRegistry::build(&old);
                let after = crate::services::session_host::HerdrRegistry::build(boot);
                assert_eq!(before.is_empty(), after.is_empty());
                assert_eq!(before.sole_host().is_some(), after.sole_host().is_some());
                let mut row = record(&owner("41"), "registry", HostedState::Bound);
                row.location.as_mut().unwrap().socket_addr =
                    root.join("old.sock").display().to_string();
                assert!(before.view(&row).is_some());
                assert!(after.view(&row).is_some());
            }
        });
        let health = herdr_health(&candidate, &pool).await;
        assert_eq!(health["restart_required"], false);
        match scenario {
            "first" => {
                assert!(health.get("migration").is_none());
                assert_eq!(
                    store.0.as_ref().unwrap().join("current.json").exists(),
                    entry != "cli",
                    "the read-only CLI never seeds approval"
                );
            }
            "refused" | "removed-agent" => {
                let reason = if entry == "cli" {
                    "session_hosts_migration_pending"
                } else {
                    "session_hosts_migration_refused"
                };
                assert_eq!(health["migration_refused"], entry != "cli");
                assert_eq!(health["migration"]["reason"], reason);
                let before = std::fs::read(store.0.as_ref().unwrap().join("current.json")).unwrap();
                let cli = migration::select(
                    BootSessionHosts::from_config(&candidate).unwrap(),
                    &store,
                    &PgRows(Some(&pool)),
                    &NoHoldStore,
                    true,
                )
                .await
                .into_hosts();
                assert_eq!(
                    cli.channels(),
                    old.channels(),
                    "read-only CLI must follow the approved endpoint"
                );
                assert_eq!(
                    std::fs::read(store.0.as_ref().unwrap().join("current.json")).unwrap(),
                    before
                );
                candidate
                    .session_hosts
                    .herdr
                    .endpoints
                    .get_mut("herdr.default")
                    .unwrap()
                    .herdr_session
                    .push_str("-edited");
                assert_eq!(
                    herdr_health(&candidate, &pool).await["restart_required"],
                    true
                );
            }
            "unreadable" => {}
            "no-pool" => assert_eq!(
                health["migration"]["reason"],
                if entry == "cli" {
                    "session_hosts_migration_pending"
                } else {
                    "session_hosts_migration_unverified"
                }
            ),
            _ => unreachable!(),
        }
        pool.close().await;
        db.drop().await;
    });
}

// SQL must include records that have no location, run on another node, or cannot be decoded.
#[tokio::test]
async fn migration_row_query_preserves_all_channel_records_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(
        &pool,
        std::path::Path::new("/fixture"),
        HostedState::Pending,
        false,
        41,
    )
    .await;
    seed(
        &pool,
        std::path::Path::new("/fixture"),
        HostedState::Bound,
        true,
        42,
    )
    .await;
    sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{location,execution_node}', '\"remote-node\"'::jsonb) WHERE channel_id = '42'").execute(&pool).await.unwrap();
    seed(
        &pool,
        std::path::Path::new("/fixture"),
        HostedState::Retired,
        true,
        43,
    )
    .await;
    sqlx::query(
        "UPDATE sessions SET hosted_execution = '{\"schema\":999}'::jsonb WHERE channel_id = '43'",
    )
    .execute(&pool)
    .await
    .unwrap();
    seed(
        &pool,
        std::path::Path::new("/fixture"),
        HostedState::Retired,
        true,
        44,
    )
    .await;
    sqlx::query("UPDATE sessions SET discord_token_hash = 'discord_fedcba9876543210' WHERE channel_id = '44'")
        .execute(&pool).await.unwrap();
    let rows = PgRows(Some(&pool))
        .rows(&[41, 42, 43, 44].into())
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.channel)
            .collect::<std::collections::BTreeSet<_>>(),
        [41, 42, 43, 44].into()
    );
    assert!(matches!(&rows[0].record, HostedRecord::Known(row) if row.location.is_none()));
    assert!(
        matches!(&rows[1].record, HostedRecord::Known(row) if row.location.as_ref().unwrap().execution_node == "remote-node")
    );
    assert!(matches!(rows[2].record, HostedRecord::Unknown(_)));
    assert!(
        matches!(rows[3].record, HostedRecord::Unknown(_)),
        "an internally valid Retired payload with a different row owner must remain protected"
    );
    pool.close().await;
    db.drop().await;
}
