//! Apply the body policy to persisted work from writers that posted raw tool results.

use std::collections::BTreeSet;

use super::deliver::{ChannelWriter, Step};
use super::pieces::TOOL_RESULT_INTERNAL;
use super::{AlarmSink, DeliveryLease, DiscordPort};
use crate::services::tui_o::shadow::{UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::LedgerEntry;

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    /// Outcomes remain intact: an uncertain Prepared is still settled from Discord history.
    pub(super) fn exclude_stored_tool_results(&mut self) {
        let ledger = self.store().ledger();
        if ledger.violation().is_some() {
            return;
        }
        let keys: BTreeSet<_> = (0..ledger.next_serial())
            .filter_map(|serial| ledger.piece(serial))
            .filter(|piece| piece.unit_key.kind == UnitKind::ToolResult)
            .map(|piece| piece.unit_key.clone())
            .collect();
        for key in keys {
            if self.exclude_tool_result(&key) == Some(Step::Stopped) {
                break;
            }
        }
    }

    /// Also guards saved/operator/shared-receipt payloads that bypass native derivation.
    pub(super) fn exclude_tool_result(&mut self, key: &UnitKey) -> Option<Step> {
        if key.kind != UnitKind::ToolResult {
            return None;
        }
        if self.store().ledger().excluded(key).is_some() {
            return Some(Step::Done);
        }
        let entry = LedgerEntry::Excluded {
            unit_key: key.clone(),
            reason: TOOL_RESULT_INTERNAL.into(),
        };
        Some(self.record(entry).map_or_else(|step| step, |()| Step::Done))
    }
}
