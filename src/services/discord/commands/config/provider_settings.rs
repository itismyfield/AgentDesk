use super::*;
use std::io;

// Discord thread parents are immutable; retain only this lookup hint, never ownership.
static FEATURE_PARENTS: std::sync::LazyLock<
    dashmap::DashMap<serenity::ChannelId, serenity::ChannelId>,
> = std::sync::LazyLock::new(dashmap::DashMap::new);

impl SharedData {
    pub(in crate::services::discord) fn feature_settings_parent(
        channel: serenity::ChannelId,
    ) -> Option<serenity::ChannelId> {
        FEATURE_PARENTS.get(&channel).map(|parent| *parent)
    }

    pub(in crate::services::discord) async fn settings_provider(
        self: &Arc<Self>,
        channel: serenity::ChannelId,
        provider: &ProviderKind,
    ) -> ProviderKind {
        effective_provider_for_channel(self, channel, provider, None).await
    }

    pub(in crate::services::discord) async fn feature_settings_channel_id(
        self: &Arc<Self>,
        provider: &ProviderKind,
        channel_id: serenity::ChannelId,
        parent_id: Option<serenity::ChannelId>,
    ) -> serenity::ChannelId {
        if let Some(parent) = parent_id {
            FEATURE_PARENTS.insert(channel_id, parent);
        }
        let mut settings = self.settings.write().await;
        let key = channel_id.get().to_string();
        if settings.channel_fast_modes.contains_key(&key)
            || settings.channel_codex_goals.contains_key(&key)
        {
            if let Some(parent) = parent_id.filter(|parent| *parent != channel_id) {
                self.inherit_feature_reset(provider, channel_id, parent, &mut settings);
            }
            channel_id
        } else {
            parent_id.unwrap_or(channel_id)
        }
    }

    fn inherit_feature_reset(
        self: &Arc<Self>,
        provider: &ProviderKind,
        channel: serenity::ChannelId,
        parent: serenity::ChannelId,
        settings: &mut super::super::super::DiscordBotSettings,
    ) {
        let key = channel.get().to_string();
        let parent_key = parent.get().to_string();
        let mut moved = false;
        // A legacy thread override can still inherit the other feature from its parent.
        // Keep that feature's pending reset on the same key the turn will consume.
        if native_fast_mode_supported(provider)
            && !settings.channel_fast_modes.contains_key(&key)
            && fast_mode_reset_pending_for_provider(self, parent, provider)
        {
            clear_fast_mode_reset_pending_for_provider(self, parent, provider);
            let reset_key = fast_mode_reset_pending_key(channel, provider);
            self.overrides
                .fast_mode_session_reset_pending
                .insert(reset_key.clone());
            settings
                .channel_fast_mode_reset_pending
                .retain(|entry| !fast_mode_reset_entry_matches_provider(entry, parent, provider));
            settings.channel_fast_mode_reset_pending.insert(reset_key);
            moved = true;
        }
        if matches!(provider, ProviderKind::Codex)
            && !settings.channel_codex_goals.contains_key(&key)
            && self
                .overrides
                .codex_goals_session_reset_pending
                .remove(&parent)
                .is_some()
        {
            settings
                .channel_codex_goals_reset_pending
                .remove(&parent_key);
            settings.channel_codex_goals_reset_pending.insert(key);
            self.overrides
                .codex_goals_session_reset_pending
                .insert(channel);
            moved = true;
        }
        if moved {
            sync_session_reset_pending(self, parent);
            sync_session_reset_pending(self, channel);
        }
    }

    pub(in crate::services::discord) async fn feature_command_channel_id(
        self: &Arc<Self>,
        http: &Arc<serenity::Http>,
        provider: &ProviderKind,
        channel_id: serenity::ChannelId,
    ) -> serenity::ChannelId {
        let parent = super::super::super::resolve_thread_parent(http, channel_id)
            .await
            .map(|(parent, _)| parent);
        self.feature_settings_channel_id(provider, channel_id, parent)
            .await
    }

    pub(in crate::services::discord) async fn check_settings_owner(
        &self,
        provider: &ProviderKind,
        channel_id: serenity::ChannelId,
    ) -> io::Result<()> {
        let local =
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
        if let Some(endpoint) = crate::config::session_hosts::herdr_endpoint(channel_id.get()) {
            reject_foreign_owner(&local, &[endpoint.execution_node])?;
        }
        if let Some(pool) = self.pg_pool.as_ref() {
            use crate::services::cluster::intake_router_hook::{
                SessionOwnerResolution, resolve_session_owner,
            };
            let owner = resolve_session_owner(
                pool,
                provider.as_str(),
                &channel_id.get().to_string(),
                &local,
                crate::config::runtime_config()
                    .cluster
                    .lease_ttl_secs
                    .max(1),
                false,
            )
            .await
            .map_err(io::Error::other)?;
            match owner {
                SessionOwnerResolution::NoOwner | SessionOwnerResolution::LiveLocal { .. } => {}
                SessionOwnerResolution::LiveForeign { instance_id, .. }
                | SessionOwnerResolution::LiveForeignIncompatible { instance_id, .. } => {
                    reject_foreign_owner(&local, &[instance_id])?;
                }
                SessionOwnerResolution::ConflictingLiveOwners { instance_ids } => {
                    return Err(io::Error::other(format!(
                        "여러 노드(`{}`)의 활성 소유 기록이 충돌하여 설정 변경을 거절합니다.",
                        instance_ids.join("`, `")
                    )));
                }
                SessionOwnerResolution::StaleOwners { instance_ids } => {
                    return Err(io::Error::other(format!(
                        "정리되지 않은 소유 기록(노드 `{}`)이 남아 있어 설정 변경을 거절합니다.",
                        instance_ids.join("`, `")
                    )));
                }
            }
            let owners: Vec<String> = sqlx::query_scalar(
                "SELECT owner_instance_id FROM intake_session_owners
                 WHERE raw_channel_id = $1 AND provider = $2 AND status = 'active'",
            )
            .bind(channel_id.get().to_string())
            .bind(provider.as_str())
            .fetch_all(pool)
            .await
            .map_err(io::Error::other)?;
            reject_foreign_owner(&local, &owners)?;
        }
        Ok(())
    }
}

fn reject_foreign_owner(local: &str, owners: &[String]) -> io::Result<()> {
    if let Some(owner) = owners.iter().find(|owner| owner.trim() != local) {
        return Err(io::Error::other(format!(
            "노드 `{owner}` 소유 채널입니다. 소유 노드에서 설정을 변경하세요."
        )));
    }
    Ok(())
}

/// Whether a Claude launch setting reaches this channel by restarting its local tmux TUI on the
/// same session; a Herdr channel and the non-TUI drivers keep their own paths.
pub(in crate::services::discord) fn claude_local_tui_relaunch_applies(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    use crate::services::provider_hosting::{
        ProviderSessionDriver, resolve_provider_session_selection_with_channel,
    };
    matches!(provider, ProviderKind::Claude)
        && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_none()
        && resolve_provider_session_selection_with_channel(provider, true, Some(channel_id.get()))
            .driver
            == ProviderSessionDriver::TuiHosting
}

fn claude_herdr_fast_unavailable(provider: &ProviderKind, channel_id: serenity::ChannelId) -> bool {
    matches!(provider, ProviderKind::Claude)
        && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some()
}

pub(in crate::services::discord) async fn update_channel_model_override(
    shared: &Arc<SharedData>,
    token: &str,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    next_override: Option<String>,
) -> io::Result<bool> {
    shared.check_settings_owner(provider, channel_id).await?;
    let mut settings = shared.settings.write().await;
    let current = shared
        .overrides
        .model_overrides
        .get(&channel_id)
        .map(|v| v.clone());
    if same_model_override(current.as_deref(), next_override.as_deref()) {
        return Ok(false);
    }
    // A local Claude TUI takes the new model by restarting on its session, keeping its context.
    let relaunch = claude_local_tui_relaunch_applies(provider, channel_id);
    let reset = !relaunch
        && session_reset_required_for_model_change(
            provider,
            current.as_deref(),
            next_override.as_deref(),
        );
    let mut candidate = settings.clone();
    let key = channel_id.get().to_string();
    if relaunch {
        candidate.channel_relaunch_pending.insert(key.clone());
    }
    match &next_override {
        Some(model) => {
            candidate.channel_model_overrides.insert(key, model.clone());
        }
        None => {
            candidate.channel_model_overrides.remove(&key);
        }
    }
    candidate.try_save_bot_settings(token)?;
    super::super::control::forget_relaunch_failures(channel_id);
    *settings = candidate;
    match next_override {
        Some(model) => {
            shared.overrides.model_overrides.insert(channel_id, model);
        }
        None => {
            shared.overrides.model_overrides.remove(&channel_id);
        }
    }
    if reset {
        shared
            .overrides
            .model_session_reset_pending
            .insert(channel_id);
    } else {
        shared
            .overrides
            .model_session_reset_pending
            .remove(&channel_id);
    }
    sync_session_reset_pending(shared, channel_id);
    Ok(true)
}

/// The effort levels a Claude TUI launch accepts.
pub(in crate::services::discord) const EFFORT_LEVELS: [&str; 5] =
    ["low", "medium", "high", "xhigh", "max"];

/// The saved effort level a local Claude TUI on the channel launches with, if it is one the CLI takes.
pub(in crate::services::discord) async fn channel_effort_level(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> Option<String> {
    if !matches!(provider, ProviderKind::Claude) {
        return None;
    }
    let settings = shared.settings.read().await;
    let level = settings
        .channel_effort_levels
        .get(&channel_id.get().to_string())?;
    EFFORT_LEVELS
        .contains(&level.as_str())
        .then(|| level.clone())
}

/// Saves the effort level a local Claude TUI launches with and asks for its same-session relaunch;
/// a Herdr channel and the non-TUI drivers refuse it with the setting left as it was.
pub(in crate::services::discord) async fn update_channel_effort(
    shared: &Arc<SharedData>,
    token: &str,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    level: &str,
) -> io::Result<bool> {
    shared.check_settings_owner(provider, channel_id).await?;
    if claude_herdr_fast_unavailable(provider, channel_id) {
        return Err(io::Error::other(
            "Herdr 채널의 Claude TUI에는 /effort 설정이 적용되지 않습니다. 설정과 현재 세션을 유지했습니다.",
        ));
    }
    if !claude_local_tui_relaunch_applies(provider, channel_id) {
        return Err(io::Error::other(
            "/effort는 로컬 Claude TUI 채널에서만 저장할 수 있습니다.",
        ));
    }
    let mut settings = shared.settings.write().await;
    let key = channel_id.get().to_string();
    if settings.channel_effort_levels.get(&key).map(String::as_str) == Some(level) {
        return Ok(false);
    }
    let mut candidate = settings.clone();
    candidate
        .channel_effort_levels
        .insert(key.clone(), level.to_string());
    candidate.channel_relaunch_pending.insert(key);
    candidate.try_save_bot_settings(token)?;
    super::super::control::forget_relaunch_failures(channel_id);
    *settings = candidate;
    Ok(true)
}

pub(in crate::services::discord) async fn update_channel_fast_mode(
    shared: &Arc<SharedData>,
    token: &str,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    enabled: bool,
    runtime_channel_id: serenity::ChannelId,
) -> io::Result<bool> {
    shared
        .check_settings_owner(provider, runtime_channel_id)
        .await?;
    if runtime_channel_id != channel_id {
        shared.check_settings_owner(provider, channel_id).await?;
    }
    if claude_herdr_fast_unavailable(provider, runtime_channel_id) {
        return Err(io::Error::other(
            "Herdr 채널의 Claude TUI에는 /fast 설정이 적용되지 않습니다. 설정과 현재 세션을 유지했습니다.",
        ));
    }
    let mut settings = shared.settings.write().await;
    if channel_fast_mode_enabled(shared, channel_id) == enabled
        && settings
            .channel_fast_modes
            .get(&channel_id.get().to_string())
            .copied()
            == Some(enabled)
    {
        return Ok(false);
    }
    let mut candidate = settings.clone();
    candidate
        .channel_fast_modes
        .insert(channel_id.get().to_string(), enabled);
    candidate
        .channel_fast_mode_reset_pending
        .retain(|entry| !fast_mode_reset_entry_matches_provider(entry, channel_id, provider));
    candidate
        .channel_fast_mode_reset_pending
        .insert(fast_mode_reset_pending_key(channel_id, provider));
    candidate.try_save_bot_settings(token)?;
    super::super::control::forget_relaunch_failures(channel_id);
    *settings = candidate;
    if enabled {
        shared.overrides.fast_mode_channels.insert(channel_id);
    } else {
        shared.overrides.fast_mode_channels.remove(&channel_id);
    }
    clear_fast_mode_reset_pending_for_provider(shared, channel_id, provider);
    shared
        .overrides
        .fast_mode_session_reset_pending
        .insert(fast_mode_reset_pending_key(channel_id, provider));
    sync_session_reset_pending(shared, channel_id);
    Ok(true)
}

pub(in crate::services::discord) async fn update_channel_codex_goals(
    shared: &Arc<SharedData>,
    token: &str,
    channel_id: serenity::ChannelId,
    enabled: bool,
) -> io::Result<bool> {
    shared
        .check_settings_owner(&ProviderKind::Codex, channel_id)
        .await?;
    let mut settings = shared.settings.write().await;
    if channel_codex_goals_enabled(shared, channel_id) == enabled
        && settings
            .channel_codex_goals
            .get(&channel_id.get().to_string())
            .copied()
            == Some(enabled)
    {
        return Ok(false);
    }
    let mut candidate = settings.clone();
    let key = channel_id.get().to_string();
    candidate.channel_codex_goals.insert(key.clone(), enabled);
    candidate.channel_codex_goals_reset_pending.insert(key);
    candidate.try_save_bot_settings(token)?;
    *settings = candidate;
    if enabled {
        shared.overrides.codex_goals_channels.insert(channel_id);
    } else {
        shared.overrides.codex_goals_channels.remove(&channel_id);
    }
    shared
        .overrides
        .codex_goals_session_reset_pending
        .insert(channel_id);
    sync_session_reset_pending(shared, channel_id);
    Ok(true)
}

#[cfg(test)]
mod truthful_tests {
    use super::super::session_override_cluster_characterization_tests::{
        isolate_persistence, test_rt,
    };
    use super::*;
    use crate::services::discord::make_shared_data_for_tests;

    #[test]
    fn truthful_failed_fast_and_goals_save_never_publish_or_reset() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let root = tempfile::tempdir().unwrap();
        let _env = isolate_persistence(root.path());
        std::fs::create_dir_all(root.path().join("config/bot_settings.json")).unwrap();
        let shared = make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(648611);
        let rt = test_rt();
        assert!(
            rt.block_on(update_channel_fast_mode(
                &shared,
                "test-token",
                channel,
                &ProviderKind::Codex,
                true,
                channel
            ))
            .is_err()
        );
        assert!(
            rt.block_on(update_channel_codex_goals(
                &shared,
                "test-token",
                channel,
                true
            ))
            .is_err()
        );
        assert!(!channel_fast_mode_enabled(&shared, channel));
        assert!(!channel_codex_goals_enabled(&shared, channel));
        assert!(!shared.overrides.session_reset_pending.contains(&channel));
        rt.block_on(async {
            let settings = shared.settings.read().await;
            assert!(
                !settings
                    .channel_fast_modes
                    .contains_key(&channel.get().to_string())
            );
            assert!(
                !settings
                    .channel_codex_goals
                    .contains_key(&channel.get().to_string())
            );
        });
    }

    #[test]
    fn truthful_parent_thread_and_explicit_false_keys_remain_readable() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = make_shared_data_for_tests();
        test_rt().block_on(async {
            let parent = serenity::ChannelId::new(648620);
            let thread = serenity::ChannelId::new(648621);
            {
                let mut settings = shared.settings.write().await;
                settings
                    .channel_fast_modes
                    .insert(parent.get().to_string(), true);
                settings
                    .channel_codex_goals
                    .insert(parent.get().to_string(), true);
            }
            let key = shared
                .feature_settings_channel_id(&ProviderKind::Codex, thread, Some(parent))
                .await;
            assert_eq!(key, parent);
            assert_eq!(channel_fast_mode_setting(&shared, key).await, Some(true));
            shared
                .settings
                .write()
                .await
                .channel_fast_modes
                .insert(thread.get().to_string(), false);
            let key = shared
                .feature_settings_channel_id(&ProviderKind::Codex, thread, Some(parent))
                .await;
            assert_eq!(key, thread);
            assert_eq!(channel_fast_mode_setting(&shared, key).await, Some(false));
            assert_eq!(channel_codex_goals_setting(&shared, key).await, Some(true));
            shared
                .settings
                .write()
                .await
                .channel_codex_goals
                .insert(thread.get().to_string(), false);
            assert_eq!(channel_codex_goals_setting(&shared, key).await, Some(false));
            assert_eq!(
                shared
                    .feature_settings_channel_id(&ProviderKind::Codex, parent, None)
                    .await,
                parent
            );
        });
    }

    #[test]
    fn truthful_headless_claude_fast_persists_and_schedules_real_reset() {
        let _hosting = crate::services::provider_hosting::TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let root = tempfile::tempdir().unwrap();
        let _env = isolate_persistence(root.path());
        let mut config = crate::config::Config::default();
        config.providers.insert(
            "claude".into(),
            crate::config::ProviderConfig {
                tui_hosting: Some(false),
                ..Default::default()
            },
        );
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::services::provider_hosting::install_provider_hosting_config(
                    &crate::config::Config::default(),
                );
            }
        }
        let _restore = Restore;
        crate::services::provider_hosting::install_provider_hosting_config(&config);
        let shared = make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(648630);
        assert!(
            test_rt()
                .block_on(update_channel_fast_mode(
                    &shared,
                    "test-token",
                    channel,
                    &ProviderKind::Claude,
                    true,
                    channel
                ))
                .unwrap()
        );
        assert!(channel_fast_mode_enabled(&shared, channel));
        assert!(fast_mode_reset_pending_for_provider(
            &shared,
            channel,
            &ProviderKind::Claude
        ));
        assert!(shared.overrides.session_reset_pending.contains(&channel));
        assert!(session_toggle_reset_line(true).contains("맥락 소실"));
    }

    // A Claude channel on the local tmux TUI saves fast mode for a same-session relaunch instead
    // of refusing it; only a Herdr channel keeps the refusal.
    #[test]
    fn truthful_local_claude_tui_fast_saves_for_a_same_session_relaunch() {
        let _hosting = crate::services::provider_hosting::TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let root = tempfile::tempdir().unwrap();
        let _env = isolate_persistence(root.path());
        let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let mut config = crate::config::Config::default();
        config.providers.insert(
            "claude".into(),
            crate::config::ProviderConfig {
                tui_hosting: Some(true),
                ..Default::default()
            },
        );
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::services::provider_hosting::install_provider_hosting_config(
                    &crate::config::Config::default(),
                );
            }
        }
        let _restore = Restore;
        crate::services::provider_hosting::install_provider_hosting_config(&config);
        let shared = make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(648631);
        let saved = test_rt().block_on(update_channel_fast_mode(
            &shared,
            "test-token",
            channel,
            &ProviderKind::Claude,
            true,
            channel,
        ));
        assert!(saved.unwrap());
        assert!(channel_fast_mode_enabled(&shared, channel));
        assert!(fast_mode_reset_pending_for_provider(
            &shared,
            channel,
            &ProviderKind::Claude
        ));
        assert!(claude_local_tui_relaunch_applies(
            &ProviderKind::Claude,
            channel
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn truthful_foreign_database_owner_is_named_and_blocks_before_save_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _hosts = crate::config::session_hosts::force_for_test(Some("gateway"), &[]);
        let config_root = tempfile::tempdir().unwrap();
        let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &config_root.path().join("missing.yaml"),
        );
        let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
        super::revision_tests::seed_worker(&pool, "remote-owner", "online").await;
        sqlx::query("INSERT INTO sessions (session_key, provider, channel_id, instance_id, status, last_heartbeat) VALUES ('settings-owner', 'codex', '648640', 'remote-owner', 'idle', NOW())")
            .execute(&pool).await.unwrap();
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let channel = serenity::ChannelId::new(648640);
        let error = update_channel_model_override(
            &shared,
            "test-token",
            channel,
            &ProviderKind::Codex,
            Some("model-a".into()),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("노드 `remote-owner` 소유"),
            "{error}"
        );
        assert!(!shared.overrides.model_overrides.contains_key(&channel));
        assert!(!shared.overrides.session_reset_pending.contains(&channel));
        pool.close().await;
        db.drop().await;
    }
    #[test]
    fn truthful_inherited_feature_reset_follows_thread_key_once() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = make_shared_data_for_tests();
        test_rt().block_on(async {
            let direct_parent = serenity::ChannelId::new(648652);
            let direct_thread = serenity::ChannelId::new(648653);
            shared
                .overrides
                .codex_goals_session_reset_pending
                .insert(direct_parent);
            {
                let mut settings = shared.settings.write().await;
                settings
                    .channel_fast_modes
                    .insert(direct_thread.get().to_string(), false);
                settings
                    .channel_codex_goals_reset_pending
                    .insert(direct_parent.get().to_string());
                shared.inherit_feature_reset(
                    &ProviderKind::Codex,
                    direct_thread,
                    direct_parent,
                    &mut settings,
                );
            }
            assert!(
                shared
                    .overrides
                    .codex_goals_session_reset_pending
                    .contains(&direct_thread)
            );
            assert!(
                !shared
                    .overrides
                    .codex_goals_session_reset_pending
                    .contains(&direct_parent)
            );

            let parent = serenity::ChannelId::new(648650);
            let thread = serenity::ChannelId::new(648651);
            {
                let mut settings = shared.settings.write().await;
                settings
                    .channel_fast_modes
                    .insert(thread.get().to_string(), false);
                settings
                    .channel_codex_goals
                    .insert(parent.get().to_string(), true);
                settings
                    .channel_codex_goals_reset_pending
                    .insert(parent.get().to_string());
            }
            shared
                .overrides
                .codex_goals_session_reset_pending
                .insert(parent);
            let key = shared
                .feature_settings_channel_id(&ProviderKind::Codex, thread, Some(parent))
                .await;
            assert_eq!(key, thread);
            assert!(shared.overrides.session_reset_pending.contains(&thread));
            assert!(
                !shared
                    .overrides
                    .codex_goals_session_reset_pending
                    .contains(&parent)
            );
            assert!(
                shared
                    .overrides
                    .codex_goals_session_reset_pending
                    .contains(&thread)
            );
            assert_eq!(channel_codex_goals_setting(&shared, key).await, Some(true));
            assert!(clear_codex_goals_reset_pending_for_channel(&shared, thread));
            sync_session_reset_pending(&shared, thread);
            shared
                .settings
                .write()
                .await
                .channel_codex_goals_reset_pending
                .remove(&thread.get().to_string());
            shared
                .feature_settings_channel_id(&ProviderKind::Codex, thread, Some(parent))
                .await;
            assert!(
                !shared
                    .overrides
                    .codex_goals_session_reset_pending
                    .contains(&thread)
            );
            assert!(!shared.overrides.session_reset_pending.contains(&thread));
            let pending = shared.settings.read().await;
            assert!(
                !pending
                    .channel_codex_goals_reset_pending
                    .contains(&parent.get().to_string())
            );
        });
    }
    #[test]
    fn truthful_pending_reset_migration_preserves_feature_and_provider() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = make_shared_data_for_tests();
        test_rt().block_on(async {
            let parent = serenity::ChannelId::new(648660);
            let thread = serenity::ChannelId::new(648661);
            let provider = ProviderKind::Codex;
            let parent_reset = fast_mode_reset_pending_key(parent, &provider);
            shared
                .overrides
                .fast_mode_session_reset_pending
                .insert(parent_reset.clone());
            {
                let mut settings = shared.settings.write().await;
                settings
                    .channel_codex_goals
                    .insert(thread.get().to_string(), false);
                settings
                    .channel_fast_modes
                    .insert(parent.get().to_string(), true);
                settings
                    .channel_fast_mode_reset_pending
                    .insert(parent_reset.clone());
                shared.inherit_feature_reset(&provider, thread, parent, &mut settings);
            }
            assert!(fast_mode_reset_pending_for_provider(
                &shared, thread, &provider
            ));
            assert!(!fast_mode_reset_pending_for_provider(
                &shared, parent, &provider
            ));
            assert!(!fast_mode_reset_pending_for_provider(
                &shared,
                thread,
                &ProviderKind::Claude
            ));
            assert!(shared.overrides.session_reset_pending.contains(&thread));
            assert!(clear_fast_mode_reset_pending_for_provider(
                &shared, thread, &provider
            ));
            sync_session_reset_pending(&shared, thread);
            let mut settings = shared.settings.write().await;
            settings
                .channel_fast_mode_reset_pending
                .remove(&fast_mode_reset_pending_key(thread, &provider));
            shared.inherit_feature_reset(&provider, thread, parent, &mut settings);
            assert!(!fast_mode_reset_pending_for_provider(
                &shared, thread, &provider
            ));
            assert!(
                !settings
                    .channel_fast_mode_reset_pending
                    .contains(&parent_reset)
            );
        });
    }
}

#[cfg(test)]
mod revision_tests;

#[cfg(test)]
pub(super) fn local_tui_for_test(channel: u64) -> crate::config::session_hosts::ForcedSessionHosts {
    let local = crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    crate::config::session_hosts::force_for_test(Some(&local), &[(channel, &local)])
}

#[cfg(test)]
#[path = "provider_settings_snapshot_tests.rs"]
mod snapshot_tests;
