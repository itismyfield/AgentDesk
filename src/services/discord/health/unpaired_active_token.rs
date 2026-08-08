use chrono::{DateTime, Utc};
use poise::serenity_prelude::ChannelId;

use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveTurnIdentity {
    nonce: Option<String>,
    message_id: Option<u64>,
    started_at: Option<DateTime<Utc>>,
}

impl From<&ChannelMailboxSnapshot> for ActiveTurnIdentity {
    fn from(snapshot: &ChannelMailboxSnapshot) -> Self {
        Self {
            nonce: snapshot.active_turn_nonce.clone(),
            message_id: snapshot.active_user_message_id.map(|id| id.get()),
            started_at: snapshot.turn_started_at,
        }
    }
}

fn recheck_confirms_same_unpaired_turn(
    initial_identity: ActiveTurnIdentity,
    initial_has_token: bool,
    initial_inflight_present: bool,
    rechecked_identity: ActiveTurnIdentity,
    rechecked_has_token: bool,
    rechecked_inflight_present: bool,
) -> bool {
    initial_has_token
        && !initial_inflight_present
        && rechecked_has_token
        && !rechecked_inflight_present
        && initial_identity == rechecked_identity
}

/// Re-observe both authorities before allowing the stall classifier to use a
/// token-without-row candidate. The mailbox is sampled first and disk second;
/// a completion or episode replacement visible in either sample invalidates
/// the candidate.
pub(super) async fn reconfirm(
    shared: &SharedData,
    provider: Option<&ProviderKind>,
    channel: ChannelId,
    initial: &ChannelMailboxSnapshot,
    initial_inflight_present: bool,
) -> bool {
    if initial.cancel_token.is_none() || initial_inflight_present {
        return false;
    }
    let Some(provider) = provider else {
        return false;
    };

    let rechecked = discord::mailbox_snapshot(shared, channel).await;
    let rechecked_inflight_present =
        discord::inflight::load_inflight_state(provider, channel.get()).is_some();
    recheck_confirms_same_unpaired_turn(
        ActiveTurnIdentity::from(initial),
        initial.cancel_token.is_some(),
        initial_inflight_present,
        ActiveTurnIdentity::from(&rechecked),
        rechecked.cancel_token.is_some(),
        rechecked_inflight_present,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_identity() -> ActiveTurnIdentity {
        ActiveTurnIdentity {
            nonce: Some("turn-a".to_string()),
            message_id: Some(42),
            started_at: Some(DateTime::from_timestamp_millis(1_000_000).unwrap()),
        }
    }

    #[test]
    fn completion_between_initial_reads_and_recheck_invalidates_candidate() {
        let initial = active_identity();
        let completed = ActiveTurnIdentity {
            nonce: None,
            message_id: None,
            started_at: None,
        };

        assert!(!recheck_confirms_same_unpaired_turn(
            initial, true, false, completed, false, false,
        ));
    }

    #[test]
    fn every_identity_coordinate_must_remain_stable() {
        let initial = active_identity();
        for changed in [
            ActiveTurnIdentity {
                nonce: Some("turn-b".to_string()),
                ..initial.clone()
            },
            ActiveTurnIdentity {
                message_id: Some(43),
                ..initial.clone()
            },
            ActiveTurnIdentity {
                started_at: DateTime::from_timestamp_millis(1_001_000),
                ..initial.clone()
            },
        ] {
            assert!(!recheck_confirms_same_unpaired_turn(
                initial.clone(),
                true,
                false,
                changed,
                true,
                false,
            ));
        }
    }
}
