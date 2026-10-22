use super::*;
use std::{path::Path, sync::MutexGuard};
use tempfile::TempDir;

struct EnvVarGuard {
    key: String,
    previous_value: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    fn set_path(key: &str, path: &Path) -> Self {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_value = std::env::var_os(key);
        unsafe { std::env::set_var(key, path) };
        Self {
            key: key.to_string(),
            previous_value,
            _lock: lock,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous_value {
            Some(value) => unsafe { std::env::set_var(&self.key, value) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

// Probe candidates are the hashes of the provider's own runtimes in any role, each once.
#[tokio::test]
async fn registered_token_hashes_are_the_providers_own_in_any_role() {
    let registry = HealthRegistry::new();
    let runtime = |hash: &str| {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().token_hash = hash.to_string();
        shared
    };
    registry.register("claude".into(), runtime("h-b")).await;
    registry
        .register_standby("Claude".into(), runtime("h-a"))
        .await;
    registry
        .register_worker("claude".into(), runtime("h-b"))
        .await;
    registry.register("codex".into(), runtime("h-c")).await;
    let hashes = |provider| registry.registered_token_hashes(provider);
    assert_eq!(hashes(&ProviderKind::Claude).await, ["h-a", "h-b"]);
    assert_eq!(hashes(&ProviderKind::Codex).await, ["h-c"]);
    assert!(hashes(&ProviderKind::Qwen).await.is_empty());
}

fn write_test_bot_token(root: &Path, bot_name: &str, token: &str) {
    crate::runtime_layout::ensure_credential_layout(root).unwrap();
    let path = crate::runtime_layout::credential_token_path(root, bot_name);
    crate::utils::secret_file::write_secret_file(&path, format!("{token}\n"))
        .expect("write test bot token");
}

#[test]
fn reload_bot_tokens_reports_success_and_invalidates_user_id_cache() {
    let temp = TempDir::new().expect("temp runtime root");
    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    write_test_bot_token(temp.path(), "announce", "announce-token");
    write_test_bot_token(temp.path(), "notify", "notify-token");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let registry = HealthRegistry::new();
        *registry
            .utility_bot(UtilityBotRole::Announce)
            .user_id
            .lock()
            .await = Some((11, 0));
        *registry
            .utility_bot(UtilityBotRole::Notify)
            .user_id
            .lock()
            .await = Some((22, 0));

        let report = registry.reload_bot_tokens().await;

        assert!(report.runtime_root_available);
        assert!(report.any_reloaded);
        assert!(report.utility_bot_user_ids_invalidated);
        assert_eq!(report.announce.status, BotTokenReloadStatus::Reloaded);
        assert!(report.announce.reloaded);
        assert!(report.announce.user_id_cache_invalidated);
        assert_eq!(
            *registry
                .utility_bot(UtilityBotRole::Announce)
                .user_id
                .lock()
                .await,
            None
        );
        assert_eq!(report.notify.status, BotTokenReloadStatus::Reloaded);
        assert!(report.notify.reloaded);
        assert!(report.notify.user_id_cache_invalidated);
        assert_eq!(
            *registry
                .utility_bot(UtilityBotRole::Notify)
                .user_id
                .lock()
                .await,
            None
        );
        assert!(resolve_bot_http(&registry, "announce").await.is_ok());
        assert!(resolve_bot_http(&registry, "notify").await.is_ok());
    });
}

#[test]
fn reload_bot_tokens_keeps_previous_client_when_credential_is_missing() {
    let temp = TempDir::new().expect("temp runtime root");
    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    write_test_bot_token(temp.path(), "announce", "announce-token");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let registry = HealthRegistry::new();
        let first = registry.reload_bot_tokens().await;
        assert_eq!(first.announce.status, BotTokenReloadStatus::Reloaded);
        assert!(resolve_bot_http(&registry, "announce").await.is_ok());

        std::fs::remove_file(crate::runtime_layout::credential_token_path(
            temp.path(),
            "announce",
        ))
        .expect("remove announce token");
        let second = registry.reload_bot_tokens().await;

        assert_eq!(
            second.announce.status,
            BotTokenReloadStatus::MissingOrInvalid
        );
        assert!(!second.announce.reloaded);
        assert!(second.announce.previous_client_kept);
        assert!(resolve_bot_http(&registry, "announce").await.is_ok());
    });
}

#[tokio::test]
async fn utility_bot_user_id_cache_rejects_stale_http_after_reload() {
    let registry = HealthRegistry::new();
    let runtime = registry.utility_bot(UtilityBotRole::Announce);
    let old_http = Arc::new(serenity::Http::new("Bot old-token"));
    let new_http = Arc::new(serenity::Http::new("Bot new-token"));

    *runtime.http.lock().await = Some(old_http.clone());
    let old_generation = runtime.token_generation.load(Ordering::SeqCst);
    assert!(
        HealthRegistry::cache_utility_bot_user_id_if_current(
            &runtime.http,
            &runtime.user_id,
            &runtime.token_generation,
            old_generation,
            &old_http,
            11,
        )
        .await
    );
    assert_eq!(*runtime.user_id.lock().await, Some((11, old_generation)));

    runtime.token_generation.fetch_add(1, Ordering::SeqCst);
    *runtime.http.lock().await = Some(new_http);
    *runtime.user_id.lock().await = None;
    assert!(
        !HealthRegistry::cache_utility_bot_user_id_if_current(
            &runtime.http,
            &runtime.user_id,
            &runtime.token_generation,
            old_generation,
            &old_http,
            22,
        )
        .await
    );
    assert_eq!(*runtime.user_id.lock().await, None);
}
