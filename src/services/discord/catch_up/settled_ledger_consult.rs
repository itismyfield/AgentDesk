//! #4564 catch-up consult of the durable completed-turn ledger.
//!
//! Built once per channel scan (mirroring the `existing_ids` construction in
//! `catch_up.rs`) and passed to `classify_catch_up_message`. A message whose id
//! is in this set has a CONFIRMED terminal delivery on record, so it must not be
//! re-flagged `TooOld` after a restart. The ledger is the ONLY settled-evidence
//! source (never a checkpoint/frontier cursor — that promotion is what closed
//! #4600 P1). An absent/malformed ledger yields an empty set, so a real message
//! is never wrongly suppressed.

use std::collections::HashSet;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::outbound::completed_turn_ledger;
use crate::services::provider::ProviderKind;

/// The set of inbound `user_msg_id`s with a durable completed-turn record for
/// `(provider, channel)`. Empty when the ledger is absent/malformed.
pub(in crate::services::discord) fn settled_ids(
    provider: &ProviderKind,
    channel: ChannelId,
) -> HashSet<u64> {
    completed_turn_ledger::settled_user_msg_ids(provider, channel.get())
}
