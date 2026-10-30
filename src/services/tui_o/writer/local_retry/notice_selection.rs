//! Selects independent notice retries without putting their debt in the Body barrier.

use super::{LOCAL_RETRY_DELAY, Pending};
use crate::services::tui_o::shadow::UnitKind;
use crate::services::tui_o::store::ledger::{LedgerState, LocalRetryMark, PieceOutcome};
use crate::services::tui_o::writer::pieces::PieceWork;

pub(super) fn is_notice(piece: &PieceWork) -> bool {
    piece.unit_key.kind == UnitKind::InputNotice
}

pub(super) fn at(ledger: &LedgerState, serial: u64) -> Option<Pending> {
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
}

pub(super) fn candidates(ledger: &LedgerState) -> impl Iterator<Item = Pending> + '_ {
    let first = super::pending(ledger);
    let from = first.as_ref().map_or(ledger.next_serial(), |root| {
        root.root_serial.saturating_add(1)
    });
    first
        .into_iter()
        .chain((from..ledger.next_serial()).filter_map(move |serial| at(ledger, serial)))
}

pub(super) fn for_piece(
    ledger: &LedgerState,
    piece: &PieceWork,
    mark: Option<&LocalRetryMark>,
) -> Option<Pending> {
    if !is_notice(piece) {
        return candidates(ledger).find(|root| !is_notice(&root.piece));
    }
    let serial = match mark {
        Some(LocalRetryMark::Attempt { root_serial }) => *root_serial,
        _ => ledger.latest_piece(&piece.unit_key, piece.index)?.0,
    };
    at(ledger, serial)
}
