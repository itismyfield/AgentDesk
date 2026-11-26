use super::*;
use crate::services::session_host::RestoreResume;
use crate::services::session_host::herdr_for_test::model::HerdrEndpoint;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};

#[derive(Debug, Clone, Copy)]
enum Snapshot {
    Missing,
    ReplacedLegacyMissing,
    ReplacedUserDefault,
    WitnessChangedDuringRead,
    Unreadable,
}

fn placed_recovery(snapshot: Snapshot) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let rig = HerdrRig::start();
    let mut pending = record(&rig);
    pending.state = HostedState::Pending;
    pending.expected = None;
    pending.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    pending.owner.runtime_root = crate::config::runtime_root().unwrap().display().to_string();
    pending
        .source_ref
        .runtime_root
        .clone_from(&pending.owner.runtime_root);
    pending
        .source_ref
        .execution_nonce
        .clone_from(&pending.execution_nonce);
    let channel = pending.owner.channel_id.parse::<u64>().unwrap();
    PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: pending.owner.provider.clone(),
        created_at: chrono::Utc::now(),
        execution_nonce: pending.execution_nonce.clone(),
        tmux_session: pending.owner.logical_key.clone(),
        channel_id: Some(channel),
        owner_runtime_root: pending.owner.runtime_root.clone(),
        host: Some("herdr".into()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
        resume_start: None,
    })
    .unwrap();
    let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {}, "cluster": {"instance_id": NODE},
        "agents": [{"id": "placed", "name": "Placed", "channels": {
            "claude": {"id": channel.to_string(), "runtime": "tui"}
        }}],
        "session_hosts": {"herdr": {
            "endpoints": { KEY: {"execution_node": NODE, "socket_path": rig.socket(),
                "herdr_home": "/adk/herdr", "herdr_session": SESSION}},
            "channels": {channel.to_string(): KEY}
        }}
    }))
    .unwrap();
    let _hosts = crate::config::session_hosts::force_config_for_test(&cfg);
    let mut endpoint = HerdrEndpoint::new(NODE, KEY, rig.socket(), SESSION).unwrap();
    if matches!(snapshot, Snapshot::ReplacedUserDefault) {
        endpoint = endpoint.with_mode(crate::config::session_hosts::EndpointMode::UserDefault);
    }
    let _registry = rig.policy_registry_on_this_thread(endpoint, |mut host| {
        host.set_read_restore_for_test(|transport, _| RestoreResume::Off {
            witness: crate::services::session_host::herdr_for_test::contract::HerdrTransport::server_witness(transport).unwrap(),
        });
        host
    });
    let (db, pool) = runtime.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,$4)")
            .bind(&pending.owner.logical_key)
            .bind(&pending.owner.discord_token_hash)
            .bind(&pending.owner.channel_id)
            .bind(serde_json::to_value(&pending).unwrap())
            .execute(&pool).await.unwrap();
        (db, pool)
    });
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    std::sync::Arc::get_mut(&mut shared)
        .unwrap()
        .token_hash
        .clone_from(&pending.owner.discord_token_hash);
    rig.show_panes(&[]);
    match snapshot {
        Snapshot::Missing => {}
        Snapshot::ReplacedLegacyMissing => rig.serve_as(&[19]),
        Snapshot::WitnessChangedDuringRead => rig.serve_as(&[7, 19]),
        Snapshot::ReplacedUserDefault => {
            let host = crate::services::claude::herdr_turn::boot_launch_host().unwrap();
            let launch = crate::services::turn_host::launch_endpoint(
                crate::config::session_hosts::herdr_endpoint(channel).unwrap(),
            );
            let original = host.current_witness(&launch).unwrap();
            let grant = crate::services::session_host::policy::permit::grant_for_test(
                &launch,
                &pending.execution_nonce,
                original,
            );
            runtime.block_on(host.grants().save(grant.clone())).unwrap();
            host.remember_grant(grant);
            rig.serve_as(&[19]);
        }
        Snapshot::Unreadable => rig.answer("session.snapshot", serde_json::json!({"type": "ok"})),
    }
    crate::services::herdr_launch::unplaced::QUIET
        .with(|quiet| quiet.set(Some(std::time::Duration::ZERO)));
    reconnect_restarted_herdr_with_shared(&shared, &ProviderKind::Claude);
    crate::services::herdr_launch::unplaced::QUIET.with(|quiet| quiet.set(None));
    let loaded: serde_json::Value = runtime
        .block_on(
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
                .bind(&pending.owner.channel_id)
                .fetch_one(&pool),
        )
        .unwrap();
    let HostedRecord::Known(row) = HostedRecord::decode(Some(&loaded)) else {
        panic!("the placed row disappeared");
    };
    let expected = match snapshot {
        Snapshot::Missing | Snapshot::ReplacedLegacyMissing => HostedState::Retired,
        _ => HostedState::Pending,
    };
    assert_eq!(row.state, expected, "{snapshot:?}");
    assert_eq!(row.execution_nonce, pending.execution_nonce);
    let requests = rig.requests();
    assert_eq!(
        requests
            .iter()
            .any(|request| request["method"] == "session.snapshot"),
        !matches!(snapshot, Snapshot::ReplacedUserDefault),
        "the grant gate precedes the snapshot",
    );
    assert!(requests.iter().all(|request| matches!(
        request["method"].as_str(),
        Some("session.snapshot" | "pane.process_info" | "ping")
    )));
    runtime.block_on(async {
        pool.close().await;
        db.drop().await;
    });
}

#[test]
fn s2b1_boot_adapter_retires_an_already_missing_pending_pane_pg() {
    placed_recovery(Snapshot::Missing);
}

#[test]
fn s2b1_legacy_replacement_missing_retires_the_pending_pane_pg() {
    placed_recovery(Snapshot::ReplacedLegacyMissing);
}

#[test]
fn s2b1_boot_adapter_keeps_a_pending_pane_on_an_unreadable_snapshot_pg() {
    placed_recovery(Snapshot::Unreadable);
}

#[test]
fn s2b1_boot_adapter_holds_when_the_snapshot_answers_on_a_different_witness_pg() {
    placed_recovery(Snapshot::WitnessChangedDuringRead);
}

#[test]
fn s2b1_user_default_replacement_holds_the_placed_pending_pg() {
    placed_recovery(Snapshot::ReplacedUserDefault);
}
