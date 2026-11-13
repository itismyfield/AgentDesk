//! A recorded thread child through the real boot seed, the 2 s refresh step and the writer host.

use super::*;
use crate::services::tui_o::channel_policy::Adoption;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};

/// Boots a home whose store holds PARENT's init and CHILD's init recorded under PARENT.
fn booted(root: &std::path::Path, parent: u64, child: u64) {
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root)
        .unwrap()
        .unwrap();
    store
        .begin_fixture_era(&[parent, child], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                policy_parent: None,
                channel,
                sources: vec![],
                initial_anchor: 100,
                build_digest: "test".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let path = root.join("o_store").join(child.to_string()).join("init");
    let mut init: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    init["policy_parent"] = parent.into();
    std::fs::write(&path, serde_json::to_vec(&init).unwrap()).unwrap();
    let config = serde_json::from_value(json!({"server":{},
        "cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-a"},
        "agents":[{"id":"custody-agent","name":"fixture","channels":{"claude":{"id":parent.to_string(),"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[parent]}}})).unwrap();
    crate::services::tui_o::channel_policy::install(&config).unwrap();
}

#[tokio::test]
async fn thread_child_boot_record_is_hosted_by_the_refresh_step_under_its_parents_allowance_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::thread_child_boot_record_is_hosted_by_the_refresh_step_under_its_parents_allowance_pg"
    )) {
        return;
    }
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use crate::services::tui_o::cutover::{BodyClaim, BodySend, BodySite, claim_then_send_at};
    let root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (parent, child) = (73_369_611, 73_369_612);
    booted(root.path(), parent, child);
    let snapshot = cutover::boot_snapshot().unwrap();
    // Only the parent is allowed and selected: the child is served under that allowance.
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, parent, "gateway").await;
    let state = discord_mock::DiscordMockState::new();
    state.allow_channel(parent);
    state.allow_channel(child);
    let (proxy, gateway, _server) = discord_mock::start(state).await;
    let context = discord_mock::serenity_context(proxy, gateway).await;
    assert!(shared.http.cached_serenity_ctx.set(context).is_ok());
    crate::services::tui_o::ownership::gate("claude").acquired();
    super::super::super::super::owner_execution::prepare(&ProviderKind::Claude, Some(&pool)).await;
    let ready = host::process_readiness();
    assert!(!ready.is_hosted(child));
    assert!(
        matches!(
            for_input_placement("claude", child).await,
            IntakeRoute::Hold(_)
        ),
        "a child O owns waits for its own writer instead of a Legacy placement"
    );
    super::super::super::super::owner_cleanup::refresh_writers(
        &shared,
        &ProviderKind::Claude,
        Some(&TuiOConfig::default()),
    )
    .await;
    let accepted = tokio::time::timeout(Duration::from_secs(12), async {
        while !(host::channel_accepts(child) && host::channel_accepts(parent)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    assert!(
        accepted.await.is_ok(),
        "hosted={:?} health={:?}",
        ready.hosted_by(child),
        crate::services::tui_o::alarm::health_reasons()
    );
    assert_ne!(ready.hosted_by(child), ready.hosted_by(parent));
    assert!(
        snapshot.execution(child).is_none() && snapshot.execution(parent).is_none(),
        "a gateway-hosted child borrows no permit"
    );
    assert_eq!(
        for_input_placement("claude", child).await,
        IntakeRoute::Gateway
    );
    let mut sends = 0;
    let claim = Some(BodyClaim::new(child, Some(ClaudeTui)));
    let sent = claim_then_send_at(BodySite::WatcherTerminal, claim, || async { sends += 1 }).await;
    assert!(matches!(sent, Ok(BodySend::OwnedByO)));
    assert_eq!(sends, 0);
    pool.close().await;
}

#[tokio::test]
async fn thread_child_execution_view_is_its_own_permit_over_its_one_adoption_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::thread_child_execution_view_is_its_own_permit_over_its_one_adoption_pg"
    )) {
        return;
    }
    use crate::services::tui_o::ownership::GatewayOwnership;
    let root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (parent, child) = (73_369_621, 73_369_622);
    execution::tests::admission(&pool, "claude", child, "mac-a").await;
    booted(root.path(), parent, child);
    let snapshot = cutover::boot_snapshot().unwrap();
    super::super::super::super::owner_execution::prepare(&ProviderKind::Claude, Some(&pool)).await;
    let view = snapshot
        .execution(child)
        .expect("the child is read for its own PG execution owner");
    let owned = GatewayOwnership::ExecutionOwned {
        epoch: 1,
        generation: 0,
    };
    assert_eq!(view.permit.expect("child permit").gate().current(), owned);
    assert!(
        snapshot.execution(parent).is_none(),
        "the parent neither lends nor receives the child's permit"
    );
    // The view holds the shared entry's one adoption, not a regenerated one.
    *snapshot.child(child).unwrap().unwrap().candidate.lock() = Adoption::Held;
    assert_eq!(view.candidate.peek(), Adoption::Held);
    *view.candidate.lock() = Adoption::Committed;
    // The refresh step observes the child too: a failed read pauses it, a good one resumes it.
    let refresh = super::super::super::super::owner_execution::refresh;
    refresh(&ProviderKind::Claude, Some(&closed_pool().await)).await;
    assert!(snapshot.execution(child).unwrap().permit.is_err());
    refresh(&ProviderKind::Claude, Some(&pool)).await;
    // The paused child resumes on its own gate, under that gate's next epoch.
    let resumed = snapshot.execution(child).unwrap().permit.unwrap();
    assert!(matches!(
        resumed.gate().current(),
        GatewayOwnership::ExecutionOwned { epoch: 2, .. }
    ));
    assert!(
        cutover::boot_ownership()
            .iter()
            .any(|(id, _, candidate)| *id == child && candidate.is_some()),
        "the child's execution owner hosts its writer"
    );
    // A non-gateway role hosts the child under the child's permit, with no gateway lease.
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, parent, "worker").await;
    spawn_execution(&shared, &ProviderKind::Claude, Some(&TuiOConfig::default()));
    assert!(host::process_readiness().is_hosted(child));
    assert!(!host::process_readiness().is_hosted(parent));
    pool.close().await;
}
