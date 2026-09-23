use chrono::{DateTime, Utc};
use poise::serenity_prelude::ChannelId;

use super::snapshot::RelayThreadProofSnapshot;
use crate::services::discord::relay_health::{
    RelayActiveTurn, RelayHealthSnapshot, observation_age_secs,
};
use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

pub(super) struct RelayHealthBuildInput {
    pub(super) provider: String,
    pub(super) channel_id: u64,
    pub(super) mailbox_has_cancel_token: bool,
    pub(super) mailbox_active_user_msg_id: Option<u64>,
    pub(super) mailbox_turn_started_at_ms: Option<i64>,
    pub(super) unpaired_active_token_reconfirmed: bool,
    pub(super) queue_depth: usize,
    pub(super) watcher_attached: bool,
    pub(super) watcher_attached_stale: bool,
    pub(super) watcher_owner_channel_id: Option<u64>,
    pub(super) tmux_session: Option<String>,
    pub(super) tmux_alive: Option<bool>,
    pub(super) bridge_inflight_present: bool,
    pub(super) bridge_current_msg_id: Option<u64>,
    pub(super) watcher_owns_live_relay: bool,
    pub(super) last_relay_ts_ms: i64,
    pub(super) last_relay_offset: u64,
    pub(super) last_capture_offset: Option<u64>,
    pub(super) unread_bytes: Option<u64>,
    pub(super) desynced: bool,
    pub(super) thread_proof: RelayThreadProofSnapshot,
    pub(super) active_turn: RelayActiveTurn,
    pub(super) last_outbound_activity_ms: Option<i64>,
}

pub(super) fn build_relay_health_snapshot(input: RelayHealthBuildInput) -> RelayHealthSnapshot {
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    let last_relay_ts_ms = (input.last_relay_ts_ms > 0).then_some(input.last_relay_ts_ms);
    RelayHealthSnapshot {
        provider: input.provider,
        channel_id: input.channel_id,
        active_turn: input.active_turn,
        tmux_session: input.tmux_session,
        tmux_alive: input.tmux_alive,
        watcher_attached: input.watcher_attached,
        watcher_attached_stale: input.watcher_attached_stale,
        watcher_owner_channel_id: input.watcher_owner_channel_id,
        watcher_owns_live_relay: input.watcher_owns_live_relay,
        bridge_inflight_present: input.bridge_inflight_present,
        bridge_current_msg_id: input.bridge_current_msg_id,
        mailbox_has_cancel_token: input.mailbox_has_cancel_token,
        mailbox_active_user_msg_id: input.mailbox_active_user_msg_id,
        mailbox_turn_started_at_ms: input.mailbox_turn_started_at_ms,
        mailbox_turn_age_secs: observation_age_secs(
            observed_at_ms,
            input.mailbox_turn_started_at_ms,
        ),
        queue_depth: input.queue_depth,
        pending_discord_callback_msg_id: input
            .bridge_current_msg_id
            .or(input.mailbox_active_user_msg_id),
        pending_thread_proof: input.thread_proof.parent_channel_id.is_some()
            || input.thread_proof.thread_channel_id.is_some(),
        parent_channel_id: input.thread_proof.parent_channel_id,
        thread_channel_id: input.thread_proof.thread_channel_id,
        last_relay_ts_ms,
        last_relay_age_secs: observation_age_secs(observed_at_ms, last_relay_ts_ms),
        last_outbound_activity_ms: input.last_outbound_activity_ms,
        last_capture_offset: input.last_capture_offset,
        last_relay_offset: input.last_relay_offset,
        unread_bytes: input.unread_bytes,
        desynced: input.desynced,
        stale_thread_proof: input.thread_proof.stale_thread_proof,
        // Readers take a reconfirmation to mean token-without-row; hold that
        // here rather than trust every caller to pass matching operands.
        unpaired_active_token_reconfirmed: input.unpaired_active_token_reconfirmed
            && input.mailbox_has_cancel_token
            && !input.bridge_inflight_present,
    }
}

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
        // #5736: read-only — a reconfirmation must not persist the row it reads.
        discord::inflight::load_inflight_state_read_only(provider, channel.get()).is_some();
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

    fn build_input(
        has_token: bool,
        inflight_present: bool,
        reconfirmed: bool,
    ) -> RelayHealthBuildInput {
        RelayHealthBuildInput {
            provider: "claude".to_string(),
            channel_id: 42,
            mailbox_has_cancel_token: has_token,
            mailbox_active_user_msg_id: Some(7),
            mailbox_turn_started_at_ms: Some(1_000_000),
            unpaired_active_token_reconfirmed: reconfirmed,
            queue_depth: 0,
            watcher_attached: true,
            watcher_attached_stale: false,
            watcher_owner_channel_id: None,
            tmux_session: None,
            tmux_alive: Some(true),
            bridge_inflight_present: inflight_present,
            bridge_current_msg_id: None,
            watcher_owns_live_relay: false,
            last_relay_ts_ms: 0,
            last_relay_offset: 0,
            last_capture_offset: None,
            unread_bytes: None,
            desynced: false,
            thread_proof: RelayThreadProofSnapshot::default(),
            active_turn: RelayActiveTurn::Foreground,
            last_outbound_activity_ms: None,
        }
    }

    #[test]
    fn a_built_snapshot_is_reconfirmed_only_for_a_token_without_a_row() {
        for has_token in [false, true] {
            for inflight_present in [false, true] {
                let snapshot =
                    build_relay_health_snapshot(build_input(has_token, inflight_present, true));
                assert_eq!(
                    snapshot.unpaired_active_token_reconfirmed,
                    has_token && !inflight_present,
                    "token={has_token} inflight={inflight_present}"
                );
                assert!(
                    !build_relay_health_snapshot(build_input(has_token, inflight_present, false))
                        .unpaired_active_token_reconfirmed
                );
            }
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
        assert!(!recheck_confirms_same_unpaired_turn(
            active_identity(),
            true,
            false,
            active_identity(),
            true,
            true,
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
