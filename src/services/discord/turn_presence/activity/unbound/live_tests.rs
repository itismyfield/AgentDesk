use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::make_shared_data_for_tests;
use std::time::Duration;

pub(in crate::services::discord::turn_presence::activity) async fn verdict_for_tests(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<String, String> {
    let ports = Arc::new(LivePorts {
        root: crate::services::tui_prompt_dedupe::binding_events::test_root(),
    });
    let (snapshot, namespace, next) =
        Snapshot::collect_pair(shared, provider, channel, ports).await?;
    if !snapshot.same_inputs(&next) {
        return Err("superseded".into());
    }
    Ok(format!(
        "{:?}",
        snapshot.verdict(shared, &namespace.inventory)
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn s1_unbound_invalidates_before_waiting_for_core() {
    let _root = TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(69_181_600);
    let key = super::super::super::Key {
        channel: channel.get(),
        provider: ShadowProvider::Claude,
        session: "old-bound-worker".into(),
        seq: 1,
    };
    let watch = Arc::new(Mutex::new(Watch {
        key: Some(key.clone()),
        generation: 3,
        revision: 4,
        ..Watch::default()
    }));
    let core = shared.core.lock().await;
    let reading = read(&shared, &ProviderKind::Claude, channel, watch.clone());
    tokio::pin!(reading);
    tokio::select! {
        _ = &mut reading => panic!("read escaped a held core lock"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
    {
        let current = lock(&watch);
        assert!(
            current.key.is_none(),
            "old bound key survived the first await"
        );
        assert_eq!((current.generation, current.revision), (4, 5));
    }
    super::super::super::install(
        &watch,
        &key,
        3,
        super::super::super::Outcome::NoSource,
        None,
    );
    assert!(matches!(
        lock(&watch).outcome,
        super::super::super::Outcome::Rebuilding
    ));
    drop(core);
    let (answer, stamp) = reading.await;
    assert_eq!(answer.activity, Activity::Unknown);
    assert!(stamp.publish(&watch, || ()).await.is_some());
    assert!(
        lock(&watch).key.is_none(),
        "Unbound must not manufacture a Bound identity"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s1_unbound_old_publication_is_rejected_after_a_new_read() {
    let _root = TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(69_181_601);
    let watch = Arc::new(Mutex::new(Watch::default()));
    let (_, first) = read(&shared, &ProviderKind::Claude, channel, watch.clone()).await;
    assert!(first.publish(&watch, || ()).await.is_some());
    let (_, second) = read(&shared, &ProviderKind::Claude, channel, watch.clone()).await;
    assert!(first.publish(&watch, || ()).await.is_none());
    assert!(second.publish(&watch, || ()).await.is_some());
    let core = shared.core.lock().await;
    assert!(
        second.publish(&watch, || ()).await.is_none(),
        "unverifiable core must fail closed"
    );
    drop(core);
    assert!(second.publish(&watch, || ()).await.is_some());
}

#[test]
fn s1_unbound_marker_collection_does_not_create_runtime_directories() {
    let _root = TestRuntimeRootGuard::new();
    let path = tmux_common::persistent_sessions_dir().unwrap();
    assert!(!path.exists());
    let trace = local_trace("s1-never-created");
    assert_eq!(trace.marker, HostKindMarker::Absent);
    assert_eq!(trace.runtime, Ok(None));
    assert!(
        !path.exists(),
        "a read-only absence probe created the sessions directory"
    );
}

#[test]
fn s1_unbound_registration_reads_preserve_precedence_and_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let _root = TestRuntimeRootGuard::new();
    let role = runtime_store::role_map_path().unwrap();
    std::fs::create_dir_all(role.parent().unwrap()).unwrap();
    std::fs::write(&role, br#"{"byChannelId":{"69181602":{"roleId":"role","promptFile":"p","provider":"claude"},"69181603":{"roleId":"role","promptFile":"p","provider":"codex"}}}"#).unwrap();
    let org = runtime_store::org_schema_path().unwrap();
    std::fs::create_dir_all(org.parent().unwrap()).unwrap();
    std::fs::write(&org, "version: 1\nagents:\n  role:\n    display_name: Role\n    provider: codex\nchannels:\n  by_id:\n    '69181602':\n      agent: role\n").unwrap();
    let config = Config::default();
    let path = crate::runtime_layout::config_file_path(&crate::config::runtime_root().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let bindings = registrations().unwrap();
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings[0].channel_id, 69_181_602);
    assert_eq!(bindings[0].owner_provider, ProviderKind::Codex);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    std::fs::write(&org, "corrupt: [").unwrap();
    assert!(
        registrations().is_err(),
        "a corrupt registration must not become empty absence"
    );
}
