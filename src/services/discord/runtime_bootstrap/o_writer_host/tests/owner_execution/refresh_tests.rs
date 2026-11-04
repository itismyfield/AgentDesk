use super::*;
use crate::services::tui_o::cutover::intake_route::{IntakeRoute, for_input_placement};

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
    assert!(matches!(
        for_input_placement("claude", channel).await,
        IntakeRoute::Hold(_)
    ));
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
    assert_eq!(
        for_input_placement("claude", channel).await,
        IntakeRoute::Gateway,
        "the same verified permit must readmit live input"
    );
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
async fn owner_refresh_readmits_live_input_after_one_transient_pg_failure_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_refresh_readmits_live_input_after_one_transient_pg_failure_pg"
    )) {
        return;
    }
    use crate::services::tui_o::channel_policy::Adoption;
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
    let refresh =
        || super::super::super::owner_execution::refresh(&ProviderKind::Claude, Some(&pool));
    refresh().await;
    assert!(
        snapshot.execution(channel).is_none(),
        "an unmanaged channel has no execution view"
    );
    assert_eq!(
        for_input_placement("claude", channel).await,
        crate::services::tui_o::cutover::intake_route::IntakeRoute::Gateway
    );
    #[derive(Clone, Default)]
    struct Warnings(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Warnings {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let warnings = Warnings::default();
    let sink = warnings.clone();
    crate::logging::test_capture::pin_callsite_interest();
    let _capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish(),
    );
    let logged = |message: &str| {
        String::from_utf8_lossy(&warnings.0.lock().unwrap())
            .matches(message)
            .count()
    };
    // Reads fail transiently for two ticks: a competing custody holder outlasts the 2s lock timeout.
    use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody};
    let mut competing = pool.begin().await.unwrap();
    custody::lock(
        &mut competing,
        &OwnerIdentity::new("claude", &channel.to_string()),
    )
    .await
    .unwrap();
    refresh().await;
    refresh().await;
    let paused = snapshot.execution(channel).map(|view| view.permit.err());
    assert!(
        matches!(&paused, Some(Some(reason)) if reason.contains("lock timeout")),
        "the failed read pauses admission: {paused:?}"
    );
    competing.rollback().await.unwrap();
    for _ in 0..4 {
        refresh().await;
    }
    // The 2s loop warns once per change, not once per tick.
    assert_eq!(logged("execution owner refresh read failed"), 1);
    assert_eq!(logged("execution identity changed; admission suspended"), 0);
    assert_eq!(
        for_input_placement("claude", channel).await,
        crate::services::tui_o::cutover::intake_route::IntakeRoute::Gateway,
        "PG answering again must readmit live input within a few refresh ticks"
    );
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_refresh_unmanaged_resident_readmits_after_transient_pg_failure_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_refresh_unmanaged_resident_readmits_after_transient_pg_failure_pg"
    )) {
        return;
    }
    use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody};
    use crate::services::tui_o::channel_policy::Adoption;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369493;
    let config = serde_json::from_value(json!({"server":{},"cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-a"},
        "agents":[{"id":"custody-agent","name":"fixture","channels":{"claude":{"id":channel.to_string(),"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel]}}})).unwrap();
    let _selected = cutover::test_override::force_boot(
        BootChannels::validate(&config)
            .unwrap()
            .adopted(Adoption::Pending),
    );
    let snapshot = cutover::boot_snapshot().unwrap();
    let source_root = tempfile::tempdir().unwrap();
    let transcript = source_root.path().join("live.jsonl");
    std::fs::write(&transcript, b"").unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("live", &transcript).unwrap();
    let io = host::test_io::TestHost::new([(channel, source)]);
    let (entered, inside) = tokio::sync::oneshot::channel();
    let (release, waiting) = std::sync::mpsc::channel();
    *io.on_facts.lock().unwrap() = Some(Box::new(move || {
        entered.send(()).unwrap();
        // Closing the sender also releases startup when a preceding assertion fails.
        let _ = waiting.recv();
    }));
    let gate = crate::services::tui_o::ownership::gate("claude");
    gate.acquired();
    let ready = host::process_readiness();
    let (runtime_root, readiness) = (source_root.path().to_path_buf(), ready.clone());
    let mut handles = host::start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        move || host::HostParts {
            io,
            runtime_root: Some(runtime_root),
            gate,
            readiness,
        },
    );
    assert_eq!(handles.len(), 1);
    tokio::time::timeout(Duration::from_secs(5), inside)
        .await
        .unwrap()
        .unwrap();
    let hosted = ready.hosted_by(channel);
    assert!(hosted.is_some());
    assert!(!ready.hosted_under(channel, &crate::services::tui_o::ownership::gate("claude")));
    let refresh =
        || super::super::super::owner_execution::refresh(&ProviderKind::Claude, Some(&pool));
    let identity = OwnerIdentity::new("claude", &channel.to_string());
    // Before any successful execution read, a failure must hold admission despite a host claim.
    let mut competing = pool.begin().await.unwrap();
    custody::lock(&mut competing, &identity).await.unwrap();
    refresh().await;
    assert!(snapshot.execution(channel).unwrap().verified().is_none());
    assert!(matches!(
        for_input_placement("claude", channel).await,
        IntakeRoute::Hold(_)
    ));
    competing.rollback().await.unwrap();
    refresh().await;
    assert!(snapshot.execution(channel).is_none());
    assert_eq!(ready.hosted_by(channel), hosted);
    assert!(
        !ready.accepts(channel),
        "startup is still held by the fixture"
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.accepts(channel) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(ready.hosted_under(channel, &crate::services::tui_o::ownership::gate("claude")));
    assert_eq!(
        for_input_placement("claude", channel).await,
        IntakeRoute::Gateway
    );
    assert_eq!(ready.hosted_by(channel), hosted);
    // Both one failure and repeated failures recover on their first successful read.
    for failures in [1, 2] {
        let mut competing = pool.begin().await.unwrap();
        custody::lock(&mut competing, &identity).await.unwrap();
        for _ in 0..failures {
            refresh().await;
            let view = snapshot.execution(channel).unwrap();
            assert!(view.permit.as_ref().unwrap_err().contains("lock timeout"));
            assert!(view.verified().is_none());
            assert!(matches!(
                for_input_placement("claude", channel).await,
                IntakeRoute::Hold(_)
            ));
            assert_eq!(ready.hosted_by(channel), hosted);
        }
        competing.rollback().await.unwrap();
        refresh().await;
        assert!(
            snapshot.execution(channel).is_none(),
            "Ok(None) must clear the temporary hold immediately"
        );
        assert_eq!(
            for_input_placement("claude", channel).await,
            IntakeRoute::Gateway
        );
        assert_eq!(ready.hosted_by(channel), hosted);
    }
    handles.pop().unwrap().stop_and_join().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn owner_refresh_stale_unmanaged_claim_releases_only_execution_hold_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_refresh_stale_unmanaged_claim_releases_only_execution_hold_pg"
    )) {
        return;
    }
    use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody};
    use crate::services::tui_o::channel_policy::Adoption;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369495;
    let config = serde_json::from_value(json!({"server":{},"cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-a"},
        "agents":[{"id":"custody-agent","name":"fixture","channels":{"claude":{"id":channel.to_string(),"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel]}}})).unwrap();
    let _selected = cutover::test_override::force_boot(
        BootChannels::validate(&config)
            .unwrap()
            .adopted(Adoption::Held),
    );
    let snapshot = cutover::boot_snapshot().unwrap();
    let source_root = tempfile::tempdir().unwrap();
    let transcript = source_root.path().join("live.jsonl");
    std::fs::write(&transcript, b"").unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("live", &transcript).unwrap();
    let io = host::test_io::TestHost::new([(channel, source)]);
    let (entered, inside) = tokio::sync::oneshot::channel();
    *io.on_facts.lock().unwrap() = Some(Box::new(move || {
        entered.send(()).unwrap();
        panic!("fixture host fails before its actor starts");
    }));
    let gate = crate::services::tui_o::ownership::gate("claude");
    gate.acquired();
    let ready = host::process_readiness();
    let (runtime_root, readiness) = (source_root.path().to_path_buf(), ready.clone());
    let mut handles = host::start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        move || host::HostParts {
            io,
            runtime_root: Some(runtime_root),
            gate,
            readiness,
        },
    );
    assert_eq!(handles.len(), 1);
    tokio::time::timeout(Duration::from_secs(5), inside)
        .await
        .unwrap()
        .unwrap();
    assert!(handles.pop().unwrap().stop_and_join().await.is_err());
    let stale = ready.hosted_by(channel);
    assert!(stale.is_some());
    assert!(!ready.hosted_under(channel, &crate::services::tui_o::ownership::gate("claude")));
    let refresh =
        || super::super::super::owner_execution::refresh(&ProviderKind::Claude, Some(&pool));
    let mut competing = pool.begin().await.unwrap();
    custody::lock(
        &mut competing,
        &OwnerIdentity::new("claude", &channel.to_string()),
    )
    .await
    .unwrap();
    refresh().await;
    assert!(snapshot.execution(channel).unwrap().permit.is_err());
    assert!(matches!(
        for_input_placement("claude", channel).await,
        IntakeRoute::Hold(_)
    ));
    competing.rollback().await.unwrap();
    for _ in 0..2 {
        refresh().await;
        assert!(
            snapshot.execution(channel).is_none(),
            "a stale host must not retain a recovered execution read's hold"
        );
        assert_eq!(ready.hosted_by(channel), stale);
        assert!(!ready.accepts(channel));
        assert!(matches!(
            for_input_placement("claude", channel).await,
            IntakeRoute::Hold(_)
        ));
    }
    pool.close().await;
}

#[tokio::test]
async fn owner_refresh_foreign_executor_after_transient_failure_stays_suspended_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_refresh_foreign_executor_after_transient_failure_stays_suspended_pg"
    )) {
        return;
    }
    use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody};
    use crate::services::tui_o::ownership::GatewayOwnership;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369494;
    let _selected = select(&ProviderKind::Claude, channel, "mac-b", "mac-a");
    execution::tests::admission(&pool, "claude", channel, "mac-b").await;
    let snapshot = cutover::boot_snapshot().unwrap();
    let permit = execution::read(&pool, "claude", channel, "mac-b", || Ok(false))
        .await
        .unwrap()
        .unwrap();
    let gate = permit.gate();
    snapshot.install_execution(channel, Some(Ok(permit)));
    let refresh =
        || super::super::super::owner_execution::refresh(&ProviderKind::Claude, Some(&pool));
    let mut competing = pool.begin().await.unwrap();
    custody::lock(
        &mut competing,
        &OwnerIdentity::new("claude", &channel.to_string()),
    )
    .await
    .unwrap();
    refresh().await;
    assert_eq!(gate.current(), GatewayOwnership::Unknown);
    assert!(snapshot.execution(channel).unwrap().verified().is_some());
    sqlx::query("UPDATE intake_session_owners SET owner_instance_id='mac-c',generation=generation+1 WHERE provider='claude' AND raw_channel_id=$1 AND status='active'")
        .bind(channel.to_string()).execute(&mut *competing).await.unwrap();
    competing.commit().await.unwrap();
    for _ in 0..2 {
        refresh().await;
        let view = snapshot.execution(channel).unwrap();
        assert!(
            view.permit
                .as_ref()
                .unwrap_err()
                .contains("channel execution owner is mac-c")
        );
        assert!(
            view.verified().is_none(),
            "a foreign owner must discard retained authority"
        );
        assert_eq!(gate.current(), GatewayOwnership::Lost);
        assert!(matches!(
            for_input_placement("claude", channel).await,
            IntakeRoute::Hold(_)
        ));
    }
    assert!(
        gate.reacquired().is_none(),
        "the old owner gate must remain fenced"
    );
    pool.close().await;
}
