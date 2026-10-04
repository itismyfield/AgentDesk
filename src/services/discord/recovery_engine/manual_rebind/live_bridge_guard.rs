use super::*;

pub(crate) async fn rebind_inflight_for_channel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: Option<String>,
    overrides: ManualRebindOverrides,
    expected_episode: Option<&super::inflight::InflightEpisodePin>,
) -> Result<RebindOutcome, RebindError> {
    let recovery = super::super::live_bridge::try_recovery(provider, channel_id)
        .map_err(|()| RebindError::InflightAlreadyExists)?;
    recovery
        .run(async {
            rebind_inflight_for_channel_inner(
                http,
                shared,
                provider,
                channel_id,
                tmux_session_override,
                overrides,
                None,
                expected_episode,
            )
            .await
        })
        .await
}

pub(crate) async fn rebind_inflight_for_channel_with_minimum_start_offset(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: Option<String>,
    minimum_initial_offset: Option<u64>,
    expected_episode: Option<&super::inflight::InflightEpisodePin>,
) -> Result<RebindOutcome, RebindError> {
    let recovery = super::super::live_bridge::try_recovery(provider, channel_id)
        .map_err(|()| RebindError::InflightAlreadyExists)?;
    recovery
        .run(async {
            rebind_inflight_for_channel_inner(
                http,
                shared,
                provider,
                channel_id,
                tmux_session_override,
                ManualRebindOverrides::default(),
                minimum_initial_offset,
                expected_episode,
            )
            .await
        })
        .await
}
