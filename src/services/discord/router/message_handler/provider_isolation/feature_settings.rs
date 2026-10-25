use super::*;

pub(in crate::services::discord::router::message_handler) fn native_fast_mode_override_for_turn(
    provider: &ProviderKind,
    channel_fast_mode_setting: Option<bool>,
) -> Option<bool> {
    if matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        channel_fast_mode_setting
    } else {
        None
    }
}

pub(in crate::services::discord::router::message_handler) fn codex_goals_override_for_turn(
    provider: &ProviderKind,
    channel_codex_goals_setting: Option<bool>,
) -> Option<bool> {
    if matches!(provider, ProviderKind::Codex) {
        channel_codex_goals_setting
    } else {
        None
    }
}
pub(in crate::services::discord::router::message_handler) async fn feature_key(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    thread_parent: Option<&(ChannelId, Option<String>)>,
) -> ChannelId {
    shared
        .feature_settings_channel_id(
            provider,
            channel_id,
            thread_parent.map(|(parent, _)| *parent),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truthful_legacy_thread_setting_reaches_turn() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let thread = ChannelId::new(64861);
            let parent = ChannelId::new(64860);
            shared
                .settings
                .write()
                .await
                .channel_fast_modes
                .insert(thread.get().to_string(), false);
            let key =
                feature_key(&shared, &ProviderKind::Codex, thread, Some(&(parent, None))).await;
            let setting =
                crate::services::discord::commands::channel_fast_mode_setting(&shared, key).await;
            assert_eq!(setting, Some(false));
        });
    }
}
