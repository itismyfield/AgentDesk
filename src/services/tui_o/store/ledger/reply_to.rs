//! The reply anchor recorded for a prepared serial, read back by retries of the same piece.

use super::LedgerState;

impl LedgerState {
    #[cfg(any(test, feature = "test-support"))]
    pub fn reply_to_for_test(&self, serial: u64) -> Option<u64> {
        self.reply_to(serial)
    }

    pub(crate) fn reply_to(&self, serial: u64) -> Option<u64> {
        self.reply_tos.get(&serial).copied()
    }

    /// As an output origin: only for the next serial, before its `Prepared`, never replaced.
    pub(super) fn replay_reply_to(&mut self, serial: u64, anchor: u64) {
        let replaced = self
            .reply_tos
            .get(&serial)
            .is_some_and(|old| *old != anchor);
        if serial != self.next_serial || self.open_serial.is_some() || replaced {
            self.violate("reply anchor must precede its own prepared serial".into());
        } else {
            self.reply_tos.insert(serial, anchor);
        }
    }
}
