//! #4564 catch-up consult of the durable completed-turn ledger.
//!
//! Built once per channel scan (mirroring the `existing_ids` construction in
//! `catch_up.rs`) and passed to `classify_catch_up_message`. A message whose id
//! is in this set has a CONFIRMED terminal delivery on record, so it must not be
//! re-flagged `TooOld` after a restart. The ledger is the ONLY settled-evidence
//! source (never a checkpoint/frontier cursor — that promotion is what closed
//! #4600 P1). An absent/malformed ledger yields an empty set, so a real message
//! is never wrongly suppressed.

use std::collections::{HashMap, HashSet};

use poise::serenity_prelude::ChannelId;

use crate::services::discord::ChannelMailboxSnapshot;
use crate::services::discord::outbound::completed_turn_ledger::{self, CompletedTurnLedger};
use crate::services::discord::recovery_known_ids::RecoveryKnownIdArm;
use crate::services::provider::ProviderKind;

/// One channel's completed-turn ledger, read once per scan. Absent or
/// malformed reads as `None`, which settles nothing.
pub(in crate::services::discord) struct ScanLedger(Option<CompletedTurnLedger>);

pub(in crate::services::discord) fn read(
    provider: &ProviderKind,
    channel: ChannelId,
) -> ScanLedger {
    ScanLedger(completed_turn_ledger::read_ledger(provider, channel.get()))
}

impl ScanLedger {
    /// Settled ids from this one read. After `snapshot` is read, the active episode's durable
    /// absorbed ids also join `known`/`arms` as `AbsorbedActiveTurn` (a restored actor has none).
    pub(in crate::services::discord) fn settle(
        &self,
        snapshot: &ChannelMailboxSnapshot,
        arms: &mut HashMap<u64, RecoveryKnownIdArm>,
        known: &mut HashSet<u64>,
    ) -> HashSet<u64> {
        let Some(ledger) = self.0.as_ref() else {
            return HashSet::new();
        };
        if let (Some(primary), Some(turn_nonce)) = (
            snapshot.active_user_message_id,
            snapshot.active_turn_nonce.as_deref(),
        ) {
            for absorbed in ledger.absorbed_by_episode(primary.get(), turn_nonce) {
                let arm = arms
                    .entry(absorbed)
                    .or_insert(RecoveryKnownIdArm::AbsorbedActiveTurn);
                if *arm != RecoveryKnownIdArm::ActiveTurn {
                    *arm = RecoveryKnownIdArm::AbsorbedActiveTurn;
                }
                known.insert(absorbed);
            }
        }
        ledger.settled_ids()
    }
}
