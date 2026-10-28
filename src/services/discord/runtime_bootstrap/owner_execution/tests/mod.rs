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
