use super::*;

#[tokio::test]
async fn owner_host_resident_refresh_recovers_after_real_lock_timeout_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_host_resident_refresh_recovers_after_real_lock_timeout_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369490;
    let _selected = select(&ProviderKind::Claude, channel, "mac-b", "mac-a");
    let delivery = execution::tests::admission(&pool, "claude", channel, "mac-b").await;
    let snapshot = cutover::boot_snapshot().unwrap();
    let permit = execution::read(&pool, "claude", channel, "mac-b", || Ok(false))
        .await
        .unwrap()
        .unwrap();
    let gate = permit.gate();
    snapshot.install_execution(channel, Some(Ok(permit)));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let discord = Discord::with_hold(Some((entered.clone(), release.clone()))).await;
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, channel, "standby").await;
    let ready = host::process_readiness();
    let mut prepared = parts_with_rest(
        &shared,
        &ProviderKind::Claude,
        None,
        Some(discord.http.clone()),
    );
    prepared.readiness = ready.clone();
    let io = prepared.io.clone();
    let mut handles = host::start_managed(
        ShadowProvider::Claude,
        false,
        cutover::boot_ownership(),
        || prepared,
    );
    assert_eq!(handles.len(), 1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.accepts(channel) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let hosted = ready.hosted_by(channel).unwrap();
    let original = origin(&pool, "claude", channel, "mac-b", delivery).await;
    snapshot.install_execution(
        channel,
        Some(Ok(execution::read(
            &pool,
            "claude",
            channel,
            "mac-b",
            || Ok(false),
        )
        .await
        .unwrap()
        .unwrap())),
    );
    let port = io.port_for(channel).await.unwrap();
    let first_port = port.clone();
    let first_origin = original.clone();
    let post = tokio::spawn(async move {
        first_port
            .post_origin(channel, "first".into(), Some(first_origin.seal("first")))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    super::super::super::owner_execution::refresh(&ProviderKind::Claude, Some(&pool)).await;
    assert!(snapshot.execution(channel).unwrap().permit.is_err());
    assert!(!ready.accepts(channel));
    assert_eq!(ready.hosted_by(channel), Some(hosted));
    release.add_permits(1);
    assert!(matches!(post.await.unwrap(), PostOutcome::Created(_)));
    super::super::super::owner_execution::refresh(&ProviderKind::Claude, Some(&pool)).await;
    let recovered = snapshot.execution(channel).unwrap().permit.unwrap();
    assert!(
        Arc::ptr_eq(&gate, &recovered.gate()),
        "the same tuple must resume the actor's original gate"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.accepts(channel) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the existing actor must publish resumed readiness");
    assert_eq!(ready.hosted_by(channel), Some(hosted));
    assert!(matches!(
        port.post_origin(channel, "second".into(), Some(original.seal("second")))
            .await,
        PostOutcome::Created(_)
    ));
    assert_eq!(discord.posts.lock().unwrap().len(), 2);
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE intake_session_owners SET status='superseded' WHERE provider='claude' AND raw_channel_id=$1 AND status='active'").bind(channel.to_string()).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO intake_session_owners(provider,raw_channel_id,owner_instance_id,generation,status) VALUES('claude',$1,'mac-b',1,'active')").bind(channel.to_string()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    super::super::super::owner_execution::refresh(&ProviderKind::Claude, Some(&pool)).await;
    assert!(
        !ready.is_hosted(channel),
        "a changed identity must stop the old host and return its claim"
    );
    assert!(matches!(
        port.post_origin(channel, "stale".into(), Some(original.seal("stale")))
            .await,
        PostOutcome::Withheld(_)
    ));
    assert_eq!(discord.posts.lock().unwrap().len(), 2);
    handles.pop().unwrap().stop_and_join().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn owner_refresh_readmits_live_input_after_owner_change_on_gateway_host_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_refresh_readmits_live_input_after_owner_change_on_gateway_host_pg"
    )) {
        return;
    }
    use crate::services::tui_o::channel_policy::Adoption;
    use crate::services::tui_o::cutover::intake_route::for_input_placement;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369492;
    let config=serde_json::from_value(json!({"server":{},"cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-a"},
        "agents":[{"id":"custody-agent","name":"fixture","channels":{"claude":{"id":channel.to_string(),"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel]}}})).unwrap();
    let _selected = cutover::test_override::force_boot(
        BootChannels::validate(&config)
            .unwrap()
            .adopted(Adoption::Pending),
    );
    let snapshot = cutover::boot_snapshot().unwrap();
    // O home boot while the channel is unmanaged: the gateway writer hosts it under the lease.
    let gate = crate::services::tui_o::ownership::gate("claude");
    gate.acquired();
    let source_root = tempfile::tempdir().unwrap();
    let transcript = source_root.path().join("live.jsonl");
    std::fs::write(&transcript, b"").unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("live", &transcript).unwrap();
    let io = host::test_io::TestHost::new([(channel, source)]);
    let ready = host::process_readiness();
    let (runtime_root, readiness) = (source_root.path().to_path_buf(), ready.clone());
    let parts = move || host::HostParts {
        io,
        runtime_root: Some(runtime_root),
        gate,
        readiness,
    };
    let _handles = host::start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        parts,
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.accepts(channel) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // A routed input makes the channel managed; the 2s loop then installs this node's permit.
    execution::tests::admission(&pool, "claude", channel, "mac-a").await;
    let refresh =
        || super::super::super::owner_execution::refresh(&ProviderKind::Claude, Some(&pool));
    refresh().await;
    assert!(snapshot.execution(channel).unwrap().permit.is_ok());
    assert_eq!(
        for_input_placement("claude", channel).await,
        crate::services::tui_o::cutover::intake_route::IntakeRoute::Gateway
    );
    // The owner moves to a new generation that is still this node, as a fallback switch-back does.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE intake_session_owners SET status='superseded' WHERE provider='claude' AND raw_channel_id=$1 AND status='active'").bind(channel.to_string()).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO intake_session_owners(provider,raw_channel_id,owner_instance_id,generation,status) VALUES('claude',$1,'mac-a',1,'active')").bind(channel.to_string()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    for _ in 0..4 {
        refresh().await;
    }
    let held = snapshot.execution(channel).unwrap().permit.err();
    assert_eq!(
        held,
        None,
        "a valid local owner in PG must restore execution authority within a few refresh ticks; \
         admission now answers {:?}",
        for_input_placement("claude", channel).await
    );
    pool.close().await;
}
