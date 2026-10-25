use super::*;

pub(super) async fn overrides_for_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> (Option<bool>, Option<bool>) {
    let fast =
        super::super::super::super::commands::channel_fast_mode_setting(shared, channel_id).await;
    let goals =
        super::super::super::super::commands::channel_codex_goals_setting(shared, channel_id).await;
    (
        native_fast_mode_override_for_turn(provider, fast),
        codex_goals_override_for_turn(provider, goals),
    )
}
