use std::collections::HashSet;

use super::super::{inflight, tui_direct_pending_start};
use super::HealthRegistry;
use crate::services::provider::ProviderKind;

fn leaked_row_sweep_candidate(state: &inflight::InflightTurnState) -> Option<(&str, u64)> {
    let tmux_session = state
        .tmux_session_name
        .as_deref()
        .map(str::trim)
        .filter(|session| !session.is_empty())?;
    (state.channel_id != 0).then_some((tmux_session, state.channel_id))
}

async fn recover_candidate(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> bool {
    let Some((tmux_session, channel_id)) = leaked_row_sweep_candidate(state) else {
        return false;
    };
    let channel = poise::serenity_prelude::ChannelId::new(channel_id);
    let Some(shared) = registry
        .shared_for_provider_on_channel(provider, channel)
        .await
    else {
        return false;
    };

    let record = tui_direct_pending_start::TuiDirectPendingStart {
        provider: provider.as_str().to_string(),
        channel_id,
        tmux_session_name: tmux_session.to_string(),
        prompt_text: String::new(),
        // A sweep has no synthetic anchor. MAX cannot equal a real Discord id,
        // so the existing foreign-row gate evaluates the durable row itself.
        anchor_message_id: u64::MAX,
        lease_relay_owner: String::new(),
        lease_runtime_kind: None,
        lease_turn_id: None,
        lease_session_key: None,
        generation: shared.restart.current_generation,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: tui_direct_pending_start::PendingStartState::Waiting,
        attempt_count: 0,
    };
    tui_direct_pending_start::demote_stale_foreign_inflight_if_current(&shared, &record).await
}

pub(in crate::services::discord) async fn sweep_leaked_inflight_rows(
    registry: &HealthRegistry,
    provider: &ProviderKind,
) -> usize {
    let states = inflight::load_inflight_states_for_sweep(provider);
    let mut seen = HashSet::new();
    let mut applied = 0;
    for (state, _) in states {
        if !seen.insert(state.channel_id) {
            continue;
        }
        if recover_candidate(registry, provider, &state).await {
            applied += 1;
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::leaked_row_sweep_candidate;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;

    fn state(channel_id: u64, tmux_session: Option<&str>) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            None,
            1,
            2,
            3,
            "prompt".to_string(),
            None,
            tmux_session.map(str::to_string),
            Some("/tmp/output.jsonl".to_string()),
            None,
            0,
        )
    }

    #[test]
    fn discord_row_is_sweep_candidate_without_pending_start() {
        let state = state(4_042_001, Some("AgentDesk-claude-discord"));
        assert_eq!(
            leaked_row_sweep_candidate(&state),
            Some(("AgentDesk-claude-discord", 4_042_001))
        );
    }

    #[test]
    fn sweep_rejects_rows_without_channel_or_tmux_identity() {
        assert!(leaked_row_sweep_candidate(&state(0, Some("tmux"))).is_none());
        assert!(leaked_row_sweep_candidate(&state(4_042_002, None)).is_none());
        assert!(leaked_row_sweep_candidate(&state(4_042_003, Some("  "))).is_none());
    }
}
