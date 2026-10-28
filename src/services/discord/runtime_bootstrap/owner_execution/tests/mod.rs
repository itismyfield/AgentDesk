use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::{channel_policy::BootChannels, ownership::GatewayOwnership};

#[tokio::test]
async fn owner_bootstrap_pg_snapshot_precedes_restore_for_all_runtime_roles() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_bootstrap_pg_snapshot_precedes_restore_for_all_runtime_roles"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73368101;
    execution::tests::admission(&pool, "claude", channel, "mac-a").await;
    let config:crate::config::Config=serde_json::from_value(serde_json::json!({"server":{},
        "cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-b"},
        "agents":[{"id":"custody-agent","name":"owner","channels":{"claude":{"id":channel.to_string(),"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel]}}})).unwrap();
    let _channels = cutover::test_override::force_boot(BootChannels::validate(&config).unwrap());
    for role in ["gateway", "standby", "worker"] {
        prepare(&ProviderKind::Claude, Some(&pool)).await;
        let view = cutover::boot_snapshot()
            .unwrap()
            .execution(channel)
            .expect(role);
        let permit = view.permit.expect(role);
        assert_eq!(
            permit.gate().current(),
            GatewayOwnership::ExecutionOwned {
                epoch: 1,
                generation: 0
            }
        );
        assert!(
            cutover::boot_ownership()
                .iter()
                .any(|(id, _, candidate)| *id == channel && candidate.is_some()),
            "{role} restore must see the PG execution owner"
        );
    }
    pool.close().await;
    prepare(&ProviderKind::Claude, Some(&pool)).await;
    assert!(
        cutover::boot_snapshot()
            .unwrap()
            .execution(channel)
            .unwrap()
            .permit
            .is_err()
    );
    assert!(
        cutover::boot_ownership()
            .iter()
            .all(|(_, _, candidate)| candidate.is_none()),
        "PG read failure must close restoration"
    );
}

#[test]
fn owner_bootstrap_read_is_wired_before_shared_watchers_and_role_branches() {
    let source = include_str!("../../../runtime_bootstrap.rs");
    let read = source
        .find("owner_execution::prepare(&provider, pg_pool.as_ref()).await;")
        .unwrap();
    for after in [
        "let shared = run_bot_build_shared_data(",
        "super::tui_prompt_relay::spawn_tui_prompt_relay(",
        "if !modules.gateway {",
        "GatewayLeaseOutcome::Standby =>",
    ] {
        assert!(
            read < source.find(after).unwrap(),
            "PG execution snapshot must precede {after}"
        );
    }
}

#[tokio::test]
async fn owner_cleanup_boot_diagnostics_wait_for_execution_read_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_boot_diagnostics_wait_for_execution_read_pg"
    )) {
        return;
    }
    use crate::services::tui_o::{
        alarm,
        topology::{HostRole, gateway_only_channels},
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (offset, mode) in ["local", "unmanaged", "foreign", "database"]
        .into_iter()
        .enumerate()
    {
        let channel = 73369110 + offset as u64;
        if mode != "unmanaged" {
            execution::tests::admission(
                &pool,
                "claude",
                channel,
                if mode == "foreign" { "mac-b" } else { "mac-a" },
            )
            .await;
        }
        let config:crate::config::Config=serde_json::from_value(serde_json::json!({"server":{},
            "cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-b"},
            "agents":[{"id":"custody-agent","name":"owner","channels":{"claude":{"id":channel.to_string(),"runtime":"tui"}}}],
            "tui_o":{"writer":{"channels":[channel]}}})).unwrap();
        let root = crate::config::runtime_root().unwrap();
        let store = crate::services::tui_o::store::OStore::open_if_enabled(
            &crate::services::tui_o::store::StoreConfig { enabled: true },
            &root,
        )
        .unwrap()
        .unwrap();
        store
            .init_channel(&crate::services::tui_o::store::Initialized {
                channel,
                sources: vec![],
                initial_anchor: 0,
                build_digest: "fixture".into(),
                at: chrono::Utc::now(),
            })
            .unwrap();
        let boot = BootChannels::validate(&config)
            .unwrap()
            .seeded(true, &config, |selected, _| {
                Ok(crate::services::tui_o::channel_policy::stored(
                    Some(&root),
                    selected,
                ))
            })
            .unwrap();
        let _channels = cutover::test_override::force_boot(boot);
        let broken = sqlx::PgPool::connect_lazy("postgres://invalid@127.0.0.1:1/unused").unwrap();
        broken.close().await;
        prepare(
            &ProviderKind::Claude,
            Some(if mode == "database" { &broken } else { &pool }),
        )
        .await;
        let snapshot = cutover::boot_snapshot().unwrap();
        let halted = alarm::health_reasons().contains(&format!("tui_o:halted:{channel}"));
        assert_eq!(halted, mode != "local", "{mode} foreign-store diagnostic");
        assert_eq!(
            gateway_only_channels(true, "claude", HostRole::Standby, Some(&snapshot)),
            Some(usize::from(mode != "local")),
            "{mode} gateway-only diagnostic"
        );
    }
    pool.close().await;
}
