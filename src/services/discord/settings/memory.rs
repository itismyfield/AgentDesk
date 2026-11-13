use super::*;

pub(crate) use crate::services::channel_roles::{
    memory_settings_for_role, resolve_agent_memory_settings,
};

pub(crate) fn memory_settings_for_binding(
    role_binding: Option<&RoleBinding>,
) -> ResolvedMemorySettings {
    crate::services::channel_roles::memory_settings_for_binding(
        role_binding.map(|binding| &binding.memory),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_4310_unbound_channel_resolves_global_memory_backend() {
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
        let _access_key_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "ISSUE_4310_MEMENTO_KEY",
            Path::new("configured"),
        );
        let config_path = runtime_layout::memory_backend_path(runtime_root.path());
        fs::create_dir_all(config_path.parent().expect("config dir")).expect("create config dir");

        fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "backend": "memento",
                "mcp": {
                    "endpoint": "http://127.0.0.1:4310",
                    "access_key_env": "ISSUE_4310_MEMENTO_KEY"
                }
            }))
            .expect("serialize memento config"),
        )
        .expect("write memento config");

        assert_eq!(
            memory_settings_for_binding(None).backend,
            MemoryBackendKind::Memento,
            "unbound channels must inherit an active global memento backend"
        );

        fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "backend": "file"
            }))
            .expect("serialize file config"),
        )
        .expect("write file config");

        assert_eq!(
            memory_settings_for_binding(None).backend,
            MemoryBackendKind::File,
            "unbound channels must inherit an explicitly configured global file backend"
        );
    }
}
