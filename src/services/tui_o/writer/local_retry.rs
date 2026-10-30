//! A single retry derived entirely from the existing ledger, including its saved payload and time.

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::deliver::{ChannelWriter, Step};
use super::pieces::PieceWork;
use super::{AlarmSink, DeliveryLease, DiscordPort};
use crate::services::tui_o::store::ledger::{LedgerState, LocalRetryMark, PieceOutcome};

mod notice_selection;

pub(crate) const LOCAL_RETRY_DELAY: Duration = Duration::from_secs(30);

pub(crate) struct Pending {
    pub root_serial: u64,
    pub piece: PieceWork,
    pub due: DateTime<Utc>,
}

pub(crate) fn pending(ledger: &LedgerState) -> Option<Pending> {
    (0..ledger.next_serial()).find_map(|serial| {
        let root = ledger.piece(serial)?;
        if ledger.excluded(&root.unit_key).is_some()
            || root.local_retry != Some(LocalRetryMark::Budget)
            || root.outcome != Some(PieceOutcome::NotFound)
            || ledger.latest_piece(&root.unit_key, root.piece_index)?.0 != serial
        {
            return None;
        }
        let due = root
            .outcome_at?
            .checked_add_signed(chrono::Duration::from_std(LOCAL_RETRY_DELAY).ok()?)?;
        Some(Pending {
            root_serial: serial,
            piece: PieceWork {
                unit_key: root.unit_key.clone(),
                index: root.piece_index,
                payload: root.payload.clone(),
                notice_origin: root.notice_origin.clone(),
            },
            due,
        })
    })
}

/// Re-read under the ownership gate, after taking the lease, before consuming any budget.
pub(super) fn may_prepare(
    ledger: &LedgerState,
    piece: &PieceWork,
    mark: Option<&LocalRetryMark>,
) -> bool {
    match (mark, notice_selection::for_piece(ledger, piece, mark)) {
        (Some(LocalRetryMark::Attempt { root_serial }), Some(root)) => {
            *root_serial == root.root_serial && root.piece == *piece
        }
        (Some(LocalRetryMark::Attempt { .. }), None) | (_, Some(_)) => false,
        (_, None) => true,
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    pub(crate) fn has_local_retry(&mut self) -> bool {
        notice_selection::candidates(self.store().ledger())
            .any(|root| !notice_selection::is_notice(&root.piece))
    }

    pub(crate) async fn retry_local(&mut self, now: DateTime<Utc>) -> Step {
        if self.is_stopped() {
            return Step::Stopped;
        }
        // Unmarked restores keep their existing actor settlement path.
        let settle = self
            .store()
            .ledger()
            .unresolved()
            .is_some_and(|(_, record)| record.local_retry.is_some());
        if settle && let Err(step) = self.settle_open().await {
            return step;
        }
        let candidates: Vec<_> = notice_selection::candidates(self.store().ledger())
            .map(|root| root.root_serial)
            .collect();
        for serial in candidates {
            let Some(root) = notice_selection::at(self.store().ledger(), serial) else {
                continue;
            };
            let notice = notice_selection::is_notice(&root.piece);
            if let Some(step) = self.exclude_tool_result(&root.piece.unit_key) {
                return step;
            }
            if now < root.due {
                if notice {
                    continue;
                }
                return Step::LeaseBusy;
            }
            let mark = LocalRetryMark::Attempt {
                root_serial: root.root_serial,
            };
            let step = self.send_piece(&root.piece, None, Some(mark), None).await;
            if notice && matches!(step, Step::Withheld | Step::LeaseBusy) {
                continue;
            }
            return step;
        }
        Step::Done
    }
}
