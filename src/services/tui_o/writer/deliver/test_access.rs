//! Test-only reads of the writer's private state, for tests outside this crate layer.

use std::collections::HashMap;

use super::super::suspension::Suspension;
use super::super::{AlarmSink, DeliveryLease, DiscordPort};
use super::{ChannelWriter, Step};
use crate::services::tui_input::human_frame::history::notice_owner::Batch;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceRecord};

impl<P, L, A> ChannelWriter<P, L, A> {
    pub fn notice_owners_for_test(&self) -> &Batch {
        &self.notice_owners
    }

    pub fn notice_pending_for_test(&self) -> &HashMap<(SourceId, u64, u64), String> {
        &self.notice_pending
    }

    pub fn boot_done_for_test(&self) -> u64 {
        self.boot.done()
    }

    pub fn suspension_for_test(&self) -> Suspension {
        self.suspension.clone()
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    pub fn resume_pieces_for_test(&self) -> Vec<(u64, PieceRecord)> {
        self.resume_pieces()
    }

    pub fn record_for_test(&mut self, entry: LedgerEntry) -> Result<(), Step> {
        self.record(entry)
    }

    /// One re-post turn, as the actor runs it after a poll.
    pub async fn repost_turn_for_test(&mut self) {
        self.repost_turn().await;
    }
}
