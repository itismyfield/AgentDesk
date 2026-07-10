use serde::Serialize;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::discord::relay_health::{RelayHealthSnapshot, RelayStallState};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;

use super::HealthRegistry;
use super::recovery::ProviderMailboxState;
use super::stall_verdict::StallVerdict;

#[derive(Debug, Serialize)]
pub(super) struct MailboxHealthSnapshot {
    pub(super) provider: String,
    pub(super) channel_id: u64,
    pub(super) has_cancel_token: bool,
    pub(super) queue_depth: usize,
    pub(super) recovery_started: bool,
    pub(super) active_request_owner: Option<u64>,
    pub(super) active_user_message_id: Option<u64>,
    pub(super) agent_turn_status: &'static str,
    pub(super) watcher_attached: bool,
    pub(super) inflight_state_present: bool,
    pub(super) tmux_present: bool,
    pub(super) process_present: bool,
    pub(super) active_dispatch_present: bool,
    pub(super) stall_shadow_verdict: Option<StallVerdict>,
    pub(super) relay_stall_state: RelayStallState,
    pub(super) relay_health: RelayHealthSnapshot,
}

/// #3293 (c): probe a channel's mailbox state WITHOUT creating a registry
/// entry. `shared.mailbox()` (the pre-#3293 probe path) mints a permanent
/// mailbox actor for every channel id it is asked about, so health/repair
/// before+after probes against a non-existent channel id polluted the
/// registry forever. No entry simply reports the idle/empty state.
pub(super) async fn peeked_provider_mailbox_state(
    shared: &SharedData,
    channel_id: u64,
) -> ProviderMailboxState {
    let Some(handle) = shared.mailbox_peek(ChannelId::new(channel_id)) else {
        return ProviderMailboxState {
            channel_id,
            has_cancel_token: false,
            queue_depth: 0,
            recovery_started: false,
        };
    };
    let snapshot = handle.snapshot().await;
    ProviderMailboxState {
        channel_id,
        has_cancel_token: snapshot.cancel_token.is_some(),
        queue_depth: snapshot.intervention_queue.len(),
        recovery_started: snapshot.recovery_started_at.is_some(),
    }
}

/// Result of [`purge_idle_channel_mailbox_registry_entry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxRegistryPurgeResult {
    /// At least one runtime's registry entry was unlinked.
    pub removed: bool,
    /// Why nothing (or not everything) was removed; `None` on full success.
    pub skipped_reason: Option<&'static str>,
}

/// #3293 (c): operator-gated purge of a channel's idle mailbox registry
/// entry. Called by the stale-mailbox repair endpoint AFTER its full gate
/// chain (CAS `expected_has_cancel_token` + `no_live_work_evidence`) passed
/// and the repair reported `applied`. Visits every registered runtime for
/// the provider filter (or all runtimes when unfiltered) because the bogus
/// entry may live in any instance registry; each removal re-verifies actor
/// idleness right before the unlink. In-memory only — no disk/DB mutation.
pub async fn purge_idle_channel_mailbox_registry_entry(
    registry: &HealthRegistry,
    provider_name: Option<&str>,
    channel_id: u64,
) -> MailboxRegistryPurgeResult {
    let runtimes = match provider_name {
        Some(name) => match ProviderKind::from_str(name) {
            Some(provider) => registry.all_shared_for_provider(&provider).await,
            None => Vec::new(),
        },
        None => registry.all_registered_shared().await,
    };
    if runtimes.is_empty() {
        return MailboxRegistryPurgeResult {
            removed: false,
            skipped_reason: Some("no_registered_runtime"),
        };
    }
    let channel = ChannelId::new(channel_id);
    let mut removed = false;
    let mut refused: Option<&'static str> = None;
    for shared in runtimes {
        match shared.mailboxes.remove_idle_entry(channel).await {
            MailboxPurgeOutcome::Removed => removed = true,
            MailboxPurgeOutcome::NoEntry => {}
            MailboxPurgeOutcome::RefusedLiveWork(reason) => refused = Some(reason),
        }
    }
    let skipped_reason = refused.or(if removed {
        None
    } else {
        Some("no_registry_entry")
    });
    MailboxRegistryPurgeResult {
        removed,
        skipped_reason,
    }
}
