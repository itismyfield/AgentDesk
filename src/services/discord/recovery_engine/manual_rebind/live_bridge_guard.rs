use super::*;
use crate::services::discord::input_runtime::fence;

/// A closed input gate refuses the rebind before its first effect; an admitted one spans it.
fn admit_input(
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<Option<fence::Permit>, RebindError> {
    fence::effect::admit(provider, channel_id).map_err(|failure| {
        fence::record_failure(provider, channel_id, &[], failure);
        RebindError::InputFenced(format!("{failure:?}"))
    })
}

pub(crate) async fn rebind_inflight_for_channel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: Option<String>,
    overrides: ManualRebindOverrides,
    expected_episode: Option<&super::inflight::InflightEpisodePin>,
) -> Result<RebindOutcome, RebindError> {
    let permit = admit_input(provider, channel_id)?;
    let recovery = super::super::live_bridge::try_recovery(provider, channel_id)
        .map_err(|()| RebindError::InflightAlreadyExists)?;
    let (http, shared, provider) = (http.clone(), shared.clone(), provider.clone());
    let expected_episode = expected_episode.cloned();
    // An admitted rebind runs on the input worker, where its row writers are allowed.
    fence::effect::run(permit, async move {
        let rebind = rebind_inflight_for_channel_inner(
            &http,
            &shared,
            &provider,
            channel_id,
            tmux_session_override,
            overrides,
            None,
            expected_episode.as_ref(),
        );
        recovery.run(rebind).await
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
    let permit = admit_input(provider, channel_id)?;
    let recovery = super::super::live_bridge::try_recovery(provider, channel_id)
        .map_err(|()| RebindError::InflightAlreadyExists)?;
    let (http, shared, provider) = (http.clone(), shared.clone(), provider.clone());
    let expected_episode = expected_episode.cloned();
    fence::effect::run(permit, async move {
        let rebind = rebind_inflight_for_channel_inner(
            &http,
            &shared,
            &provider,
            channel_id,
            tmux_session_override,
            ManualRebindOverrides::default(),
            minimum_initial_offset,
            expected_episode.as_ref(),
        );
        recovery.run(rebind).await
    })
    .await
}

#[cfg(test)]
#[path = "live_bridge_guard_tests.rs"]
mod tests;
