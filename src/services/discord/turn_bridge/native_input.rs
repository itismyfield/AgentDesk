//! Native-owned inputs never borrow the bridge's unscoped body or legacy completion.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum State {
    Legacy,
    #[cfg(unix)]
    Pending,
    Settled,
}

pub(super) fn enabled() -> bool {
    #[cfg(unix)]
    return super::super::turn_finalizer::native_prepare::enabled();
    #[cfg(not(unix))]
    false
}

pub(in crate::services::discord) async fn state(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    token: &CancelToken,
) -> State {
    #[cfg(unix)]
    if *provider == ProviderKind::Claude {
        let key = super::super::turn_finalizer::TurnKey::new(
            ChannelId::new(row.channel_id),
            row.effective_finalizer_turn_id(),
            row.born_generation,
        )
        .with_episode_nonce(row.turn_nonce.as_deref());
        match shared.turn_finalizer.native_scope(key, token).await {
            Ok(true) => {
                return if token.is_completion_cleanup()
                    || shared
                        .turn_finalizer
                        .native_settled(key, token)
                        .await
                        .unwrap_or(false)
                {
                    State::Settled
                } else {
                    State::Pending
                };
            }
            Err(_) => return State::Pending,
            Ok(false) => {}
        }
    }
    #[cfg(not(unix))]
    let _ = (shared, provider, row, token);
    State::Legacy
}

pub(in crate::services::discord) async fn reject_legacy_body(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    token: &CancelToken,
    body: &mut String,
) -> bool {
    if state(shared, provider, row, token).await == State::Legacy {
        return false;
    }
    body.clear();
    true
}

pub(in crate::services::discord) async fn neutral_exit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    token: &CancelToken,
) -> bool {
    state(shared, provider, row, token).await != State::Legacy
}
