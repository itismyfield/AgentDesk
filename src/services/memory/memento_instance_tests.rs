use super::*;
use std::net::TcpListener;

pub(in crate::services::memory) fn with_configured_memento(
    test: &str,
    check: impl FnOnce(&TcpListener, &Path),
) {
    if !crate::services::provider_auth_profile::instance_env_tests::instance_child(test) {
        return;
    }
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    crate::config::test_instance::initialize().unwrap();
    let instance = crate::config::test_instance::current().unwrap().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let key_name = "I2B_SYNTHETIC_MEMENTO_KEY";
    let _key = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        key_name,
        std::ffi::OsStr::new("fixture-only"),
    );
    let memory = json!({"version":2,"backend":"memento",
        "mcp":{"endpoint":endpoint,"access_key_env":key_name}});
    let path = instance.root.join("config/agentdesk.yaml");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["memory"] = memory.clone();
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::write(
        runtime_layout::memory_backend_path(&instance.root),
        serde_json::to_vec(&memory).unwrap(),
    )
    .unwrap();
    // Both the canonical declaration and its supported JSON fallback name the private sink.
    let loaded = runtime_layout::load_memory_backend(&instance.root);
    assert_eq!(loaded.backend, "memento");
    assert_eq!(loaded.mcp.endpoint, endpoint);
    assert_eq!(loaded.mcp.access_key_env, key_name);
    check(&listener, &instance.root);
}

#[test]
fn test_instance_memento_runtime_config_rejects_post_startup_configuration() {
    with_configured_memento(
        concat!(
            module_path!(),
            "::test_instance_memento_runtime_config_rejects_post_startup_configuration"
        ),
        |listener, _root| {
            let backend = MementoBackend::new(ResolvedMemorySettings {
                backend: crate::domain::memory::MemoryBackendKind::Memento,
                ..Default::default()
            });
            assert!(matches!(
                backend.runtime_config(),
                Err(error) if error == "Memento is unavailable in a test instance"
            ));
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        },
    );
}

#[test]
fn test_instance_memento_anchor_load_never_connects_after_configuration_drift() {
    with_configured_memento(
        concat!(
            module_path!(),
            "::test_instance_memento_anchor_load_never_connects_after_configuration_drift"
        ),
        |listener, root| {
            let settings = ResolvedMemorySettings {
                memory_group: Some("test-instance".into()),
                backend: crate::domain::memory::MemoryBackendKind::Memento,
                recall_timeout_ms: 50,
                ..Default::default()
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let prompt = runtime.block_on(load_session_anchor_prompt(SessionAnchorRequest {
                settings: &settings,
                provider: &crate::services::provider::ProviderKind::Claude,
                current_path: root.to_str().unwrap(),
                channel_id: 900004,
                memory_scope_channel_id: 900004,
                role_binding: None,
                session_id: Some("synthetic-test-instance-session"),
                fresh: true,
            }));
            assert!(prompt.is_none());
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        },
    );
}
